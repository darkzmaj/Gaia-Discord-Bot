import {
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  ChannelType,
  EmbedBuilder,
  MessageFlags,
  ModalBuilder,
  OverwriteType,
  StringSelectMenuBuilder,
  TextInputBuilder,
  TextInputStyle,
  UserSelectMenuBuilder,
  type ButtonInteraction,
  type Guild,
  type GuildMember,
  type ModalSubmitInteraction,
  type StringSelectMenuInteraction,
  type UserSelectMenuInteraction,
  type VoiceBasedChannel,
  type VoiceState,
} from "discord.js";
import type {
  ActiveRoom,
  CookieRepository,
  VoicePreference,
} from "../db/repositories.js";
import { KeyedLock } from "./locks.js";
import { logDiscordError, logger } from "../logger.js";

const brand = 0xd9a066;
const compactMs = (started: number) =>
  `${Math.round(performance.now() - started)}ms`;
type CachedAccess = { allowed: string[]; denied: string[] };

export const roomAccessPlan = (
  everyoneId: string,
  ownerId: string,
  preference: Pick<VoicePreference, "is_locked">,
  access: CachedAccess,
  verificationRoleId: string | null,
  verificationRoleExists: boolean,
) => {
  const restricted = !!(verificationRoleId && verificationRoleExists);
  return [
    {
      id: everyoneId,
      allow: [] as string[],
      deny: preference.is_locked || restricted ? ["Connect"] : ([] as string[]),
    },
    { id: ownerId, allow: ["Connect"], deny: [] as string[] },
    ...(restricted && !preference.is_locked
      ? [{ id: verificationRoleId!, allow: ["Connect"], deny: [] as string[] }]
      : []),
    ...[...new Set([...access.allowed, ...access.denied])].map((id) => ({
      id,
      allow: access.denied.includes(id) ? [] : ["Connect"],
      deny: access.denied.includes(id) ? ["Connect"] : [],
    })),
  ];
};

export class VoiceRoomService {
  private readonly locks = new KeyedLock();
  private readonly prefs = new Map<string, VoicePreference>();
  private readonly accesses = new Map<string, CachedAccess>();
  private readonly configs = new Map<
    string,
    { enabled: boolean; joinChannelId: string | null }
  >();
  private readonly renameCooldowns = new Map<string, number>();
  constructor(private readonly repo: CookieRepository) {}
  private key(guildId: string, ownerId: string) {
    return `${guildId}:${ownerId}`;
  }
  private clearOwnerCache(guildId: string, ownerId: string) {
    const key = this.key(guildId, ownerId);
    this.prefs.delete(key);
    this.accesses.delete(key);
  }
  forgetGuild(guildId: string): void {
    this.configs.delete(guildId);
    for (const key of this.prefs.keys())
      if (key.startsWith(`${guildId}:`)) this.prefs.delete(key);
    for (const key of this.accesses.keys())
      if (key.startsWith(`${guildId}:`)) this.accesses.delete(key);
    for (const key of this.renameCooldowns.keys())
      if (key.startsWith(`${guildId}:`)) this.renameCooldowns.delete(key);
  }

