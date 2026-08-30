import { ApplicationIntegrationType, InteractionContextType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { ADMIN_LABEL, STAFF_LABEL } from './services/authorization.js';

const guildCommand = (command: any) => command.setIntegrationTypes(ApplicationIntegrationType.GuildInstall).setContexts(InteractionContextType.Guild);

export const commands = [
  guildCommand(new SlashCommandBuilder().setName('admin').setDescription(`${ADMIN_LABEL} Open Cookie Administration.`).setDefaultMemberPermissions(PermissionFlagsBits.Administrator)),
  guildCommand(new SlashCommandBuilder().setName('embed').setDescription(`${ADMIN_LABEL} Open Cookie Embed Manager.`).setDefaultMemberPermissions(PermissionFlagsBits.Administrator)),
  guildCommand(new SlashCommandBuilder().setName('mute').setDescription(`${STAFF_LABEL} Temporarily mute a member.`).setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers).addUserOption((o) => o.setName('user').setDescription('Member.').setRequired(true)).addStringOption((o) => o.setName('duration').setDescription('Example: 1d 4h 30m.').setRequired(true)).addStringOption((o) => o.setName('reason').setDescription('Optional reason.').setRequired(false))),
  guildCommand(new SlashCommandBuilder().setName('unmute').setDescription(`${STAFF_LABEL} Unmute a member.`).setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers).addUserOption((o) => o.setName('user').setDescription('Member.').setRequired(true)).addStringOption((o) => o.setName('reason').setDescription('Optional reason.').setRequired(false))),
  guildCommand(new SlashCommandBuilder().setName('unverify').setDescription(`${ADMIN_LABEL} Unverify a member.`).setDefaultMemberPermissions(PermissionFlagsBits.Administrator).addUserOption((o) => o.setName('user').setDescription('Member.').setRequired(true))),
].map((command) => command.toJSON());
