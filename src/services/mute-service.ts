import { ChannelType, EmbedBuilder, PermissionsBitField, type Guild, type GuildBasedChannel, type GuildMember } from 'discord.js';
import type { CookieRepository } from '../db/repositories.js';
import { MAX_MUTE_MS, humanDuration } from './duration.js';
import { logger } from '../logger.js';
import { KeyedLock } from './locks.js';

export type MuteResult = { error?: string; expiresAt?: Date; dmDelivered?: boolean };
export type UnmuteResult = { changed: boolean; dmDelivered?: boolean; error?: string };
const mutedPermissions = [PermissionsBitField.Flags.SendMessages, PermissionsBitField.Flags.SendMessagesInThreads, PermissionsBitField.Flags.CreatePublicThreads, PermissionsBitField.Flags.CreatePrivateThreads, PermissionsBitField.Flags.AddReactions, PermissionsBitField.Flags.Connect, PermissionsBitField.Flags.Speak, PermissionsBitField.Flags.UseVAD, PermissionsBitField.Flags.UseApplicationCommands, PermissionsBitField.Flags.UseEmbeddedActivities, PermissionsBitField.Flags.UseSoundboard, PermissionsBitField.Flags.UseExternalSounds];

export class MuteService {
  private timer?: NodeJS.Timeout;
  private readonly locks = new KeyedLock();
  constructor(private readonly repo: CookieRepository, private readonly guilds: () => Iterable<Guild>) {}
  start(): void { this.timer = setInterval(() => void this.expireDue().catch((error) => logger.error('mute expiry sweep failed', { error: error instanceof Error ? error.name : 'unknown' })), 30_000); void this.expireDue().catch((error) => logger.error('initial mute expiry sweep failed', { error: error instanceof Error ? error.name : 'unknown' })); void this.recoverActive().catch((error) => logger.error('mute recovery failed', { error: error instanceof Error ? error.name : 'unknown' })); }
  stop(): void { if (this.timer) clearInterval(this.timer); this.timer = undefined; }

  async mute(guild: Guild, member: GuildMember, moderator: GuildMember, durationMs: number, reason?: string): Promise<MuteResult> { return this.locks.run(`${guild.id}:${member.id}`, async () => {
    if (durationMs <= 0 || durationMs > MAX_MUTE_MS) return { error: 'Maximum mute duration is 30 days.' };
    if (member.id === moderator.id || member.id === guild.members.me?.id || member.id === guild.ownerId) return { error: 'You cannot mute that member.' };
    if (member.permissions.has(PermissionsBitField.Flags.Administrator)) return { error: '🔴 Cannot mute this member because Discord Administrator bypasses channel permission overwrites.' };
    const me = guild.members.me;
    if (!me || member.roles.highest.comparePositionTo(me.roles.highest) >= 0) return { error: 'Cookie cannot mute that member because of role hierarchy.' };
    if (!(await this.repo.settings(guild.id)).moderation_enabled) return { error: 'Moderation is disabled on this server.' };
    const start = new Date(), expiresAt = new Date(start.getTime() + durationMs);
    if (!(await this.repo.addMute({ guild_id: guild.id, user_id: member.id, moderator_id: moderator.id, reason: reason ?? null, started_at: start.toISOString(), expires_at: expiresAt.toISOString() }))) return { error: 'That member is already muted.' };
    try { await this.applyGuild(guild, member.id); } catch { await this.restore(guild, member.id).catch(() => undefined); await this.repo.removeMute(guild.id, member.id); return { error: 'Cookie could not apply mute channel restrictions. Check Manage Channels permission.' }; }
    const dmDelivered = await this.dmMuted(member, guild, durationMs, expiresAt, reason);
    logger.info('member muted', { guildId: guild.id, userId: member.id, moderatorId: moderator.id });
    return { expiresAt, dmDelivered };
  }); }

  async unmute(guild: Guild, userId: string, reason: string, automaticOrModerator?: boolean | GuildMember): Promise<UnmuteResult> { const automatic = automaticOrModerator === true; return this.locks.run(`${guild.id}:${userId}`, async () => {
    if (!(await this.repo.mute(guild.id, userId))) return { changed: false };
    try { await this.restore(guild, userId); } catch { return { changed: false, error: 'Cookie could not restore this member’s channel permissions.' }; }
    await this.repo.removeMute(guild.id, userId);
    const member = await guild.members.fetch(userId).catch(() => null);
    const dmDelivered = member ? await this.dmUnmuted(member, guild, reason, automatic) : false;
    logger.info('member unmuted', { guildId: guild.id, userId, automatic });
    return { changed: true, dmDelivered };
  }); }