  async onVoiceState(oldState: VoiceState, state: VoiceState): Promise<void> {
    if (!state.member) return;
    const guild = state.guild,
      oldChannel = oldState.channel,
      newChannel = state.channel;
    if (oldChannel?.id !== newChannel?.id && oldChannel)
      void this.reconcileRoom(guild, oldChannel);
    if (
      !newChannel ||
      oldChannel?.id === newChannel.id ||
      state.member.user.bot
    )
      return;
    const config = await this.config(guild.id);
    if (!config.enabled || newChannel.id !== config.joinChannelId) return;
    await this.locks.run(`${guild.id}:${state.member.id}`, async () => {
      if (
        state.member?.voice.channelId !== config.joinChannelId ||
        (await this.repo.activeRoomForOwner(guild.id, state.member.id))
      )
        return;
      await this.createRoom(guild, state.member!, newChannel);
    });
  }
  async refreshGuildConfig(guildId: string): Promise<void> {
    this.configs.delete(guildId);
    await this.config(guildId);
  }
  async createRoom(
    guild: Guild,
    owner: GuildMember,
    source: VoiceBasedChannel,
  ): Promise<void> {
    await this.locks.run(`${guild.id}:room-create`, () =>
      this.createRoomLocked(guild, owner, source),
    );
  }
  private async createRoomLocked(
    guild: Guild,
    owner: GuildMember,
    source: VoiceBasedChannel,
  ): Promise<void> {
    const started = performance.now();
    const [preference, access, verification] = await Promise.all([
      this.preference(guild.id, owner.id),
      this.access(guild.id, owner.id),
      this.repo.verification(guild.id),
    ]);
    const reservationMs = compactMs(started);
    const room = await guild.channels
      .create({
        name: preference.custom_name ?? this.availableName(guild),
        type: ChannelType.GuildVoice,
        parent: source.parentId ?? undefined,
        userLimit: preference.user_limit,
        permissionOverwrites: this.overwrites(
          guild,
          owner.id,
          preference,
          access,
          verification.enabled ? verification.role_id : null,
        ) as any,
        reason: `Cookie room for ${owner.user.tag}`,
      })
      .catch(() => null);
    if (!room)
      return void logger.error("room creation failed", {
        guildId: guild.id,
        userId: owner.id,
      });
    const creationMs = compactMs(started);
    if (
      !(await this.repo.createRoom({
        guild_id: guild.id,
        channel_id: room.id,
        owner_id: owner.id,
        control_channel_id: room.id,
      }))
    ) {
      await room
        .delete("Duplicate Cookie room prevented")
        .catch(() => undefined);
      return;
    }
    if (owner.voice.channelId !== source.id) {
      await this.repo.deleteRoom(guild.id, room.id);
      await room.delete("Owner left join channel").catch(() => undefined);
      return;
    }
    try {
      await owner.voice.setChannel(room, "Cookie join-to-create");
    } catch {
      await this.repo.deleteRoom(guild.id, room.id);
      await room.delete("Cookie could not move owner").catch(() => undefined);
      return;
    }
    const moveMs = compactMs(started);
    logger.info("voice room critical path", {
      guildId: guild.id,
      userId: owner.id,
      reservation: reservationMs,
      channelCreation: creationMs,
      memberMove: moveMs,
      total: moveMs,
    });
    void this.postCreate(guild, room, owner.id, started);
  }
  private async postCreate(
    guild: Guild,
    room: VoiceBasedChannel,
    ownerId: string,
    started: number,
  ): Promise<void> {
    try {
      const message = await room.send({
        content: `<@${ownerId}>`,
        allowedMentions: { users: [ownerId] },
        embeds: [await this.panelEmbed(guild, ownerId)],
        components: this.controls(room.id),
      });
      await this.repo.setRoomControlMessage(guild.id, room.id, message.id);
      logger.info("voice room post-create setup", {
        guildId: guild.id,
        channelId: room.id,
        duration: compactMs(started),
      });
    } catch {
      logger.warn("voice room panel setup failed", {
        guildId: guild.id,
        channelId: room.id,
      });
    }
  }
  async reconcileRoom(guild: Guild, channel: VoiceBasedChannel): Promise<void> {
    const existing = await this.repo.activeRoom(guild.id, channel.id);
    if (!existing) return;
    await this.locks.run(`${guild.id}:channel:${channel.id}`, async () => {
      const room = await this.repo.activeRoom(guild.id, channel.id);
      const live = guild.channels.cache.get(channel.id);
      if (!room || !live?.isVoiceBased()) {
        if (room) await this.repo.deleteRoom(guild.id, channel.id);
        return;
      }
      const humans = live.members.filter((m) => !m.user.bot);
      if (!humans.size) {
        await this.repo.deleteRoom(guild.id, live.id);
        await live
          .delete("Cookie temporary room is empty")
          .catch(() => undefined);
        return;
      }
      if (!live.members.has(room.owner_id)) {
        const next = [...humans.values()][
          Math.floor(Math.random() * humans.size)
        ];
        if (
          !next ||
          !(await this.repo.transferRoom(
            guild.id,
            live.id,
            room.owner_id,
            next.id,
          ))
        )
          return;
        await this.applyPreferences(guild, live, next.id);
        await this.refreshPanel(guild, live.id);
        await next
          .send({
            embeds: [
              new EmbedBuilder()
                .setColor(brand)
                .setTitle("Cookie room ownership transferred")
                .setDescription(
                  `You now own **${live.name}** in **${guild.name}**. Your saved room preferences were applied.`,
                ),
            ],
          })
          .catch(() => undefined);
        logger.info("voice ownership transferred", {
          guildId: guild.id,
          channelId: live.id,
          userId: next.id,
        });
      }
    });
  }
  async cleanupGuild(guild: Guild): Promise<void> {
    for (const room of await this.repo.activeRooms(guild.id)) {
      const channel = guild.channels.cache.get(room.channel_id);
      await this.repo.deleteRoom(guild.id, room.channel_id);
      this.clearOwnerCache(guild.id, room.owner_id);
      if (channel?.isVoiceBased())
        await channel
          .delete("Cookie Voice Rooms disabled")
          .catch(() => undefined);
    }
  }
  async recover(guild: Guild): Promise<void> {
    await this.refreshGuildConfig(guild.id);
    for (const room of await this.repo.activeRooms(guild.id)) {
      const channel = guild.channels.cache.get(room.channel_id);
      if (!channel?.isVoiceBased())
        await this.repo.deleteRoom(guild.id, room.channel_id);
      else await this.reconcileRoom(guild, channel);
    }
  }
  async reconcileGuildAccess(
    guild: Guild,
    previousVerificationRoleId?: string | null,
  ): Promise<void> {
    for (const room of await this.repo.activeRooms(guild.id)) {
      const channel = guild.channels.cache.get(room.channel_id);
      if (!channel?.isVoiceBased()) {
        await this.repo.deleteRoom(guild.id, room.channel_id);
        continue;
      }
      await this.locks.run(`${guild.id}:channel:${channel.id}`, () =>
        this.applyPreferences(
          guild,
          channel,
          room.owner_id,
          previousVerificationRoleId,
        ),
      );
    }
  }
  async handleButton(i: ButtonInteraction): Promise<void> {
    const [, action, channelId] = i.customId.split(":");
    if (
      !i.guild ||
      !channelId ||
      !(await this.authorizedRoom(i.guild, channelId, i.user.id))
    )
      return void i.reply({
        content: "You are not the current owner of this room.",
        flags: MessageFlags.Ephemeral,
      });
    if (action === "toggle") {
      const p = await this.preference(i.guild.id, i.user.id);
      await this.setPreference(
        i.guild.id,
        i.user.id,
        "is_locked",
        p.is_locked ? 0 : 1,
      );
      await this.applyAndRefresh(i.guild, channelId, i.user.id);
      return void i.reply({
        content: p.is_locked ? "Room unlocked." : "Room locked.",
        flags: MessageFlags.Ephemeral,
      });
    }
    if (action === "limit") return void i.showModal(this.limitModal(channelId));
    if (action === "rename")
      return void i.showModal(this.renameModal(channelId));
    if (action === "allow" || action === "deny")
      return void i.reply({
        content: `Select a member to ${action}.`,
        components: [
          new ActionRowBuilder<UserSelectMenuBuilder>().addComponents(
            new UserSelectMenuBuilder()
              .setCustomId(`roompick:${action}:${channelId}`)
              .setPlaceholder(`Select a member to ${action}`)
              .setMinValues(1)
              .setMaxValues(1),
          ),
        ],
        flags: MessageFlags.Ephemeral,
      });
    if (action === "manage") return void this.showAccess(i, channelId);
    if (action === "reset")
      return void i.reply({
        embeds: [
          new EmbedBuilder()
            .setColor(0xed4245)
            .setTitle("Reset Voice Room Settings?")
            .setDescription(
              "This resets your saved settings for this server: user limit, lock state, allowed users, and denied users. Your current room will update too.",
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder()
              .setCustomId(`roomreset:cancel:${channelId}`)
              .setLabel("Cancel")
              .setStyle(ButtonStyle.Secondary),
            new ButtonBuilder()
              .setCustomId(`roomreset:confirm:${channelId}`)
              .setLabel("Reset Settings")
              .setStyle(ButtonStyle.Danger),
          ),
        ],
        flags: MessageFlags.Ephemeral,
      });
  }
  async handleModal(i: ModalSubmitInteraction): Promise<void> {
    const [, action, channelId] = i.customId.split(":");
    if (
      !i.guild ||
      !channelId ||
      !(await this.authorizedRoom(i.guild, channelId, i.user.id))
    )
      return void i.reply({
        content: "You are not the current owner of this room.",
        flags: MessageFlags.Ephemeral,
      });
    if (action === "rename") {
      const name = i.fields.getTextInputValue("name").trim();
      if (!name || name.length > 20)
        return void i.reply({
          content: "Room names must be between 1 and 20 characters.",
          flags: MessageFlags.Ephemeral,
        });
      const key = `${i.guild.id}:${channelId}`,
        last = this.renameCooldowns.get(key) ?? 0;
      if (Date.now() - last < 5_000)
        return void i.reply({
          content: "Please wait a few seconds before renaming again.",
          flags: MessageFlags.Ephemeral,
        });
      const channel = i.guild.channels.cache.get(channelId);
      if (!channel?.isVoiceBased())
        return void i.reply({
          content: "That room no longer exists.",
          flags: MessageFlags.Ephemeral,
        });
      await channel.setName(name, "Cookie room rename").catch(() => null);
      this.renameCooldowns.set(key, Date.now());
      await this.setPreference(i.guild.id, i.user.id, "custom_name", name);
      await this.refreshPanel(i.guild, channelId);
      return void i.reply({
        content: `Room renamed to **${name}**.`,
        flags: MessageFlags.Ephemeral,
      });
    }
    if (action !== "limit") return;
    const value = i.fields.getTextInputValue("limit").trim();
    if (!/^\d+$/.test(value) || Number(value) > 25)
      return void i.reply({
        content: "Enter a whole number from 1 to 25, or 0 for unlimited.",
        flags: MessageFlags.Ephemeral,
      });
    await this.setPreference(
      i.guild.id,
      i.user.id,
      "user_limit",
      Number(value),
    );
    await this.applyAndRefresh(i.guild, channelId, i.user.id);
    await i.reply({ content: "Room user limit updated.", flags: MessageFlags.Ephemeral });
  }
  async handleUserSelect(i: UserSelectMenuInteraction): Promise<void> {
    const [, action, channelId] = i.customId.split(":");
    if (
      !i.guild ||
      !channelId ||
      !["allow", "deny"].includes(action) ||
      !(await this.authorizedRoom(i.guild, channelId, i.user.id))
    )
      return void i.reply({
        content: "You are not the current owner of this room.",
        flags: MessageFlags.Ephemeral,
      });
    const userId = i.values[0];
    const member =
      i.guild.members.cache.get(userId) ??
      (await i.guild.members.fetch(userId).catch(() => null));
    if (!member)
      return void i.reply({
        content: "That user is no longer in this server.",
        flags: MessageFlags.Ephemeral,
      });
    await this.setAccess(
      i.guild.id,
      i.user.id,
      member.id,
      action as "allow" | "deny",
      true,
    );
    await this.applyAndRefresh(i.guild, channelId, i.user.id);
    await i.update({
      content: `<@${member.id}> was ${action === "allow" ? "allowed" : "denied"}.`,
      components: [],
    });
  }
  async handleStringSelect(i: StringSelectMenuInteraction): Promise<void> {
    const [, list, channelId] = i.customId.split(":");
    if (
      !i.guild ||
      !channelId ||
      !["allowed", "denied"].includes(list) ||
      !(await this.authorizedRoom(i.guild, channelId, i.user.id))
    )
      return void i.reply({
        content: "You are not the current owner of this room.",
        flags: MessageFlags.Ephemeral,
      });
    const userId = i.values[0];
    await this.setAccess(
      i.guild.id,
      i.user.id,
      userId,
      list === "allowed" ? "allow" : "deny",
      false,
    );
    await this.applyAndRefresh(i.guild, channelId, i.user.id);
    await this.showAccess(i, channelId, true);
  }
  async handleReset(i: ButtonInteraction): Promise<void> {
    const [, action, channelId] = i.customId.split(":");
    if (
      !i.guild ||
      !channelId ||
      !(await this.authorizedRoom(i.guild, channelId, i.user.id))
    )
      return void i.reply({
        content: "You are not the current owner of this room.",
        flags: MessageFlags.Ephemeral,
      });
    if (action === "cancel")
      return void i.update({
        content: "Reset cancelled.",
        embeds: [],
        components: [],
      });
    await this.repo.resetPreference(i.guild.id, i.user.id);
    this.prefs.delete(this.key(i.guild.id, i.user.id));
    this.accesses.delete(this.key(i.guild.id, i.user.id));
    const channel = i.guild.channels.cache.get(channelId);
    if (channel?.isVoiceBased())
      await channel
        .setName(this.availableName(i.guild), "Cookie room settings reset")
        .catch(() => undefined);
    await this.applyAndRefresh(i.guild, channelId, i.user.id);
    await i.update({
      content: "Your room settings were reset for this server.",
      embeds: [],
      components: [],
    });
  }
  private async showAccess(
    i: ButtonInteraction | StringSelectMenuInteraction,
    channelId: string,
    update = false,
  ): Promise<void> {
    if (!i.guild) return;
    const access = await this.access(i.guild.id, i.user.id);
    const format = (ids: string[]) =>
      ids.length ? ids.map((id) => `<@${id}>`).join("\n") : "None";
    const rows: ActionRowBuilder<StringSelectMenuBuilder>[] = [];
    if (access.allowed.length) {
      const menu = new StringSelectMenuBuilder()
        .setCustomId(`roomaccess:allowed:${channelId}`)
        .setPlaceholder("Remove an allowed user")
        .addOptions(
          access.allowed
            .slice(0, 25)
            .map((id) => ({
              label:
                i.guild!.members.cache.get(id)?.displayName ?? `User ${id}`,
              value: id,
            })),
        );
      rows.push(
        new ActionRowBuilder<StringSelectMenuBuilder>().addComponents(menu),
      );
    }
    if (access.denied.length) {
      const menu = new StringSelectMenuBuilder()
        .setCustomId(`roomaccess:denied:${channelId}`)
        .setPlaceholder("Remove a denied user")
        .addOptions(
          access.denied
            .slice(0, 25)
            .map((id) => ({
              label:
                i.guild!.members.cache.get(id)?.displayName ?? `User ${id}`,
              value: id,
            })),
        );
      rows.push(
        new ActionRowBuilder<StringSelectMenuBuilder>().addComponents(menu),
      );
    }
    const payload = {
      embeds: [
        new EmbedBuilder()
          .setColor(brand)
          .setTitle("Manage Room Access")
          .addFields(
            { name: "Allowed", value: format(access.allowed), inline: true },
            { name: "Denied", value: format(access.denied), inline: true },
          ),
      ],
      components: rows,
    };
    if (update && i.isStringSelectMenu()) await i.update(payload);
    else await i.reply({ ...payload, flags: MessageFlags.Ephemeral });
  }
  private limitModal(channelId: string) {
    const input = new TextInputBuilder()
      .setCustomId("limit")
      .setLabel("User limit (0–25)")
      .setStyle(TextInputStyle.Short)
      .setRequired(true)
      .setMaxLength(2);
    return new ModalBuilder()
      .setCustomId(`roommodal:limit:${channelId}`)
      .setTitle("Set Room User Limit")
      .addComponents(
        new ActionRowBuilder<TextInputBuilder>().addComponents(input),
      );
  }
  private renameModal(channelId: string) {
    return new ModalBuilder()
      .setCustomId(`roommodal:rename:${channelId}`)
      .setTitle("Room Name")
      .addComponents(
        new ActionRowBuilder<TextInputBuilder>().addComponents(
          new TextInputBuilder()
            .setCustomId("name")
            .setLabel("Room name")
            .setStyle(TextInputStyle.Short)
            .setRequired(true)
            .setMaxLength(20),
        ),
      );
  }
  private async config(guildId: string) {
    const cached = this.configs.get(guildId);
    if (cached) return cached;
    const [settings, joinChannelId] = await Promise.all([
      this.repo.settings(guildId),
      this.repo.joinChannel(guildId),
    ]);
    const value = { enabled: !!settings.voice_rooms_enabled, joinChannelId };
    this.configs.set(guildId, value);
    return value;
  }
  private async preference(guildId: string, userId: string) {
    const key = this.key(guildId, userId);
    const cached = this.prefs.get(key);
    if (cached) return cached;
    const value = await this.repo.preference(guildId, userId);
    this.prefs.set(key, value);
    return value;
  }
  private async access(guildId: string, userId: string) {
    const key = this.key(guildId, userId);
    const cached = this.accesses.get(key);
    if (cached) return cached;
    const value = await this.repo.access(guildId, userId);
    this.accesses.set(key, value);
    return value;
  }
  private async setPreference(
    guildId: string,
    userId: string,
    field: "is_locked" | "user_limit" | "custom_name",
    value: number | string | null,
  ) {
    await this.repo.updatePreference(guildId, userId, field, value);
    const p = await this.preference(guildId, userId);
    (p as any)[field] = value;
  }
  private async setAccess(
    guildId: string,
    ownerId: string,
    userId: string,
    action: "allow" | "deny",
    present: boolean,
  ) {
    const type = action === "allow" ? "allowed" : "denied",
      opposite = action === "allow" ? "denied" : "allowed";
    await this.repo.setAccess(guildId, ownerId, userId, type, present);
    if (present)
      await this.repo.setAccess(guildId, ownerId, userId, opposite, false);
    const access = await this.access(guildId, ownerId);
    access[type] = present
      ? [...new Set([...access[type], userId])]
      : access[type].filter((id) => id !== userId);
    if (present)
      access[opposite] = access[opposite].filter((id) => id !== userId);
  }
  private async applyAndRefresh(
    guild: Guild,
    channelId: string,
    ownerId: string,
  ) {
    const channel = guild.channels.cache.get(channelId);
    if (channel?.isVoiceBased()) {
      await this.applyPreferences(guild, channel, ownerId);
      await this.refreshPanel(guild, channelId);
    }
  }
  private async applyPreferences(
    guild: Guild,
    channel: VoiceBasedChannel,
    ownerId: string,
    previousVerificationRoleId?: string | null,
  ) {
    const [p, access, verification, mutes] = await Promise.all([
      this.preference(guild.id, ownerId),
      this.access(guild.id, ownerId),
      this.repo.verification(guild.id),
      this.repo.activeMutes(guild.id),
    ]);
    const muted = new Set(mutes.map((mute) => mute.user_id));
    await channel.setUserLimit(p.user_limit);
    const staleMemberOverwrites = [
      ...channel.permissionOverwrites.cache.values(),
    ].filter((o) => o.type === OverwriteType.Member && !muted.has(o.id));
    for (let index = 0; index < staleMemberOverwrites.length; index += 10)
      await Promise.all(
        staleMemberOverwrites
          .slice(index, index + 10)
          .map((overwrite) =>
            channel.permissionOverwrites
              .delete(overwrite.id, "Cookie room access sync")
              .catch(() => undefined),
          ),
      );
    const restricted = !!(
      verification.enabled &&
      verification.role_id &&
      guild.roles.cache.has(verification.role_id)
    );
    await channel.permissionOverwrites.edit(guild.roles.everyone, {
      Connect: p.is_locked || restricted ? false : null,
    });
    if (
      previousVerificationRoleId &&
      previousVerificationRoleId !== verification.role_id
    )
      await channel.permissionOverwrites
        .edit(previousVerificationRoleId, { Connect: null })
        .catch(() => undefined);
    if (verification.role_id)
      await channel.permissionOverwrites
        .edit(verification.role_id, {
          Connect: restricted && !p.is_locked ? true : null,
        })
        .catch(() => undefined);
    await channel.permissionOverwrites.edit(ownerId, { Connect: true });
    for (const id of new Set([...access.allowed, ...access.denied]))
      if (!muted.has(id))
        await channel.permissionOverwrites.edit(id, {
          Connect: access.denied.includes(id) ? false : true,
        });
  }
  private overwrites(
    guild: Guild,
    ownerId: string,
    p: VoicePreference,
    access: CachedAccess,
    verificationRoleId: string | null,
  ) {
    return roomAccessPlan(
      guild.roles.everyone.id,
      ownerId,
      p,
      access,
      verificationRoleId,
      !!verificationRoleId && guild.roles.cache.has(verificationRoleId),
    );
  }
  private async refreshPanel(guild: Guild, channelId: string) {
    const active = await this.repo.activeRoom(guild.id, channelId);
    const channel = guild.channels.cache.get(channelId);
    if (!active?.control_message_id || !channel?.isVoiceBased()) return;
    const message = await channel.messages
      .fetch(active.control_message_id)
      .catch(async (error: unknown) => {
        logDiscordError("voice-room.panel.fetch", error, {
          service: "voice-room",
          guildId: guild.id,
          channelId,
          messageId: active.control_message_id ?? undefined,
        });
        await this.repo.setRoomControlMessage(guild.id, channelId, null);
        return null;
      });
    if (message) {
      const updated = await message
        .edit({
          content: `<@${active.owner_id}>`,
          allowedMentions: { parse: [] },
          embeds: [await this.panelEmbed(guild, active.owner_id)],
          components: this.controls(channelId),
        })
        .catch(async (error: unknown) => {
          logDiscordError("voice-room.panel.edit", error, {
            service: "voice-room",
            guildId: guild.id,
            channelId,
            messageId: active.control_message_id ?? undefined,
          });
          await this.repo.setRoomControlMessage(guild.id, channelId, null);
          return null;
        });
      if (!updated) return;
    }
  }
  private async panelEmbed(guild: Guild, ownerId: string) {
    const p = await this.preference(guild.id, ownerId);
    const room = await this.repo.activeRoomForOwner(guild.id, ownerId);
    return new EmbedBuilder()
      .setColor(brand)
      .setTitle("🍪 Cookie Voice Room")
      .addFields(
        { name: "Owner", value: `<@${ownerId}>` },
        {
          name: "Room",
          value: room
            ? (guild.channels.cache.get(room.channel_id)?.name ??
              p.custom_name ??
              "Room")
            : (p.custom_name ?? "Room"),
        },
        {
          name: "Access",
          value: p.is_locked ? "🔴 Locked" : "🟢 Unlocked",
          inline: true,
        },
        {
          name: "User Limit",
          value: p.user_limit ? `🟢 ${p.user_limit} users` : "🟢 Unlimited",
          inline: true,
        },
      )
      .setFooter({
        text: "Your room settings are saved for the next room you create.",
      });
  }
  private controls(channelId: string) {
    return [
      new ActionRowBuilder<ButtonBuilder>().addComponents(
        new ButtonBuilder()
          .setCustomId(`room:toggle:${channelId}`)
          .setLabel("Lock / Unlock")
          .setStyle(ButtonStyle.Secondary),
        new ButtonBuilder()
          .setCustomId(`room:limit:${channelId}`)
          .setLabel("User Limit")
          .setStyle(ButtonStyle.Primary),
        new ButtonBuilder()
          .setCustomId(`room:rename:${channelId}`)
          .setLabel("Rename")
          .setStyle(ButtonStyle.Primary),
        new ButtonBuilder()
          .setCustomId(`room:allow:${channelId}`)
          .setLabel("Allow User")
          .setStyle(ButtonStyle.Success),
        new ButtonBuilder()
          .setCustomId(`room:deny:${channelId}`)
          .setLabel("Deny User")
          .setStyle(ButtonStyle.Danger),
      ),
      new ActionRowBuilder<ButtonBuilder>().addComponents(
        new ButtonBuilder()
          .setCustomId(`room:manage:${channelId}`)
          .setLabel("Manage Access")
          .setStyle(ButtonStyle.Secondary),
        new ButtonBuilder()
          .setCustomId(`room:reset:${channelId}`)
          .setLabel("Reset Settings")
          .setStyle(ButtonStyle.Danger),
      ),
    ];
  }
  private async authorizedRoom(
    guild: Guild,
    channelId: string,
    userId: string,
  ): Promise<ActiveRoom | undefined> {
    const room = await this.repo.activeRoom(guild.id, channelId);
    return room?.owner_id === userId ? room : undefined;
  }
  private availableName(guild: Guild) {
    const used = new Set(
      [...guild.channels.cache.values()]
        .map((c) => /^Room (\d+)$/.exec(c.name)?.[1])
        .filter(Boolean)
        .map(Number),
    );
    let n = 1;
    while (used.has(n)) n++;
    return `Room ${n}`;
  }
}
