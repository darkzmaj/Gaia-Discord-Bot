import {
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  ChannelSelectMenuBuilder,
  ChannelType,
  EmbedBuilder,
  MessageFlags,
  ModalBuilder,
  RoleSelectMenuBuilder,
  StringSelectMenuBuilder,
  TextInputBuilder,
  TextInputStyle,
  UserSelectMenuBuilder,
} from "discord.js";
import type {
  CookieRepository,
  VerificationConfig,
} from "../db/repositories.js";
import { hasPermission } from "./authorization.js";
import type { MuteService } from "./mute-service.js";
import type { VoiceRoomService } from "./voice-room-service.js";
import {
  normalizeVerificationColor,
  type VerificationService,
} from "./verification-service.js";
import {
  BackupAlreadyRunningError,
  MemberSynchronizationError,
  type BackupService,
} from "./backup-service.js";
import { logDiscordError } from "../logger.js";
import type { AuditLogService } from "./audit-log-service.js";

const color = 0xd9a066;

/** Members whose role state would actually change: holders for remove, non-holders for add. */
export function roleChangeTargets(
  members: Iterable<any>,
  mode: string,
  roleId: string,
  target: "user" | "everyone" | "bots",
): any[] {
  return [...members].filter(
    (member) =>
      (target !== "bots" || member.user.bot) &&
      member.roles.cache.has(roleId) === (mode === "remove"),
  );
}
type DeploymentSession = { guildId: string; userId: string; channelId: string };

export class AdminUi {
  private readonly deployments = new Map<string, DeploymentSession>();
  constructor(
    private readonly repo: CookieRepository,
    private readonly mutes: MuteService,
    private readonly rooms: VoiceRoomService,
    private readonly verification: VerificationService,
    private readonly backups: BackupService,
    private readonly uptime: () => string,
    private readonly audit?: AuditLogService,
  ) {}

  private auditConfiguration(i: any, title: string, details?: Record<string, string | number | boolean>): void {
    this.audit?.record({ title, category: "Admin Configuration", color: "warning", guild: { id: i.guild.id, name: i.guild.name }, actor: { id: i.user.id, name: i.user.tag }, details });
  }

  async open(i: any) {
    if (!i.guild || !(await this.admin(i))) return;
    await i.reply({
      ...(await this.dashboard(i.guild)),
      flags: MessageFlags.Ephemeral,
    });
  }