  async applyMuteOnJoin(member: GuildMember): Promise<void> { if (await this.repo.mute(member.guild.id, member.id)) await this.locks.run(`${member.guild.id}:${member.id}`, () => this.applyGuild(member.guild, member.id)); }
  async configureTicketCategory(guild: Guild, categoryId: string | null): Promise<void> { await this.repo.updateSetting(guild.id, 'ticket_category_id', categoryId); for (const mute of await this.repo.activeMutes(guild.id)) await this.locks.run(`${guild.id}:${mute.user_id}`, async () => { await this.restore(guild, mute.user_id); await this.applyGuild(guild, mute.user_id); }); }
  async syncNewChannel(channel: GuildBasedChannel): Promise<void> { if (channel.type === ChannelType.GuildCategory || !('permissionOverwrites' in channel)) return; const settings = await this.repo.settings(channel.guild.id); if (channel.parentId === settings.ticket_category_id) return; for (const mute of await this.repo.activeMutes(channel.guild.id)) await this.locks.run(`${channel.guild.id}:${mute.user_id}`, () => this.applyChannel(channel as any, mute.user_id)); }
  private async expireDue(): Promise<void> { for (const mute of await this.repo.dueMutes()) { const guild = [...this.guilds()].find((g) => g.id === mute.guild_id); if (guild) await this.unmute(guild, mute.user_id, '', true); else { await this.repo.clearMuteRestores(mute.guild_id, mute.user_id); await this.repo.removeMute(mute.guild_id, mute.user_id); } } }
  private async recoverActive(): Promise<void> { const now = new Date(); for (const guild of this.guilds()) for (const mute of await this.repo.activeMutes(guild.id)) { if (new Date(mute.expires_at) <= now) await this.unmute(guild, mute.user_id, '', true); else await this.locks.run(`${guild.id}:${mute.user_id}`, () => this.applyGuild(guild, mute.user_id)).catch(() => logger.warn('mute recovery failed', { guildId: guild.id, userId: mute.user_id })); } }
  private async applyGuild(guild: Guild, userId: string): Promise<void> { const ticket = (await this.repo.settings(guild.id)).ticket_category_id; const list = [...guild.channels.cache.filter((c) => c.type !== ChannelType.GuildCategory && c.parentId !== ticket && 'permissionOverwrites' in c).values()]; for (let index = 0; index < list.length; index += 10) await Promise.all(list.slice(index, index + 10).map((channel) => this.applyChannel(channel as any, userId))); }
  private async applyChannel(channel: any, userId: string): Promise<void> { const existing = channel.permissionOverwrites.cache.get(userId); await this.repo.saveMuteRestore(channel.guild.id, userId, channel.id, !!existing, String(existing?.allow.bitfield ?? 0n), String(existing?.deny.bitfield ?? 0n)); const permissions = Object.fromEntries(mutedPermissions.map((permission) => [permission, false])); await channel.permissionOverwrites.edit(userId, permissions, { reason: 'Cookie member mute' }); }
  private async restore(guild: Guild, userId: string): Promise<void> { for (const saved of await this.repo.muteRestores(guild.id, userId)) { const channel = guild.channels.cache.get(saved.channel_id); if (!channel || !('permissionOverwrites' in channel)) continue; if (saved.existed) await (channel as any).permissionOverwrites.edit(userId, { allow: BigInt(saved.allow_bits), deny: BigInt(saved.deny_bits) }, { reason: 'Cookie mute restoration' }); else await (channel as any).permissionOverwrites.delete(userId, 'Cookie mute restoration').catch(() => undefined); } await this.repo.clearMuteRestores(guild.id, userId); }
  private async dmMuted(member: GuildMember, guild: Guild, duration: number, expires: Date, reason?: string): Promise<boolean> { try { await member.send({ embeds: [new EmbedBuilder().setColor(0xed4245).setTitle('🔇 You have been muted').addFields({ name: 'Server', value: guild.name }, { name: 'Duration', value: humanDuration(duration), inline: true }, { name: 'Unmute', value: `<t:${Math.floor(expires.getTime() / 1000)}:F>\n<t:${Math.floor(expires.getTime() / 1000)}:R>`, inline: true }, { name: 'Reason', value: reason ?? 'No reason provided.' })] }); return true; } catch { logger.warn('mute DM failed', { guildId: guild.id, userId: member.id }); return false; } }
  private async dmUnmuted(member: GuildMember, guild: Guild, reason: string, automatic: boolean): Promise<boolean> { try { await member.send({ embeds: [new EmbedBuilder().setColor(0x57f287).setTitle(automatic ? '🔊 Your mute has expired' : '🔊 You have been unmuted').setDescription(automatic ? 'You can communicate normally again.' : null).addFields({ name: 'Server', value: guild.name }, ...(automatic ? [] : [{ name: 'Reason', value: reason }]))] }); return true; } catch { return false; } }
}
