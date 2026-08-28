import {
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  EmbedBuilder,
  MessageFlags,
  PermissionsBitField,
  type ButtonInteraction,
  type Guild,
  type GuildBasedChannel,
  type Role,
} from "discord.js";
import type {
  CookieRepository,
  VerificationConfig,
} from "../db/repositories.js";
import type { VoiceRoomService } from "./voice-room-service.js";
import { KeyedLock } from "./locks.js";
import type { AuditLogService } from "./audit-log-service.js";

export const normalizeVerificationColor = (input: string): number | null => {
  const value = input.trim().replace(/^#/, "");
  return /^[0-9a-fA-F]{6}$/.test(value) ? Number.parseInt(value, 16) : null;
};

export class VerificationService {
  private readonly locks = new KeyedLock();
  constructor(private readonly repo: CookieRepository, private readonly audit?: AuditLogService) {}

  async button(interaction: ButtonInteraction): Promise<void> {
    if (!interaction.guild) return;
    await this.locks.run(
      `${interaction.guild.id}:${interaction.user.id}`,
      async () => {
        const config = await this.repo.verification(interaction.guild!.id);
        if (!config.enabled)
          return void await interaction.reply({
            content: "🔴 Verification is currently disabled on this server.",
            flags: MessageFlags.Ephemeral,
          });
        const role = config.role_id
          ? interaction.guild!.roles.cache.get(config.role_id)
          : null;
        const member = await interaction
          .guild!.members.fetch(interaction.user.id)
          .catch(() => null);
        if (!role || !member)
          return void await interaction.reply({
            content:
              "Verification is not configured correctly. Please contact an administrator.",
            flags: MessageFlags.Ephemeral,
          });
        if (!this.assignable(interaction.guild!, role))
          return void await interaction.reply({
            content: "Cookie cannot assign the configured verification role.",
            flags: MessageFlags.Ephemeral,
          });
        if (member.roles.cache.has(role.id))
          return void await interaction.reply({
            content: "🟢 You are already verified.",
            flags: MessageFlags.Ephemeral,
          });
        await member.roles
          .add(role, "Cookie verification")
          .then(() => {
            this.audit?.record({ title: "Member Verified", category: "Verification", color: "success", guild: { id: interaction.guild!.id, name: interaction.guild!.name }, target: { id: member.id, name: member.user.tag }, details: { Role: role.name } });
            return interaction.reply({
              content: "🟢 You have been verified.",
              flags: MessageFlags.Ephemeral,
            });
          })
          .catch(() => {
            this.audit?.record({ title: "Verification Failed", category: "Verification", color: "danger", priority: "critical", guild: { id: interaction.guild!.id, name: interaction.guild!.name }, target: { id: interaction.user.id, name: interaction.user.tag }, reason: "Cookie could not assign the verification role." });
            return interaction.reply({
              content: "Cookie could not assign the verification role.",
              flags: MessageFlags.Ephemeral,
            });
          });
      },
    );
  }

  async unverify(
    guild: Guild,
    userId: string,
    executor?: { id: string; user?: { tag?: string } },
  ): Promise<{ changed: boolean; error?: string }> {
    const config = await this.repo.verification(guild.id);
    if (!config.role_id)
      return {
        changed: false,
        error: "🔴 No verification role is configured for this server.",
      };
    const role = guild.roles.cache.get(config.role_id);
    if (!role)
      return {
        changed: false,
        error: "🔴 The configured verification role no longer exists.",
      };
    if (!this.assignable(guild, role))
      return {
        changed: false,
        error: "Cookie cannot manage the configured verification role.",
      };
    return this.locks.run(`${guild.id}:${userId}`, async () => {
      const member = await guild.members.fetch(userId).catch(() => null);
      if (!member)
        return {
          changed: false,
          error: "That user is not a member of this server.",
        };
      if (!member.manageable)
        return {
          changed: false,
          error: "Cookie cannot manage that member because of role hierarchy.",
        };
      if (!member.roles.cache.has(role.id)) return { changed: false };
      try {
        await member.roles.remove(role, "Cookie unverify");
        this.audit?.record({ title: "Member Unverified", category: "Verification", color: "warning", priority: "critical", guild: { id: guild.id, name: guild.name }, target: { id: member.id, name: member.user.tag }, actor: executor ? { id: executor.id, name: executor.user?.tag } : undefined, details: { Role: role.name } });
        return { changed: true };
      } catch {
        return {
          changed: false,
          error: "Cookie could not remove the verification role.",
        };
      }
    });
  }

  assignable(guild: Guild, role: Role): boolean {
    const me = guild.members.me;
    return (
      !!me?.permissions.has(PermissionsBitField.Flags.ManageRoles) &&
      !role.managed &&
      me.roles.highest.comparePositionTo(role) > 0
    );
  }

  configurationError(
    guild: Guild,
    config: VerificationConfig,
    channel?: GuildBasedChannel | null,
  ): string | null {
    if (!config.role_id) return "Set a verification role first.";
    const role = guild.roles.cache.get(config.role_id);
    if (!role) return "The configured verification role no longer exists.";
    if (!this.assignable(guild, role))
      return "Cookie cannot assign the configured verification role.";
    if (
      !config.title.trim() ||
      config.title.length > 256 ||
      !config.description.trim() ||
      config.description.length > 4096 ||
      !config.button_text.trim() ||
      config.button_text.length > 80 ||
      !Number.isInteger(config.color) ||
      config.color < 0 ||
      config.color > 0xffffff
    )
      return "The verification message configuration is invalid.";
    if (channel) {
      const me = guild.members.me;
      if (!channel.isTextBased() || !("permissionsFor" in channel) || !me)
        return "Choose a text channel.";
      const permissions = channel.permissionsFor(me);
      if (
        !permissions?.has([
          PermissionsBitField.Flags.ViewChannel,
          PermissionsBitField.Flags.SendMessages,
          PermissionsBitField.Flags.EmbedLinks,
        ])
      )
        return "Cookie needs View Channel, Send Messages, and Embed Links in that channel.";
    }
    return null;
  }

  panel(config: VerificationConfig, disabled = false) {
    return {
      embeds: [
        new EmbedBuilder()
          .setColor(config.color)
          .setTitle(config.title)
          .setDescription(config.description),
      ],
      components: [
        new ActionRowBuilder<ButtonBuilder>().addComponents(
          new ButtonBuilder()
            .setCustomId("verify:click")
            .setLabel(config.button_text)
            .setStyle(ButtonStyle.Success)
            .setDisabled(disabled),
        ),
      ],
    };
  }

  async setEnabled(
    guild: Guild,
    enabled: boolean,
    rooms: VoiceRoomService,
  ): Promise<{ changed: boolean; error?: string }> {
    const [settings, roles, current] = await Promise.all([
      this.repo.settings(guild.id),
      this.repo.autoroles(guild.id),
      this.repo.verification(guild.id),
    ]);
    if (enabled && settings.autorole_enabled && roles.user_role_id)
      return {
        changed: false,
        error:
          "🔴 Verification Cannot Be Enabled\n\nDisable User AutoRole first. User AutoRole automatically assigns roles when members join, which conflicts with Cookie verification.",
      };
    if (current.enabled === Number(enabled)) return { changed: false };
    await this.repo.updateVerification(guild.id, { enabled: enabled ? 1 : 0 });
    this.audit?.record({ title: `Verification ${enabled ? "Enabled" : "Disabled"}`, category: "Verification", color: "warning", guild: { id: guild.id, name: guild.name } });
    await rooms.reconcileGuildAccess(guild, current.role_id);
    return { changed: true };
  }

  async disableForUserAutorole(
    guild: Guild,
    rooms: VoiceRoomService,
  ): Promise<boolean> {
    const config = await this.repo.verification(guild.id);
    if (!config.enabled) return false;
    await this.repo.updateVerification(guild.id, { enabled: 0 });
    await rooms.reconcileGuildAccess(guild, config.role_id);
    return true;
  }
}