  async handle(i: any) {
    if (!i.guild || !(await this.admin(i))) return;
    const [, kind, action, guildId] = i.customId.split(":");
    if (guildId !== i.guild.id)
      return void await i.reply({
        content: "That control belongs to a different server.",
        flags: MessageFlags.Ephemeral,
      });
    if (kind === "nav") return void await i.update(await this.view(i.guild, action));
    if (kind === "verify") {
      const config = await this.repo.verification(i.guild.id);
      const result = await this.verification.setEnabled(
        i.guild,
        !config.enabled,
        this.rooms,
      );
      if (result.changed) this.auditConfiguration(i, `Verification ${!config.enabled ? "Enabled" : "Disabled"}`);
      return void await i.update({
        ...(await this.view(i.guild, "verification")),
        content: result.error,
      });
    }
    if (kind === "backup") {
      if (action === "toggle") {
        const settings = await this.repo.backupSettings(i.guild.id);
        await this.repo.updateBackupSettings(i.guild.id, {
          automatic_enabled: settings.automatic_enabled ? 0 : 1,
        });
        this.auditConfiguration(i, `Automatic Backups ${settings.automatic_enabled ? "Disabled" : "Enabled"}`);
        return void await i.update(await this.view(i.guild, "backups"));
      }
      if (action === "create")
        return void await i.update({
          content:
            "Create a configuration backup for this guild? Existing backups are retained until the new snapshot is stored successfully.",
          embeds: [],
          components: [
            new ActionRowBuilder<ButtonBuilder>().addComponents(
              new ButtonBuilder()
                .setCustomId(`adminui:backupcreate:confirm:${i.guild.id}`)
                .setLabel("Create Backup")
                .setStyle(ButtonStyle.Success),
              this.nav("backups", i.guild.id, "Cancel", ButtonStyle.Secondary),
            ),
          ],
        });
      if (action === "view")
        return void await i.update(
          await this.backupPicker(i.guild, "view", i.user.id),
        );
      if (action === "restore")
        return void await i.update(
          await this.backupPicker(i.guild, "restore", i.user.id),
        );
    }
    if (kind === "backupcreate" && action === "confirm") {
      await i.deferUpdate();
      let backup;
      try {
        backup = await this.backups.create(i.guild, i.user.id, "manual");
      } catch (error) {
        if (error instanceof BackupAlreadyRunningError)
          return void await i.editReply({
            embeds: [
              new EmbedBuilder()
                .setColor(0xfee75c)
                .setTitle("Backup Already Running")
                .setDescription(
                  "A backup of this server is currently being created.",
                ),
            ],
            components: [],
          });
        if (error instanceof MemberSynchronizationError && error.rateLimited)
          return void await i.editReply({
            embeds: [
              new EmbedBuilder()
                .setColor(0xed4245)
                .setTitle("Backup Failed")
                .setDescription(
                  "Discord temporarily rate-limited member synchronization.\nNo backup was created.\nPlease try again shortly.",
                ),
            ],
            components: [],
          });
        throw error;
      }
      return void await i.editReply({
        ...(await this.view(i.guild, "backups")),
        content: `🟢 Backup Created\n\n${backup.source_guild_name} • ${new Date(backup.created_at).toLocaleString("en-GB", { timeZone: "Europe/Ljubljana" })}\nRoles: ${backup.role_count} • Channels: ${backup.channel_count} • Members: ${backup.member_count}`,
      });
    }
    if (kind === "backupdelete" && action.startsWith("confirm-")) {
      const backupId = action.slice(8);
      return void await i.update({
        content: "Delete this backup permanently? This cannot be undone.",
        embeds: [],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder()
              .setCustomId(
                `adminui:backupdelete:delete-${backupId}:${i.guild.id}`,
              )
              .setLabel("Confirm Delete")
              .setStyle(ButtonStyle.Danger),
            this.nav("backups", i.guild.id, "Cancel", ButtonStyle.Secondary),
          ),
        ],
      });
    }
    if (kind === "backupdelete" && action.startsWith("delete-")) {
      const backupId = action.slice(7);
      const deleted = await this.backups.delete(backupId, i.user.id);
      return void await i.update({
        ...(await this.view(i.guild, "backups")),
        content: deleted
          ? "🟢 Backup deleted."
          : "That backup no longer exists.",
      });
    }
    if (kind === "backuprestore" && action.startsWith("confirm-")) {
      const backupId = action.slice(8);
      await i.deferUpdate();
      let lastUpdate = 0,
        progressAvailable = true;
      const context = {
        service: "backup-restore-ui",
        guildId: i.guild.id,
        interactionId: i.id,
        customId: i.customId,
      };
      const progress = async (
        phase: string,
        completed?: number,
        total?: number,
      ) => {
        if (!progressAvailable) return;
        const now = Date.now();
        if (now - lastUpdate < 1_500 && completed !== total) return;
        lastUpdate = now;
        try {
          await i.editReply({
            content: `Full Server Restore Running\n\n🟢 ${phase}${completed !== undefined && total !== undefined ? ` — ${completed} / ${total}` : ""}\n\nProgress is persisted in Cookie's restore operation.`,
            embeds: [],
            components: [],
          });
        } catch (error) {
          progressAvailable = false;
          logDiscordError("backup.restore.progress.editReply", error, context);
        }
        // The next phase can remove the channel holding this component's message. Do not use it after announcing cleanup.
        if (phase === "Destructive cleanup begins") progressAvailable = false;
      };
      try {
        const result = await this.backups.restore(
          backupId,
          i.guild,
          i.user.id,
          progress,
        );
        if (!progressAvailable) return;
        const partial = result.status === "failed_partial";
        try {
          await i.editReply({
            content: `${partial ? "🔴 Restore Partially Failed" : result.warnings.length ? "🟡 Restore Completed With Warnings" : "🟢 Restore Completed"}\n\nSafety backup: ${result.safetyBackupId ?? "not created"}\nRoles: ${result.roles}\nChannels: ${result.channels}\nPermission overwrites: ${result.overwrites}\nMember roles: ${result.members}${
              result.criticalFailures.length
                ? `\n\nCritical failures: ${result.criticalFailures.length}\n${result.criticalFailures
                    .slice(0, 8)
                    .map((failure) => `• ${failure}`)
                    .join("\n")}`
                : ""
            }${
              result.warnings.length
                ? `\n\nWarnings / unsupported: ${result.warnings.length}\n${result.warnings
                    .slice(0, 8)
                    .map((warning) => `• ${warning}`)
                    .join("\n")}`
                : ""
            }`,
            embeds: [],
            components: [
              new ActionRowBuilder<ButtonBuilder>().addComponents(
                this.nav("backups", i.guild.id, "← Back"),
              ),
            ],
          });
        } catch (error) {
          logDiscordError("backup.restore.final.editReply", error, context);
        }
      } catch (error) {
        if (!progressAvailable) return;
        try {
          await i.editReply({
            content: `🔴 Restore Aborted Before Destructive Changes\n\n${error instanceof Error ? error.message : "The safety backup or preflight check failed."}`,
            embeds: [],
            components: [
              new ActionRowBuilder<ButtonBuilder>().addComponents(
                this.nav("backups", i.guild.id, "← Back"),
              ),
            ],
          });
        } catch (replyError) {
          logDiscordError(
            "backup.restore.abort.editReply",
            replyError,
            context,
          );
        }
      }
      return;
    }
    if (kind === "verifyrole" && action === "remove") {
      const config = await this.repo.verification(i.guild.id);
      await this.repo.updateVerification(i.guild.id, { role_id: null });
      await this.rooms.reconcileGuildAccess(i.guild, config.role_id);
      return void await i.update({
        ...(await this.view(i.guild, "verification")),
        content:
          "🟡 Verification role removed. Verification will not restrict room access until a valid role is configured.",
      });
    }
    if (kind === "verifymessage" && action === "edit")
      return void await i.showModal(await this.messageModal(i.guild));
    if (kind === "verifycolor" && action === "show")
      return void await i.update(this.colorPicker(i.guild.id));
    if (kind === "verifycolor" && action === "custom")
      return void await i.showModal(this.colorModal(i.guild.id));
    if (kind === "verifypreview") {
      const config = await this.repo.verification(i.guild.id);
      return void await i.reply({
        content: "Preview — this button is disabled and cannot verify anyone.",
        ...this.verification.panel(config, true),
        flags: MessageFlags.Ephemeral,
      });
    }
    if (kind === "verifysend" && action === "choose")
      return void await i.update(this.channelPicker(i.guild.id));
    if (kind === "verifydeploy") return await this.deploy(i, action);
    if (kind === "verifyupdate" && action === "edit")
      return await this.updateDeployed(i);
    if (kind === "toggle") {
      const key = action as
        | "autorole_enabled"
        | "voice_rooms_enabled"
        | "moderation_enabled";
      const enabled = await this.repo.toggle(i.guild.id, key);
      this.auditConfiguration(i, `${key === "autorole_enabled" ? "AutoRole" : key === "voice_rooms_enabled" ? "Voice Rooms" : "Moderation"} ${enabled ? "Enabled" : "Disabled"}`);
      let notice: string | undefined;
      if (
        key === "autorole_enabled" &&
        enabled &&
        (await this.repo.autoroles(i.guild.id)).user_role_id &&
        (await this.verification.disableForUserAutorole(i.guild, this.rooms))
      )
        notice =
          "🟡 Verification Disabled\n\nVerification was automatically disabled because User AutoRole is enabled. User AutoRole and Verification cannot be enabled at the same time.";
      if (key === "voice_rooms_enabled" && !enabled)
        await this.rooms.cleanupGuild(i.guild);
      await this.rooms.refreshGuildConfig(i.guild.id);
      return void await i.update({
        ...(await this.view(i.guild, this.sectionFor(key))),
        content: notice,
      });
    }
    if (kind === "remove") {
      if (action === "user" || action === "bot")
        await this.repo.setAutorole(
          i.guild.id,
          action === "user" ? "user_role_id" : "bot_role_id",
          null,
        );
      if (action === "user" || action === "bot") this.auditConfiguration(i, `${action === "user" ? "User" : "Bot"} AutoRole Changed`, { After: "None" });
      if (action === "join") {
        await this.repo.setJoinChannel(i.guild.id, null);
        await this.rooms.refreshGuildConfig(i.guild.id);
        this.auditConfiguration(i, "Join Channel Changed", { After: "None" });
      }
      if (action === "ticket")
        await this.mutes.configureTicketCategory(i.guild, null);
      if (action === "ticket") this.auditConfiguration(i, "Ticket Exception Changed", { After: "None" });
      return void await i.update(
        await this.view(
          i.guild,
          action === "join"
            ? "voice"
            : action === "ticket"
              ? "moderation"
              : "autorole",
        ),
      );
    }
    if (kind === "pick")
      return void await i.update(await this.picker(i.guild, action));
    if (kind === "rolemgmt" && (action === "add" || action === "remove"))
      return void await i.update({
        content: `Select the role to ${action}.`,
        embeds: [],
        components: [
          new ActionRowBuilder<RoleSelectMenuBuilder>().addComponents(
            new RoleSelectMenuBuilder()
              .setCustomId(`adminui:rmrole:${action}:${i.guild.id}`)
              .setPlaceholder("Select a role"),
          ),
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            this.nav("roles", i.guild.id, "← Back"),
          ),
        ],
      });
    if (kind === "rmtarget" || kind === "rmapply") {
      const [mode, roleId, target] = action.split("-");
      const role = await this.managedRole(i, mode, roleId);
      if (!role) return;
      const verb = mode === "add" ? "Add" : "Remove",
        preposition = mode === "add" ? "to" : "from";
      if (kind === "rmtarget" && target === "user")
        return void await i.update({
          content: `Select the member to ${mode} <@&${role.id}> ${preposition}.`,
          embeds: [],
          components: [
            new ActionRowBuilder<UserSelectMenuBuilder>().addComponents(
              new UserSelectMenuBuilder()
                .setCustomId(`adminui:rmuser:${mode}-${role.id}:${i.guild.id}`)
                .setPlaceholder("Select a member"),
            ),
            new ActionRowBuilder<ButtonBuilder>().addComponents(
              this.nav("roles", i.guild.id, "Cancel", ButtonStyle.Secondary),
            ),
          ],
        });
      if (target !== "everyone" && target !== "bots") return;
      const group = target === "bots" ? "bot" : "member";
      if (kind === "rmtarget") {
        const count = roleChangeTargets(
          await this.allMembers(i.guild),
          mode,
          role.id,
          target,
        ).length;
        return void await i.update({
          content: count
            ? `${verb} <@&${role.id}> ${preposition} **${count}** ${group}${count === 1 ? "" : "s"}? This cannot be undone automatically.`
            : `No ${group}s need this change.`,
          embeds: [],
          components: [
            new ActionRowBuilder<ButtonBuilder>().addComponents(
              new ButtonBuilder()
                .setCustomId(`adminui:rmapply:${action}:${i.guild.id}`)
                .setLabel(`Confirm ${verb}`)
                .setStyle(mode === "add" ? ButtonStyle.Success : ButtonStyle.Danger)
                .setDisabled(!count),
              this.nav("roles", i.guild.id, "Cancel", ButtonStyle.Secondary),
            ),
          ],
        });
      }
      await i.deferUpdate();
      // Clearing the components first stops a second click from starting a parallel run.
      await i.editReply({ content: `🟡 ${verb === "Add" ? "Adding" : "Removing"} <@&${role.id}>…`, embeds: [], components: [] });
      const members = roleChangeTargets(
        await this.allMembers(i.guild),
        mode,
        role.id,
        target,
      );
      const result = await this.applyRole(i, role, mode, members, target === "bots" ? "All bots" : "Everyone");
      // shortcut: one final edit; very large servers can outlive the 15-minute interaction token, the audit record still lands.
      await i
        .editReply({
          content: this.roleResult(verb, role.id, preposition, group, result),
          components: [
            new ActionRowBuilder<ButtonBuilder>().addComponents(
              this.nav("roles", i.guild.id, "← Back"),
            ),
          ],
        })
        .catch((error: unknown) =>
          logDiscordError("roles.bulk.editReply", error, {
            service: "role-management",
            guildId: i.guild.id,
            interactionId: i.id,
            customId: i.customId,
          }),
        );
    }
  }

  async handleSelect(i: any) {
    if (!i.guild || !(await this.admin(i))) return;
    const [, kind, action, guildId] = i.customId.split(":");
    if (guildId !== i.guild.id)
      return void await i.reply({
        content: "That control belongs to a different server.",
        flags: MessageFlags.Ephemeral,
      });
    const id = i.values[0];
    if (kind === "rmrole") {
      const role = await this.managedRole(i, action, id);
      if (!role) return;
      const target = (key: string, label: string) =>
        new ButtonBuilder()
          .setCustomId(`adminui:rmtarget:${action}-${role.id}-${key}:${i.guild.id}`)
          .setLabel(label)
          .setStyle(ButtonStyle.Primary);
      return void await i.update({
        content: `${action === "add" ? "Add" : "Remove"} <@&${role.id}> ${action === "add" ? "to" : "from"} whom?`,
        embeds: [],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            target("user", "Specific User"),
            target("everyone", "Everyone"),
            target("bots", "All Bots"),
            this.nav("roles", i.guild.id, "Cancel", ButtonStyle.Secondary),
          ),
        ],
      });
    }
    if (kind === "rmuser") {
      const [mode, roleId] = action.split("-");
      const role = await this.managedRole(i, mode, roleId);
      if (!role) return;
      const member = await i.guild.members.fetch(id).catch(() => null);
      if (!member)
        return void await i.reply({
          content: "That user is not a member of this server.",
          flags: MessageFlags.Ephemeral,
        });
      const verb = mode === "add" ? "Add" : "Remove",
        preposition = mode === "add" ? "to" : "from";
      const back = new ActionRowBuilder<ButtonBuilder>().addComponents(
        this.nav("roles", i.guild.id, "← Back"),
      );
      if (!roleChangeTargets([member], mode, role.id, "user").length)
        return void await i.update({
          content: `🟡 <@${member.id}> ${mode === "add" ? "already has" : "does not have"} <@&${role.id}>.`,
          embeds: [],
          components: [back],
        });
      const result = await this.applyRole(i, role, mode, [member], `<@${member.id}>`);
      return void await i.update({
        content: this.roleResult(verb, role.id, preposition, "member", result),
        embeds: [],
        components: [back],
      });
    }
    if (kind === "backupselect") {
      try {
        const preview = await this.backups.preview(id, i.guild, i.user.id);
        if (action === "view")
          return void await i.update({
            content: "",
            embeds: [
              new EmbedBuilder()
                .setColor(color)
                .setTitle("Guild Backup")
                .setDescription(
                  `**${preview.backup.source_guild_name}** • ${new Date(preview.backup.created_at).toLocaleString("en-GB", { timeZone: "Europe/Ljubljana" })}\n${preview.backup.creation_type}\n\n**Identity**\nName: Captured\nDescription: ${"description" in preview.snapshot.guild ? "Captured" : "Legacy backup"}\nIcon: ${preview.snapshot.guild.assets?.icon?.state === "captured" ? "Captured" : preview.snapshot.guild.assets?.icon?.state === "absent" ? "None" : "Legacy backup"}\nCommunity: ${preview.snapshot.guild.communityEnabled ? "Enabled" : "Disabled"}\n\n**Structure**\nRoles: ${preview.backup.role_count}\nChannels: ${preview.backup.channel_count}\nMember assignments: ${preview.backup.member_count}\nPermission overwrites: ${preview.backup.overwrite_count}\nStorage: ${(preview.backup.snapshot_size / 1024 / 1024).toFixed(2)} MB\nSchema: v${preview.backup.snapshot_schema_version}${preview.unsupported.length ? `\nUnsupported: ${preview.unsupported.length}` : ""}`,
                ),
            ],
            components: [
              new ActionRowBuilder<ButtonBuilder>().addComponents(
                new ButtonBuilder()
                  .setCustomId(
                    `adminui:backupdelete:confirm-${preview.backup.backup_id}:${i.guild.id}`,
                  )
                  .setLabel("Delete Backup")
                  .setStyle(ButtonStyle.Danger),
                this.nav("backups", i.guild.id, "← Back"),
              ),
            ],
          });
        return void await i.update({
          content: "",
          embeds: [
            new EmbedBuilder()
              .setColor(0xed4245)
              .setTitle("Full Server Restore")
              .setDescription(
                `⚠️ **Destructive replacement**\n\nSource: **${preview.backup.source_guild_name}**\nDestination: **${i.guild.name}**\n\nCookie will delete destination channels and non-managed roles it can manage, then reconstruct the backup. @everyone, managed/integration roles, Cookie's bot role, Discord-protected objects, and active temporary Cookie rooms are preserved. Existing member role assignments are replaced where Cookie can manage them.\n\nRoles to recreate: ${preview.rolesToCreate}\nChannels to recreate: ${preview.channelsToCreate}\nPermission overwrites: ${preview.overwrites}\nMember assignments to evaluate: ${preview.memberAssignments}${preview.unsupported.length ? `\nUnsupported in snapshot: ${preview.unsupported.join(" ")}` : ""}\n\nA fresh destination safety backup is captured, committed, hashed and revalidated before any deletion. Partial Discord failures cannot be rolled back automatically.`,
              ),
          ],
          components: [
            new ActionRowBuilder<ButtonBuilder>().addComponents(
              new ButtonBuilder()
                .setCustomId(
                  `adminui:backuprestore:confirm-${preview.backup.backup_id}:${i.guild.id}`,
                )
                .setLabel("I Understand — Start Full Restore")
                .setStyle(ButtonStyle.Danger),
              this.nav("backups", i.guild.id, "Cancel", ButtonStyle.Secondary),
            ),
          ],
        });
      } catch (error) {
        return void await i.reply({
          content:
            error instanceof Error ? error.message : "Backup is unavailable.",
          flags: MessageFlags.Ephemeral,
        });
      }
    }
    if (kind === "role") {
      const role = i.guild.roles.cache.get(id);
      if (!role || !this.verification.assignable(i.guild, role))
        return void await i.reply({
          content:
            "Cookie cannot use that role. Check Manage Roles, role hierarchy, and that it is not an integration role.",
          flags: MessageFlags.Ephemeral,
        });
      if (action === "verification") {
        const old = await this.repo.verification(i.guild.id);
        await this.repo.updateVerification(i.guild.id, { role_id: id });
        await this.rooms.reconcileGuildAccess(i.guild, old.role_id);
        this.auditConfiguration(i, "Verification Role Changed", { Before: old.role_id ?? "None", After: id });
      } else {
        await this.repo.setAutorole(
          i.guild.id,
          action === "user" ? "user_role_id" : "bot_role_id",
          id,
        );
        this.auditConfiguration(i, `${action === "user" ? "User" : "Bot"} AutoRole Changed`, { After: id });
        if (
          action === "user" &&
          (await this.repo.settings(i.guild.id)).autorole_enabled &&
          (await this.verification.disableForUserAutorole(i.guild, this.rooms))
        )
          return void await i.update({
            ...(await this.view(i.guild, "autorole")),
            content:
              "🟡 Verification Disabled\n\nVerification was automatically disabled because User AutoRole is enabled. User AutoRole and Verification cannot be enabled at the same time.",
          });
      }
    }
    if (kind === "color") {
      if (id === "custom") return void await i.showModal(this.colorModal(i.guild.id));
      const parsed = normalizeVerificationColor(id);
      if (parsed === null)
        return void await i.reply({
          content: "That color is invalid.",
          flags: MessageFlags.Ephemeral,
        });
      await this.repo.updateVerification(i.guild.id, { color: parsed });
    }
    if (kind === "verifychannel") {
      const channel = await i.guild.channels.fetch(id).catch(() => null);
      const config = await this.repo.verification(i.guild.id);
      const error = this.verification.configurationError(
        i.guild,
        config,
        channel,
      );
      if (error)
        return void await i.reply({
          content: `🔴 Cannot deploy verification message: ${error}`,
          flags: MessageFlags.Ephemeral,
        });
      const token = `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
      this.deployments.set(token, {
        guildId: i.guild.id,
        userId: i.user.id,
        channelId: id,
      });
      return void await i.update({
        content: `Ready to send the verification message to <#${id}>. Review the preview, then confirm.`,
        ...this.verification.panel(config, true),
        components: [
          ...this.verification.panel(config, true).components,
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder()
              .setCustomId(`adminui:verifydeploy:${token}:${i.guild.id}`)
              .setLabel("Send")
              .setStyle(ButtonStyle.Success),
            this.nav(
              "verification",
              i.guild.id,
              "Cancel",
              ButtonStyle.Secondary,
            ),
          ),
        ],
      });
    }
    if (kind === "staff") {
      if (action === "add") await this.repo.addStaff(i.guild.id, id, i.user.id);
      else await this.repo.removeStaff(i.guild.id, id);
      this.auditConfiguration(i, action === "add" ? "Staff Added" : "Staff Removed", { Member: id });
    }
    if (kind === "channel") {
      if (action === "join") {
        await this.repo.setJoinChannel(i.guild.id, id);
        await this.rooms.refreshGuildConfig(i.guild.id);
        this.auditConfiguration(i, "Join Channel Changed", { After: id });
      } else { await this.mutes.configureTicketCategory(i.guild, id); this.auditConfiguration(i, "Ticket Exception Changed", { After: id }); }
    }
    await i.update(
      await this.view(
        i.guild,
        action === "join"
          ? "voice"
          : action === "ticket"
            ? "moderation"
            : action === "verification"
              ? "verification"
              : action === "user" || action === "bot"
                ? "autorole"
                : action === "set"
                  ? "verification"
                  : "staff",
      ),
    );
  }

  async handleModal(i: any) {
    if (!i.guild || !(await this.admin(i))) return;
    const [, kind, action, guildId] = i.customId.split(":");
    if (kind !== "modal" || guildId !== i.guild.id) return;
    if (action === "message") {
      const title = i.fields.getTextInputValue("title").trim(),
        description = i.fields.getTextInputValue("description").trim(),
        button = i.fields.getTextInputValue("button").trim();
      if (
        !title ||
        title.length > 256 ||
        !description ||
        description.length > 4096 ||
        !button ||
        button.length > 80
      )
        return void await i.reply({
          content:
            "Use a title up to 256 characters, description up to 4096 characters, and button text up to 80 characters.",
          flags: MessageFlags.Ephemeral,
        });
      await this.repo.updateVerification(i.guild.id, {
        title,
        description,
        button_text: button,
      });
      return void await i.update({
        content: "🟢 Verification message configuration saved.",
        ...(await this.view(i.guild, "verification")),
      });
    }
    if (action === "color") {
      const parsed = normalizeVerificationColor(
        i.fields.getTextInputValue("color"),
      );
      if (parsed === null)
        return void await i.reply({
          content:
            "Enter a six-digit hexadecimal color, for example `#57F287`.",
          flags: MessageFlags.Ephemeral,
        });
      await this.repo.updateVerification(i.guild.id, { color: parsed });
      return void await i.update({
        content: "🟢 Verification color saved.",
        ...(await this.view(i.guild, "verification")),
      });
    }
  }

  private async deploy(i: any, token: string) {
    const session = this.deployments.get(token);
    if (
      !session ||
      session.guildId !== i.guild.id ||
      session.userId !== i.user.id
    )
      return void await i.reply({
        content:
          "That deployment confirmation expired. Start again from Verification.",
        flags: MessageFlags.Ephemeral,
      });
    const channel = await i.guild.channels
      .fetch(session.channelId)
      .catch(() => null);
    const config = await this.repo.verification(i.guild.id);
    const error = this.verification.configurationError(
      i.guild,
      config,
      channel,
    );
    if (error || !channel || !channel.isTextBased() || !("send" in channel))
      return void await i.reply({
        content: `🔴 Cannot deploy verification message: ${error ?? "Choose a text channel."}`,
        flags: MessageFlags.Ephemeral,
      });
    const message = await (channel as any)
      .send(this.verification.panel(config))
      .catch(() => null);
    if (!message)
      return void await i.reply({
        content:
          "Cookie could not send the verification message. Check its channel permissions.",
        flags: MessageFlags.Ephemeral,
      });
    await this.repo.updateVerification(i.guild.id, {
      channel_id: channel.id,
      message_id: message.id,
    });
    this.auditConfiguration(i, "Verification Message Deployed", { Channel: channel.id, Message: message.id });
    this.deployments.delete(token);
    await i.update({
      ...(await this.view(i.guild, "verification")),
      content: `🟢 Verification message deployed in <#${channel.id}>.`,
    });
  }

  private async updateDeployed(i: any) {
    const config = await this.repo.verification(i.guild.id);
    const tracked = await this.deployedMessage(i.guild, config);
    if (!tracked)
      return void await i.update({
        ...(await this.view(i.guild, "verification")),
        content:
          "🟡 The tracked verification message no longer exists. Its deployment record was cleared.",
      });
    const error = this.verification.configurationError(
      i.guild,
      config,
      tracked.channel,
    );
    if (error)
      return void await i.reply({
        content: `🔴 Cannot update verification message: ${error}`,
        flags: MessageFlags.Ephemeral,
      });
    const updated = await tracked.message
      .edit(this.verification.panel(config))
      .catch((error: unknown) => {
        logDiscordError("verification.deployment.edit", error, {
          service: "verification",
          guildId: i.guild.id,
          channelId: tracked.channel.id,
          messageId: tracked.message.id,
          interactionId: i.id,
          customId: i.customId,
        });
        return null;
      });
    if (!updated) {
      await this.repo.updateVerification(i.guild.id, {
        channel_id: null,
        message_id: null,
      });
      return void await i.update({
        ...(await this.view(i.guild, "verification")),
        content:
          "🟡 The tracked verification message no longer exists. Its deployment record was cleared.",
      });
    }
    await i.update({
      ...(await this.view(i.guild, "verification")),
      content: "🟢 Verification message updated.",
    });
  }

  private async deployedMessage(
    guild: any,
    config: VerificationConfig,
  ): Promise<{ channel: any; message: any } | null> {
    if (!config.channel_id || !config.message_id) return null;
    const channel = await guild.channels
      .fetch(config.channel_id)
      .catch((error: unknown) => {
        logDiscordError("verification.deployment.channel.fetch", error, {
          service: "verification",
          guildId: guild.id,
          channelId: config.channel_id!,
          messageId: config.message_id!,
        });
        return null;
      });
    const message =
      channel?.isTextBased() && "messages" in channel
        ? await channel.messages
            .fetch(config.message_id)
            .catch((error: unknown) => {
              logDiscordError("verification.deployment.message.fetch", error, {
                service: "verification",
                guildId: guild.id,
                channelId: config.channel_id!,
                messageId: config.message_id!,
              });
              return null;
            })
        : null;
    if (!message || message.author?.id !== guild.client.user?.id) {
      await this.repo.updateVerification(guild.id, {
        channel_id: null,
        message_id: null,
      });
      return null;
    }
    return { channel, message };
  }

  private async deploymentText(
    guild: any,
    config: VerificationConfig,
  ): Promise<string> {
    return (await this.deployedMessage(guild, config))
      ? `🟢 Deployed in <#${config.channel_id}>`
      : "🟡 Not deployed";
  }
  private async admin(i: any) {
    const member = await i.guild.members.fetch(i.user.id).catch(() => null);
    if (!member || !(await hasPermission(this.repo, member, "administrator"))) {
      await i.reply({
        content:
          "You need Discord Administrator permission to use Cookie Administration.",
        flags: MessageFlags.Ephemeral,
      });
      return false;
    }
    return true;
  }

  private async dashboard(guild: any) {
    const [s, roles, join, staff, verification] = await Promise.all([
      this.repo.settings(guild.id),
      this.repo.autoroles(guild.id),
      this.repo.joinChannel(guild.id),
      this.repo.staffCount(guild.id),
      this.repo.verification(guild.id),
    ]);
    const icon = (v: boolean) => (v ? "🟢" : "🔴");
    const configured = (id: string | null, name: string) =>
      id ? `🟢 ${name} — <@&${id}>` : `🟡 ${name} — Not configured`;
    const conflict = !!s.autorole_enabled && !!roles.user_role_id;
    const verifyStatus = conflict
      ? "🔴 Verification — Disabled (User AutoRole active)"
      : !verification.enabled
        ? "⚪ Verification — Disabled"
        : verification.role_id
          ? "🟢 Verification — configured"
          : "🟡 Verification — Missing role";
    return {
      embeds: [
        new EmbedBuilder()
          .setColor(color)
          .setTitle("🍪 Cookie Administration")
          .setDescription(
            `Managing **${guild.name}**\n\n🟢 **Cookie**\nOnline • Uptime: ${this.uptime()}\n\n**Modules**\n${icon(!!s.autorole_enabled)} AutoRole\n${icon(!!s.voice_rooms_enabled)} Voice Rooms\n${icon(!!s.moderation_enabled)} Moderation\n${verifyStatus}\n🟢 Backups\n\n**Configuration**\n${configured(roles.user_role_id, "User AutoRole")}\n${configured(roles.bot_role_id, "Bot AutoRole")}\n${join ? `🟢 Voice Rooms — <#${join}>` : "🟡 Voice Rooms — Not configured"}\n${s.ticket_category_id ? `🟢 Ticket Category — <#${s.ticket_category_id}>` : "🟡 Ticket Category — Not configured"}\n\n**Staff**\n🟢 ${staff} configured (Administrators always count)`,
          ),
      ],
      components: [
        new ActionRowBuilder<ButtonBuilder>().addComponents(
          this.nav("autorole", guild.id, "AutoRole"),
          this.nav("voice", guild.id, "Voice Rooms"),
          this.nav("staff", guild.id, "Staff"),
          this.nav("moderation", guild.id, "Moderation"),
          this.nav("verification", guild.id, "Verification"),
        ),
        new ActionRowBuilder<ButtonBuilder>().addComponents(
          this.nav("backups", guild.id, "Backups"),
          this.nav("roles", guild.id, "Remove/Add Roles"),
          this.nav("dashboard", guild.id, "Refresh", ButtonStyle.Secondary),
        ),
      ],
    };
  }

  private async view(guild: any, section: string): Promise<any> {
    if (section === "dashboard") return this.dashboard(guild);
    if (section === "roles")
      return {
        content: "",
        embeds: [
          new EmbedBuilder()
            .setColor(color)
            .setTitle("Remove/Add Roles")
            .setDescription(
              "Add or remove a role for a specific member, everyone, or all bots in this server.\n\nCookie can only manage roles below its own highest role, and you can only manage roles below yours.",
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder()
              .setCustomId(`adminui:rolemgmt:add:${guild.id}`)
              .setLabel("Add Role")
              .setStyle(ButtonStyle.Success),
            new ButtonBuilder()
              .setCustomId(`adminui:rolemgmt:remove:${guild.id}`)
              .setLabel("Remove Role")
              .setStyle(ButtonStyle.Danger),
            this.nav("dashboard", guild.id, "← Back"),
          ),
        ],
      };
    const s = await this.repo.settings(guild.id);
    if (section === "autorole") {
      const r = await this.repo.autoroles(guild.id);
      const note =
        s.autorole_enabled && r.user_role_id
          ? "\n\n🟡 User AutoRole is active. Verification cannot be enabled while it assigns a role to human members."
          : "";
      return {
        embeds: [
          new EmbedBuilder()
            .setColor(color)
            .setTitle("AutoRole")
            .setDescription(
              `Status: ${s.autorole_enabled ? "🟢 Enabled" : "🔴 Disabled"}\n\nUser AutoRole: ${r.user_role_id ? `<@&${r.user_role_id}>` : "🟡 Not configured"}\nBot AutoRole: ${r.bot_role_id ? `<@&${r.bot_role_id}>` : "🟡 Not configured"}${note}`,
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            this.toggle("autorole_enabled", guild.id, !!s.autorole_enabled),
            this.pick("user", guild.id, "Set User Role"),
            this.pick("bot", guild.id, "Set Bot Role"),
          ),
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            this.remove("user", guild.id, "Remove User Role", !r.user_role_id),
            this.remove("bot", guild.id, "Remove Bot Role", !r.bot_role_id),
            this.nav("dashboard", guild.id, "← Back"),
          ),
        ],
      };
    }
    if (section === "voice") {
      const [join, v] = await Promise.all([
        this.repo.joinChannel(guild.id),
        this.repo.verification(guild.id),
      ]);
      const access = v.enabled
        ? v.role_id && guild.roles.cache.has(v.role_id)
          ? `<@&${v.role_id}>\nControlled by Verification`
          : "🟡 Verification role missing"
        : "Everyone\nVerification disabled";
      return {
        embeds: [
          new EmbedBuilder()
            .setColor(color)
            .setTitle("Voice Rooms")
            .setDescription(
              `Status: ${s.voice_rooms_enabled ? "🟢 Enabled" : "🔴 Disabled"}\n\nJoin-to-Create: ${join ? `<#${join}>` : "🟡 Not configured"}\n\nDefault Access\n${access}`,
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            this.toggle(
              "voice_rooms_enabled",
              guild.id,
              !!s.voice_rooms_enabled,
            ),
            this.pick("join", guild.id, "Set Join Channel"),
            this.remove("join", guild.id, "Remove Join Channel", !join),
            this.nav("dashboard", guild.id, "← Back"),
          ),
        ],
      };
    }
    if (section === "moderation") {
      const count = (await this.repo.activeMutes(guild.id)).filter(
        (mute) => new Date(mute.expires_at) > new Date(),
      ).length;
      return {
        embeds: [
          new EmbedBuilder()
            .setColor(color)
            .setTitle("Moderation")
            .setDescription(
              `Status: ${s.moderation_enabled ? "🟢 Enabled" : "🔴 Disabled"}\n\nTicket Exception: ${s.ticket_category_id ? `<#${s.ticket_category_id}>` : "🟡 Not configured"}\n\nActive Mutes: ${count} member${count === 1 ? "" : "s"}`,
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            this.toggle("moderation_enabled", guild.id, !!s.moderation_enabled),
            this.pick("ticket", guild.id, "Set Ticket Category"),
            this.pick("mutes", guild.id, "View Mutes", !count),
            this.remove(
              "ticket",
              guild.id,
              "Remove Ticket Category",
              !s.ticket_category_id,
            ),
            this.nav("dashboard", guild.id, "← Back"),
          ),
        ],
      };
    }
    if (section === "backups") {
      const [settings, backups] = await Promise.all([
        this.repo.backupSettings(guild.id),
        this.repo.backups(guild.id),
      ]);
      const latest = backups[0];
      return {
        embeds: [
          new EmbedBuilder()
            .setColor(color)
            .setTitle("Guild Backups")
            .setDescription(
              `Automatic Backups: ${settings.automatic_enabled ? "🟢 Enabled — daily at 03:00 Europe/Ljubljana" : "⚪ Disabled"}\n\nStored Backups: ${backups.length} / 2\nLatest Backup: ${latest ? `${latest.source_guild_name} • ${new Date(latest.created_at).toLocaleString("en-GB", { timeZone: "Europe/Ljubljana" })}` : "None"}\nStorage: ${(backups.reduce((total, backup) => total + backup.snapshot_size, 0) / 1024 / 1024).toFixed(2)} MB\n\nConfiguration snapshots include roles, supported channels, overwrites, member role assignments, and Cookie configuration. They do not include messages, DMs, integrations, managed roles, or a byte-for-byte server image.`,
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder()
              .setCustomId(`adminui:backup:toggle:${guild.id}`)
              .setLabel(
                settings.automatic_enabled
                  ? "Disable Automatic"
                  : "Enable Automatic",
              )
              .setStyle(
                settings.automatic_enabled
                  ? ButtonStyle.Danger
                  : ButtonStyle.Success,
              ),
            new ButtonBuilder()
              .setCustomId(`adminui:backup:create:${guild.id}`)
              .setLabel("Create Backup")
              .setStyle(ButtonStyle.Primary),
            new ButtonBuilder()
              .setCustomId(`adminui:backup:view:${guild.id}`)
              .setLabel("View Backups")
              .setStyle(ButtonStyle.Secondary)
              .setDisabled(!backups.length),
            new ButtonBuilder()
              .setCustomId(`adminui:backup:restore:${guild.id}`)
              .setLabel("Restore Backup")
              .setStyle(ButtonStyle.Danger),
            this.nav("dashboard", guild.id, "← Back"),
          ),
        ],
      };
    }
    if (section === "verification") {
      const v = await this.repo.verification(guild.id);
      const [ar, deployment] = await Promise.all([
        this.repo.autoroles(guild.id),
        this.deploymentText(guild, v),
      ]);
      const conflict = s.autorole_enabled && ar.user_role_id;
      const roleState =
        v.role_id && guild.roles.cache.has(v.role_id)
          ? `<@&${v.role_id}>`
          : "🟡 Not configured or missing";
      return {
        embeds: [
          new EmbedBuilder()
            .setColor(v.color)
            .setTitle("Verification")
            .setDescription(
              `Status: ${v.enabled ? "🟢 Enabled" : "⚪ Disabled"}${conflict ? "\n🔴 Unavailable while User AutoRole is enabled." : ""}\n\nVerified Role: ${roleState}\nVerification Message: ${deployment}\n\nTitle: ${v.title}\nButton: ${v.button_text}`,
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder()
              .setCustomId(`adminui:verify:toggle:${guild.id}`)
              .setLabel(v.enabled ? "Disable" : "Enable")
              .setStyle(v.enabled ? ButtonStyle.Danger : ButtonStyle.Success)
              .setDisabled(!!conflict && !v.enabled),
            this.pick("verification", guild.id, "Set Role"),
            new ButtonBuilder()
              .setCustomId(`adminui:verifymessage:edit:${guild.id}`)
              .setLabel("Edit Message")
              .setStyle(ButtonStyle.Primary),
            new ButtonBuilder()
              .setCustomId(`adminui:verifycolor:show:${guild.id}`)
              .setLabel("Set Color")
              .setStyle(ButtonStyle.Secondary),
            new ButtonBuilder()
              .setCustomId(`adminui:verifypreview:show:${guild.id}`)
              .setLabel("Preview")
              .setStyle(ButtonStyle.Secondary),
          ),
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder()
              .setCustomId(`adminui:verifysend:choose:${guild.id}`)
              .setLabel("Send Verification")
              .setStyle(ButtonStyle.Success),
            new ButtonBuilder()
              .setCustomId(`adminui:verifyupdate:edit:${guild.id}`)
              .setLabel("Update Message")
              .setStyle(ButtonStyle.Primary)
              .setDisabled(!v.channel_id || !v.message_id),
            new ButtonBuilder()
              .setCustomId(`adminui:verifyrole:remove:${guild.id}`)
              .setLabel("Remove Role")
              .setStyle(ButtonStyle.Secondary)
              .setDisabled(!v.role_id),
            this.nav("dashboard", guild.id, "← Back"),
          ),
        ],
      };
    }
    const names = await this.repo.staffMembers(guild.id);
    return {
      embeds: [
        new EmbedBuilder()
          .setColor(color)
          .setTitle("Cookie Staff")
          .setDescription(
            `Cookie Staff can mute and unmute members. Discord Administrators automatically have Staff access.\n\n**Configured Staff**\n${names.length ? names.map((id) => `• <@${id}>`).join("\n") : "None"}`,
          ),
      ],
      components: [
        new ActionRowBuilder<ButtonBuilder>().addComponents(
          this.pick("staffadd", guild.id, "Add Staff"),
          this.pick("staffremove", guild.id, "Remove Staff", !names.length),
          this.nav("dashboard", guild.id, "← Back"),
        ),
      ],
    };
  }

  private async backupPicker(
    guild: any,
    action: "view" | "restore",
    userId: string,
  ): Promise<any> {
    const backups = await this.backups.availableTo(userId);
    if (!backups.length)
      return {
        content: "No backups you are authorized to access are available.",
        embeds: [],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            this.nav("backups", guild.id, "← Back"),
          ),
        ],
      };
    return {
      content:
        action === "restore"
          ? "Select a backup for a destructive full-server restore."
          : "Select a backup to inspect.",
      embeds: [],
      components: [
        new ActionRowBuilder<StringSelectMenuBuilder>().addComponents(
          new StringSelectMenuBuilder()
            .setCustomId(`adminui:backupselect:${action}:${guild.id}`)
            .setPlaceholder("Select a backup")
            .addOptions(
              backups.slice(0, 25).map((backup) => ({
                label:
                  `${backup.source_guild_name} — ${new Date(backup.created_at).toLocaleDateString("en-GB")}`.slice(
                    0,
                    100,
                  ),
                description:
                  `${backup.creation_type} • ${backup.channel_count} channels • ${backup.role_count} roles`.slice(
                    0,
                    100,
                  ),
                value: backup.backup_id,
              })),
            ),
        ),
        new ActionRowBuilder<ButtonBuilder>().addComponents(
          this.nav("backups", guild.id, "← Back"),
        ),
      ],
    };
  }
  private async picker(guild: any, action: string): Promise<any> {
    if (action === "mutes") {
      const mutes = (await this.repo.activeMutes(guild.id))
        .filter((mute) => new Date(mute.expires_at) > new Date())
        .slice(0, 10);
      return {
        content: "",
        embeds: [
          new EmbedBuilder()
            .setColor(color)
            .setTitle("Active Mutes • Page 1")
            .setDescription(
              mutes
                .map(
                  (mute) =>
                    `🔇 <@${mute.user_id}>\nExpires: <t:${Math.floor(new Date(mute.expires_at).getTime() / 1000)}:R>\nReason: ${mute.reason ?? "No reason provided."}`,
                )
                .join("\n\n") || "No active mutes.",
            ),
        ],
        components: [
          new ActionRowBuilder<ButtonBuilder>().addComponents(
            this.nav("moderation", guild.id, "← Back"),
          ),
        ],
      };
    }
    if (action === "user" || action === "bot" || action === "verification")
      return {
        content: `Select the ${action === "verification" ? "verification" : action} role.`,
        embeds: [],
        components: [
          new ActionRowBuilder<RoleSelectMenuBuilder>().addComponents(
            new RoleSelectMenuBuilder()
              .setCustomId(`adminui:role:${action}:${guild.id}`)
              .setPlaceholder("Select a role"),
          ),
        ],
      };
    if (action === "join")
      return {
        content: "Select the join-to-create voice channel.",
        embeds: [],
        components: [
          new ActionRowBuilder<ChannelSelectMenuBuilder>().addComponents(
            new ChannelSelectMenuBuilder()
              .setCustomId(`adminui:channel:join:${guild.id}`)
              .setChannelTypes(ChannelType.GuildVoice)
              .setPlaceholder("Select a voice channel"),
          ),
        ],
      };
    if (action === "ticket")
      return {
        content: "Select the ticket category.",
        embeds: [],
        components: [
          new ActionRowBuilder<ChannelSelectMenuBuilder>().addComponents(
            new ChannelSelectMenuBuilder()
              .setCustomId(`adminui:channel:ticket:${guild.id}`)
              .setChannelTypes(ChannelType.GuildCategory)
              .setPlaceholder("Select a category"),
          ),
        ],
      };
    if (action === "staffadd")
      return {
        content: "Select a member to add to Cookie Staff.",
        embeds: [],
        components: [
          new ActionRowBuilder<UserSelectMenuBuilder>().addComponents(
            new UserSelectMenuBuilder()
              .setCustomId(`adminui:staff:add:${guild.id}`)
              .setPlaceholder("Select a member"),
          ),
        ],
      };
    const staff = await this.repo.staffMembers(guild.id);
    return {
      content: "Select a configured staff member to remove.",
      embeds: [],
      components: [
        new ActionRowBuilder<StringSelectMenuBuilder>().addComponents(
          new StringSelectMenuBuilder()
            .setCustomId(`adminui:staff:remove:${guild.id}`)
            .setPlaceholder("Select staff")
            .addOptions(
              staff.slice(0, 25).map((id) => ({
                label: guild.members.cache.get(id)?.displayName ?? `User ${id}`,
                value: id,
              })),
            ),
        ),
      ],
    };
  }
  private async messageModal(guild: any) {
    const v = await this.repo.verification(guild.id);
    return new ModalBuilder()
      .setCustomId(`adminui:modal:message:${guild.id}`)
      .setTitle("Edit Verification Message")
      .addComponents(
        new ActionRowBuilder<TextInputBuilder>().addComponents(
          new TextInputBuilder()
            .setCustomId("title")
            .setLabel("Embed Title")
            .setStyle(TextInputStyle.Short)
            .setValue(v.title)
            .setRequired(true)
            .setMaxLength(256),
        ),
        new ActionRowBuilder<TextInputBuilder>().addComponents(
          new TextInputBuilder()
            .setCustomId("description")
            .setLabel("Embed Description")
            .setStyle(TextInputStyle.Paragraph)
            .setValue(v.description)
            .setRequired(true)
            .setMaxLength(4000),
        ),
        new ActionRowBuilder<TextInputBuilder>().addComponents(
          new TextInputBuilder()
            .setCustomId("button")
            .setLabel("Verification Button Text")
            .setStyle(TextInputStyle.Short)
            .setValue(v.button_text)
            .setRequired(true)
            .setMaxLength(80),
        ),
      );
  }
  private colorPicker(guildId: string) {
    return {
      content: "Choose a verification embed color.",
      embeds: [],
      components: [
        new ActionRowBuilder<StringSelectMenuBuilder>().addComponents(
          new StringSelectMenuBuilder()
            .setCustomId(`adminui:color:set:${guildId}`)
            .setPlaceholder("Select a color")
            .addOptions(
              { label: "Green", value: "57F287" },
              { label: "Blue", value: "5865F2" },
              { label: "Red", value: "ED4245" },
              { label: "Yellow", value: "FEE75C" },
              { label: "Purple", value: "9B59B6" },
              { label: "Custom…", value: "custom" },
            ),
        ),
        new ActionRowBuilder<ButtonBuilder>().addComponents(
          this.nav("verification", guildId, "← Back"),
        ),
      ],
    };
  }
  private colorModal(guildId: string) {
    return new ModalBuilder()
      .setCustomId(`adminui:modal:color:${guildId}`)
      .setTitle("Custom Verification Color")
      .addComponents(
        new ActionRowBuilder<TextInputBuilder>().addComponents(
          new TextInputBuilder()
            .setCustomId("color")
            .setLabel("Hex color (for example #57F287)")
            .setStyle(TextInputStyle.Short)
            .setRequired(true)
            .setMaxLength(7),
        ),
      );
  }
  private channelPicker(guildId: string) {
    return {
      content: "Select a text channel for the verification message.",
      embeds: [],
      components: [
        new ActionRowBuilder<ChannelSelectMenuBuilder>().addComponents(
          new ChannelSelectMenuBuilder()
            .setCustomId(`adminui:verifychannel:choose:${guildId}`)
            .setChannelTypes(
              ChannelType.GuildText,
              ChannelType.GuildAnnouncement,
            )
            .setPlaceholder("Select a text channel"),
        ),
        new ActionRowBuilder<ButtonBuilder>().addComponents(
          this.nav("verification", guildId, "← Back"),
        ),
      ],
    };
  }
  private nav(
    section: string,
    guildId: string,
    label: string,
    style = ButtonStyle.Primary,
  ) {
    return new ButtonBuilder()
      .setCustomId(`adminui:nav:${section}:${guildId}`)
      .setLabel(label)
      .setStyle(style);
  }
  private toggle(key: string, guildId: string, enabled: boolean) {
    return new ButtonBuilder()
      .setCustomId(`adminui:toggle:${key}:${guildId}`)
      .setLabel(enabled ? "Disable" : "Enable")
      .setStyle(enabled ? ButtonStyle.Danger : ButtonStyle.Success);
  }
  private pick(
    action: string,
    guildId: string,
    label: string,
    disabled = false,
  ) {
    return new ButtonBuilder()
      .setCustomId(`adminui:pick:${action}:${guildId}`)
      .setLabel(label)
      .setStyle(ButtonStyle.Primary)
      .setDisabled(disabled);
  }
  private remove(
    action: string,
    guildId: string,
    label: string,
    disabled = false,
  ) {
    return new ButtonBuilder()
      .setCustomId(`adminui:remove:${action}:${guildId}`)
      .setLabel(label)
      .setStyle(ButtonStyle.Secondary)
      .setDisabled(disabled);
  }
  /** Rechecked on every step: the role, Cookie's hierarchy and the actor's hierarchy can change between clicks. */
  private async managedRole(i: any, mode: string, roleId: string) {
    const fail = async (content: string) => {
      await i.reply({ content, flags: MessageFlags.Ephemeral });
      return null;
    };
    if (mode !== "add" && mode !== "remove") return fail("Unknown role action.");
    const role = i.guild.roles.cache.get(roleId);
    if (!role || role.id === i.guild.id)
      return fail("That role no longer exists or cannot be assigned.");
    if (!this.verification.assignable(i.guild, role))
      return fail(
        "Cookie cannot use that role. Check Manage Roles, role hierarchy, and that it is not an integration role.",
      );
    const actor = await i.guild.members.fetch(i.user.id).catch(() => null);
    if (
      i.guild.ownerId !== i.user.id &&
      !(actor && actor.roles.highest.comparePositionTo(role) > 0)
    )
      return fail("You can only manage roles below your own highest role.");
    return role;
  }

  /** Discord allows one full member request (opcode 8) per guild about every 30s; a rate limit means a fetch just
   * filled the cache, which the GuildMembers intent keeps current, so the cache is the same dataset. */
  private async allMembers(guild: any) {
    try {
      return (await guild.members.fetch()).values();
    } catch (error: any) {
      if (
        !String(error?.name ?? "").includes("GatewayRateLimit") &&
        !String(error?.message ?? "").includes("opcode 8")
      )
        throw error;
      return guild.members.cache.values();
    }
  }

  private async applyRole(i: any, role: any, mode: string, members: any[], target: string) {
    let changed = 0;
    const failed: string[] = [];
    const reason = `Cookie role management by ${i.user.tag ?? i.user.id}`;
    for (const member of members)
      await (mode === "add"
        ? member.roles.add(role, reason)
        : member.roles.remove(role, reason)
      ).then(
        () => changed++,
        (error: unknown) => {
          failed.push(member.id);
          logDiscordError(`roles.${mode}`, error, {
            service: "role-management",
            guildId: i.guild.id,
            userId: member.id,
          });
        },
      );
    this.audit?.record({
      title: mode === "add" ? "Role Added" : "Role Removed",
      category: "Role Management",
      color: failed.length ? "danger" : "warning",
      guild: { id: i.guild.id, name: i.guild.name },
      actor: { id: i.user.id, name: i.user.tag },
      details: { Role: role.name, Target: target, Changed: changed, Failed: failed.length },
    });
    return { changed, failed };
  }

  private roleResult(
    verb: string,
    roleId: string,
    preposition: string,
    group: string,
    result: { changed: number; failed: string[] },
  ) {
    const past = verb === "Add" ? "Added" : "Removed";
    return `${result.failed.length ? "🟡" : "🟢"} ${past} <@&${roleId}> ${preposition} ${result.changed} ${group}${result.changed === 1 ? "" : "s"}.${result.failed.length ? `\n🔴 Failed for ${result.failed.length}: ${result.failed.slice(0, 10).map((id) => `<@${id}>`).join(" ")}` : ""}`;
  }

  private sectionFor(key: string) {
    return key === "autorole_enabled"
      ? "autorole"
      : key === "voice_rooms_enabled"
        ? "voice"
        : "moderation";
  }
}
