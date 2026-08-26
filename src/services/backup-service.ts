import { createHash, randomUUID } from 'node:crypto';
import { ChannelType, PermissionFlagsBits, type Client, type Guild } from 'discord.js';
import type { CookieRepository, GuildBackup } from '../db/repositories.js';
import { KeyedLock } from './locks.js';
import { logger } from '../logger.js';
import { isDiscordAdministrator } from './authorization.js';

export const BACKUP_SCHEMA_VERSION = 1;
export const MAX_BACKUP_BYTES = 25 * 1024 * 1024;
const supportedChannelTypes = [ChannelType.GuildCategory, ChannelType.GuildText, ChannelType.GuildVoice, ChannelType.GuildAnnouncement, ChannelType.GuildStageVoice, ChannelType.GuildForum, ChannelType.GuildMedia];
type Snapshot = { schemaVersion: number; guild: any; roles: any[]; channels: any[]; members: { userId: string; roleIds: string[] }[]; cookie: any; unsupported?: string[] };
export type RestoreProgress = (phase: string, completed?: number, total?: number) => void | Promise<void>;
export type SavedOverwrite = { id: string; type: 0 | 1; allow: string; deny: string };
export type MappedOverwrite = { id: string; allow: bigint; deny: bigint };
export type RestoreResult = { restoreId: string; status: 'completed' | 'completed_with_warnings' | 'failed_partial'; warnings: string[]; criticalFailures: string[]; safetyBackupId?: string; roles: number; channels: number; overwrites: number; members: number };

/** Maps only destination IDs. Missing targets are deliberately skipped, never guessed. */
export const mapPermissionOverwrites = (saved: SavedOverwrite[], roleMap: Map<string, string>, destinationMemberIds: Set<string>): { mapped: MappedOverwrite[]; skipped: number } => {
  let skipped = 0;
  const mapped = saved.flatMap((overwrite) => {
    const id = overwrite.type === 0 ? roleMap.get(overwrite.id) : destinationMemberIds.has(overwrite.id) ? overwrite.id : undefined;
    if (!id) { skipped++; return []; }
    return [{ id, allow: BigInt(overwrite.allow), deny: BigInt(overwrite.deny) }];
  });
  return { mapped, skipped };
};

const localParts = (date = new Date()) => Object.fromEntries(new Intl.DateTimeFormat('en-GB', { timeZone: 'Europe/Ljubljana', year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', hour12: false }).formatToParts(date).filter((part) => part.type !== 'literal').map((part) => [part.type, part.value]));
const dateKey = (date?: Date) => { const p = localParts(date); return `${p.year}-${p.month}-${p.day}`; };
const progressJson = (roleMap: Map<string, string>, channelMap: Map<string, string>, counts: Record<string, number>, safetyBackupId?: string) => JSON.stringify({ roleMap: [...roleMap], channelMap: [...channelMap], ...counts, safetyBackupId });
const errorText = (error: unknown) => {
  const value = error as { code?: unknown; rawError?: { code?: unknown; message?: unknown }; message?: unknown };
  const code = value?.code ?? value?.rawError?.code;
  const message = String(value?.rawError?.message ?? value?.message ?? 'unknown Discord error').replace(/[\r\n]/g, ' ').slice(0, 280);
  return `Discord error${code !== undefined ? ` ${code}` : ''}: ${message}`;
};
class PartialRestoreError extends Error {}

export class BackupService {
  private readonly locks = new KeyedLock();
  private readonly activeRestores = new Set<string>();
  private timer?: NodeJS.Timeout;
  constructor(private readonly repo: CookieRepository, private readonly client: Client) {}

  start(): void { this.timer = setInterval(() => void this.runAutomatic().catch((error) => logger.error('backup scheduler failed', { error: error instanceof Error ? error.name : 'unknown' })), 30 * 60_000); void this.runAutomatic().catch((error) => logger.error('initial backup scheduler failed', { error: error instanceof Error ? error.name : 'unknown' })); }
  stop(): void { if (this.timer) clearInterval(this.timer); this.timer = undefined; }
  async runAutomatic(now = new Date()): Promise<void> {
    if (Number(localParts(now).hour) < 3) return;
    for (const guild of this.client.guilds.cache.values()) {
      if (this.activeRestores.has(guild.id)) continue;
      const settings = await this.repo.backupSettings(guild.id);
      if (!settings.automatic_enabled || settings.last_success_at && dateKey(new Date(settings.last_success_at)) === dateKey(now)) continue;
      await this.repo.updateBackupSettings(guild.id, { last_attempt_at: now.toISOString() });
      await this.create(guild, null, 'automatic').catch((error) => logger.warn('automatic backup failed', { guildId: guild.id, error: error instanceof Error ? error.name : 'unknown' }));
    }
    await this.repo.purgeExpiredBackups(new Date(now.getTime() - 30 * 86400_000).toISOString());
  }

  async create(guild: Guild, createdBy: string | null, type: 'manual' | 'automatic' | 'safety', protectedBackupId?: string): Promise<GuildBackup> {
    if (type !== 'safety' && this.activeRestores.has(guild.id)) throw new Error('Backup creation is paused while a full server restore is running.');
    return this.locks.run(`backup:${guild.id}`, async () => {
      const snapshot = await this.capture(guild); this.validate(snapshot);
      const snapshotJson = JSON.stringify(snapshot), snapshotSize = Buffer.byteLength(snapshotJson, 'utf8');
      if (snapshotSize > MAX_BACKUP_BYTES) throw new Error(`Snapshot exceeds the ${Math.round(MAX_BACKUP_BYTES / 1024 / 1024)} MB limit.`);
      const backup: GuildBackup = { backup_id: randomUUID(), source_guild_id: guild.id, source_guild_name: guild.name, created_at: new Date().toISOString(), created_by: createdBy, creation_type: type, snapshot_schema_version: BACKUP_SCHEMA_VERSION, snapshot_json: snapshotJson, snapshot_hash: createHash('sha256').update(snapshotJson).digest('hex'), snapshot_size: snapshotSize, role_count: snapshot.roles.length, channel_count: snapshot.channels.length, member_count: snapshot.members.length, overwrite_count: snapshot.channels.reduce((total, channel) => total + channel.overwrites.length, 0), source_removed_at: null };
      await this.repo.storeBackup(backup, protectedBackupId);
      if (type === 'automatic') await this.repo.updateBackupSettings(guild.id, { last_success_at: backup.created_at });
      return backup;
    });
  }

  async canAccess(backup: GuildBackup, userId: string): Promise<boolean> { const source = this.client.guilds.cache.get(backup.source_guild_id); if (source) { const member = await source.members.fetch(userId).catch(() => null); return !!member && isDiscordAdministrator(member); } return backup.creation_type === 'manual' && backup.created_by === userId; }
  async availableTo(userId: string): Promise<GuildBackup[]> { const result: GuildBackup[] = []; for (const backup of await this.repo.allBackups()) if (await this.canAccess(backup, userId)) result.push(backup); return result; }
  async delete(backupId: string, userId: string): Promise<boolean> { const backup = await this.requireAccess(backupId, userId); return this.repo.deleteBackup(backup.backup_id, backup.source_guild_id); }
  async preview(backupId: string, destination: Guild, userId: string): Promise<{ backup: GuildBackup; snapshot: Snapshot; rolesToCreate: number; channelsToCreate: number; overwrites: number; memberAssignments: number; unsupported: string[] }> { const backup = await this.requireAccess(backupId, userId); const snapshot = this.load(backup); return { backup, snapshot, rolesToCreate: snapshot.roles.length, channelsToCreate: snapshot.channels.length, overwrites: backup.overwrite_count, memberAssignments: snapshot.members.length, unsupported: snapshot.unsupported ?? [] }; }

  async restore(backupId: string, destination: Guild, userId: string, report?: RestoreProgress): Promise<RestoreResult> {
    if (this.activeRestores.has(destination.id)) throw new Error('Another backup restore is currently modifying this server.');
    const initialMember = await destination.members.fetch(userId).catch(() => null);
    if (!initialMember || !isDiscordAdministrator(initialMember)) throw new Error('You need Administrator permission in the destination guild.');
    await this.requireAccess(backupId, userId); this.activeRestores.add(destination.id);
    try { return await this.locks.run(`restore:${destination.id}`, () => this.restoreLocked(backupId, destination, userId, report)); }
    finally { this.activeRestores.delete(destination.id); }
  }

  private async restoreLocked(backupId: string, destination: Guild, userId: string, report?: RestoreProgress): Promise<RestoreResult> {
    const destinationMember = await destination.members.fetch(userId).catch(() => null);
    if (!destinationMember || !isDiscordAdministrator(destinationMember)) throw new Error('You need Administrator permission in the destination guild.');
    const backup = await this.requireAccess(backupId, userId), snapshot = this.load(backup);
    if ((await this.repo.backupSettings(destination.id)).source_removed_at) throw new Error('Destination guild is unavailable.');
    const restoreId = randomUUID(), warnings = [...(snapshot.unsupported ?? [])], criticalFailures: string[] = [];
    const roleMap = new Map<string, string>([[snapshot.guild.id, destination.roles.everyone.id]]), channelMap = new Map<string, string>();
    const counts = { roles: 0, channels: 0, overwrites: 0, members: 0 };
    let phase = 'safety backup', safetyBackupId: string | undefined, destructive = false;
    const operation = () => ({ progress_json: progressJson(roleMap, channelMap, counts, safetyBackupId), errors_json: JSON.stringify([...warnings, ...criticalFailures]) });
    await this.repo.createRestoreOperation({ restore_id: restoreId, backup_id: backup.backup_id, destination_guild_id: destination.id, requested_by: userId, status: 'planned', current_phase: phase, ...operation() });
    try {
      await report?.('Safety Backup');
      const safety = await this.create(destination, userId, 'safety', backup.source_guild_id === destination.id ? backup.backup_id : undefined);
      // The row is loaded again after commit: its hash, JSON and schema must all be valid before deletion starts.
      this.load(safety); safetyBackupId = safety.backup_id;
      await this.preflight(destination);
      this.assertReconstructable(snapshot);
      await this.repo.updateRestoreOperation(restoreId, { status: 'running', current_phase: 'destination cleanup', ...operation() });
      phase = 'destination cleanup'; await report?.('Deleting destination channels and roles'); destructive = true;
      await this.deleteDestinationState(destination, warnings, criticalFailures, report);
      this.throwIfCritical(criticalFailures);

      phase = 'roles'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() }); await report?.('Roles', 0, snapshot.roles.length);
      for (let index = 0; index < snapshot.roles.length; index++) {
        const saved = snapshot.roles[index];
        try { const role = await destination.roles.create({ name: saved.name, color: saved.color, hoist: saved.hoist, permissions: BigInt(saved.permissions), mentionable: saved.mentionable, unicodeEmoji: saved.unicodeEmoji ?? undefined, reason: 'Cookie full backup restore' } as any); roleMap.set(saved.id, role.id); counts.roles++; }
        catch (error) { criticalFailures.push(`Role ${saved.name} could not be created — ${errorText(error)}`); }
        if (index % 10 === 0 || index + 1 === snapshot.roles.length) await report?.('Roles', index + 1, snapshot.roles.length);
      }
      await this.applyRoleHierarchy(destination, snapshot, roleMap, warnings, criticalFailures);
      try { await destination.roles.everyone.setPermissions(BigInt(snapshot.guild.everyonePermissions), 'Cookie full backup restore'); }
      catch (error) { criticalFailures.push(`@everyone permissions could not be restored — ${errorText(error)}`); }
      this.throwIfCritical(criticalFailures);

      phase = 'categories'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() });
      await this.createChannels(destination, snapshot.channels.filter((channel) => channel.type === ChannelType.GuildCategory), channelMap, counts, criticalFailures, report, 'Categories');
      // Community requires mapped Rules/Updates channels. Create ordinary channels first, then turn it on before announcement channels.
      phase = 'community prerequisites'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() }); await report?.('Community prerequisites');
      const ordinary = snapshot.channels.filter((channel) => channel.type !== ChannelType.GuildCategory && channel.type !== ChannelType.GuildAnnouncement);
      await this.createChannels(destination, ordinary, channelMap, counts, criticalFailures, report, 'Channels');
      this.throwIfCritical(criticalFailures);
      await this.restoreCommunity(destination, snapshot, channelMap, warnings, criticalFailures);
      this.throwIfCritical(criticalFailures);
      phase = 'announcement channels'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() });
      await this.createChannels(destination, snapshot.channels.filter((channel) => channel.type === ChannelType.GuildAnnouncement), channelMap, counts, criticalFailures, report, 'Announcement channels');
      await this.applyChannelPositions(destination, snapshot.channels, channelMap, warnings, criticalFailures);
      this.throwIfCritical(criticalFailures);

      phase = 'permission overwrites'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() });
      await this.applyOverwrites(destination, snapshot, roleMap, channelMap, counts, warnings, criticalFailures, report);
      this.throwIfCritical(criticalFailures);
      phase = 'member roles'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() });
      await this.replaceMemberRoles(destination, snapshot, roleMap, counts, warnings, report);
      phase = 'guild settings'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() }); await report?.('Guild settings');
      await this.restoreGuildSettings(destination, snapshot, channelMap, warnings, criticalFailures);
      this.throwIfCritical(criticalFailures);
      phase = 'Cookie configuration'; await report?.('Cookie Configuration'); await this.restoreCookieConfig(destination, snapshot.cookie, roleMap, channelMap, warnings, userId);
      phase = 'final validation'; await this.repo.updateRestoreOperation(restoreId, { current_phase: phase, ...operation() }); await report?.('Final Validation');
      await this.finalValidate(destination, snapshot, roleMap, channelMap, warnings, criticalFailures);
      const status: RestoreResult['status'] = criticalFailures.length ? 'failed_partial' : warnings.length ? 'completed_with_warnings' : 'completed';
      await this.repo.updateRestoreOperation(restoreId, { status, current_phase: status === 'failed_partial' ? 'partial failure' : 'complete', ...operation(), completed: true });
      return { restoreId, status, warnings, criticalFailures, safetyBackupId, ...counts };
    } catch (error) {
      if (error instanceof PartialRestoreError || destructive) {
        if (!(error instanceof PartialRestoreError)) criticalFailures.push(`${phase} — ${errorText(error)}`);
        await this.repo.updateRestoreOperation(restoreId, { status: 'failed_partial', current_phase: phase, ...operation(), completed: true });
        return { restoreId, status: 'failed_partial', warnings, criticalFailures, safetyBackupId, ...counts };
      }
      await this.repo.updateRestoreOperation(restoreId, { status: 'failed', current_phase: phase, ...operation(), errors_json: JSON.stringify([...warnings, `${phase} — ${errorText(error)}`]), completed: true });
      throw error;
    }
  }

  private async preflight(guild: Guild): Promise<void> {
    const me = guild.members.me;
    if (!me) return; // cache-less test and partial gateway state; Discord still authoritatively checks every call.
    const needed = [PermissionFlagsBits.ManageChannels, PermissionFlagsBits.ManageRoles, PermissionFlagsBits.ManageGuild];
    if (!me.permissions.has(needed)) throw new Error('Cookie needs Manage Channels, Manage Roles, and Manage Server before a full restore can begin.');
  }
  private assertReconstructable(snapshot: Snapshot): void {
    if (snapshot.channels.some((channel) => channel.type === ChannelType.GuildAnnouncement) && typeof snapshot.guild.communityEnabled !== 'boolean') throw new Error('This older backup did not capture Community state or its required channel mappings, so it cannot safely reconstruct announcement channels. Create a fresh backup with the current Cookie version before a full restore.');
  }
  private throwIfCritical(critical: string[]): void { if (critical.length) throw new PartialRestoreError(critical[critical.length - 1]); }
  private async deleteDestinationState(guild: Guild, warnings: string[], critical: string[], report?: RestoreProgress): Promise<void> {
    const active = new Set((await this.repo.activeRooms(guild.id).catch(() => [])).map((room) => room.channel_id));
    const channels = [...guild.channels.cache.values()].filter((channel: any) => supportedChannelTypes.includes(channel.type) && !active.has(channel.id));
    // Children are removed before their categories. Managed/system objects and undeletable objects are preserved and reported.
    channels.sort((a: any, b: any) => Number(a.type === ChannelType.GuildCategory) - Number(b.type === ChannelType.GuildCategory));
    for (let index = 0; index < channels.length; index++) {
      const channel: any = channels[index];
      if (!channel.deletable) { warnings.push(`Destination channel ${channel.name} is Discord-protected or above Cookie and was preserved.`); continue; }
      try { await channel.delete('Cookie full backup restore: authoritative destination cleanup'); }
      catch (error) { critical.push(`Destination channel ${channel.name} could not be deleted — ${errorText(error)}`); }
      if (index % 10 === 0 || index + 1 === channels.length) await report?.('Deleting channels', index + 1, channels.length);
    }
    const me = guild.members.me;
    const roles = [...guild.roles.cache.values()].filter((role: any) => role.id !== guild.id && !role.managed && !(me && role.id === me.roles.botRole?.id));
    for (const role of roles as any[]) {
      if (!role.editable) { warnings.push(`Destination role ${role.name} is above Cookie's hierarchy and was preserved.`); continue; }
      try { await role.delete('Cookie full backup restore: authoritative destination cleanup'); }
      catch (error) { critical.push(`Destination role ${role.name} could not be deleted — ${errorText(error)}`); }
    }
  }
  private async applyRoleHierarchy(guild: Guild, snapshot: Snapshot, roleMap: Map<string, string>, warnings: string[], critical: string[]): Promise<void> {
    const ceiling = (guild.members.me as any)?.roles?.highest?.position ?? Number.MAX_SAFE_INTEGER;
    for (const saved of [...snapshot.roles].sort((a, b) => a.position - b.position)) {
      const role: any = roleMap.has(saved.id) ? guild.roles.cache.get(roleMap.get(saved.id)!) : undefined;
      if (!role) { critical.push(`Role hierarchy for ${saved.name} cannot be restored because the role is unavailable.`); continue; }
      if (!role.editable) { critical.push(`Role hierarchy for ${saved.name} cannot be restored because Cookie cannot manage it.`); continue; }
      if (saved.position >= ceiling) warnings.push(`Role ${saved.name} was placed below Cookie's highest role instead of its source position.`);
      try { await role.setPosition(Math.min(saved.position, Math.max(1, ceiling - 1)), { reason: 'Cookie full backup restore' }); }
      catch (error) { critical.push(`Role position for ${saved.name} could not be restored — ${errorText(error)}`); }
    }
  }
  private async createChannels(guild: Guild, savedChannels: any[], channelMap: Map<string, string>, counts: Record<string, number>, critical: string[], report: RestoreProgress | undefined, label: string): Promise<void> {
    await report?.(label, 0, savedChannels.length);
    for (let index = 0; index < savedChannels.length; index++) {
      const saved = savedChannels[index], parent = saved.parentId ? channelMap.get(saved.parentId) : undefined;
      if (saved.parentId && !parent) { critical.push(`Channel ${saved.name} cannot be created because its restored category is unavailable.`); continue; }
      try { const channel: any = await guild.channels.create({ ...this.channelOptions(saved, parent), type: saved.type, reason: 'Cookie full backup restore' }); channelMap.set(saved.id, channel.id); counts.channels++; }
      catch (error) { critical.push(`Channel ${saved.name} could not be created — ${errorText(error)}`); }
      if (index % 10 === 0 || index + 1 === savedChannels.length) await report?.(label, index + 1, savedChannels.length);
    }
  }
  private async restoreCommunity(guild: Guild, snapshot: Snapshot, channelMap: Map<string, string>, warnings: string[], critical: string[]): Promise<void> {
    if (typeof snapshot.guild.communityEnabled !== 'boolean') { warnings.push('Community state was not captured by this older backup and was preserved.'); return; }
    const sourceCommunity = !!snapshot.guild.communityEnabled;
    const rules = snapshot.guild.rulesChannelId ? channelMap.get(snapshot.guild.rulesChannelId) : undefined;
    const updates = snapshot.guild.publicUpdatesChannelId ? channelMap.get(snapshot.guild.publicUpdatesChannelId) : undefined;
    if (sourceCommunity && (!rules || !updates)) { critical.push('Community could not be enabled because the backed-up Rules or Community Updates channel was not reconstructed.'); return; }
    const current = new Set((guild as any).features ?? []);
    const features = [...current].filter((feature) => feature !== 'COMMUNITY'); if (sourceCommunity) features.push('COMMUNITY');
    try { await guild.edit({ features, rulesChannel: rules ?? null, publicUpdatesChannel: updates ?? null, reason: 'Cookie full backup restore: Community state' } as any); }
    catch (error) {
      const message = `Community source ${sourceCommunity ? 'enabled' : 'disabled'} could not be applied — ${errorText(error)}`;
      if (sourceCommunity) critical.push(message); else warnings.push(message);
    }
  }
  private async applyChannelPositions(guild: Guild, channels: any[], channelMap: Map<string, string>, warnings: string[], critical: string[]): Promise<void> {
    for (const saved of channels) {
      const channel: any = channelMap.has(saved.id) ? guild.channels.cache.get(channelMap.get(saved.id)!) : undefined;
      if (!channel) { critical.push(`Channel position for ${saved.name} cannot be restored because its channel is unavailable.`); continue; }
      try { await channel.setPosition(saved.position, { reason: 'Cookie full backup restore' }); }
      catch (error) { warnings.push(`Channel position for ${saved.name} could not be restored — ${errorText(error)}`); }
    }
  }
  private async applyOverwrites(guild: Guild, snapshot: Snapshot, roleMap: Map<string, string>, channelMap: Map<string, string>, counts: Record<string, number>, warnings: string[], critical: string[], report?: RestoreProgress): Promise<void> {
    await guild.members.fetch().catch(() => undefined); const memberIds = new Set(guild.members.cache.keys());
    const total = snapshot.channels.reduce((value, channel) => value + channel.overwrites.length, 0); let complete = 0; await report?.('Permissions', 0, total);
    for (const saved of snapshot.channels) {
      const channel: any = channelMap.has(saved.id) ? guild.channels.cache.get(channelMap.get(saved.id)!) : undefined;
      if (!channel || !channel.permissionOverwrites) { critical.push(`Permission overwrites for ${saved.name} cannot be restored because its destination channel is unavailable.`); continue; }
      const mapped = mapPermissionOverwrites(saved.overwrites, roleMap, memberIds);
      if (mapped.skipped) warnings.push(`${mapped.skipped} member-specific permission target${mapped.skipped === 1 ? '' : 's'} for ${saved.name} is no longer in the destination and was skipped.`);
      for (const overwrite of mapped.mapped) {
        try { await channel.permissionOverwrites.edit(overwrite.id, { allow: overwrite.allow, deny: overwrite.deny }, { reason: 'Cookie full backup restore' }); counts.overwrites++; }
        catch (error) { critical.push(`Permission overwrite for ${saved.name} could not be restored — ${errorText(error)}`); }
        complete++; if (complete % 20 === 0 || complete === total) await report?.('Permissions', complete, total);
      }
    }
  }
  private async replaceMemberRoles(guild: Guild, snapshot: Snapshot, roleMap: Map<string, string>, counts: Record<string, number>, warnings: string[], report?: RestoreProgress): Promise<void> {
    await report?.('Member Roles', 0, snapshot.members.length);
    await this.forEachBounded(snapshot.members, 5, async (saved, index) => {
      const member: any = await guild.members.fetch(saved.userId).catch(() => null);
      if (!member) { warnings.push('Roles for one unavailable member were skipped.'); return; }
      const desired = new Set(saved.roleIds.map((id) => roleMap.get(id)).filter((id): id is string => !!id));
      const removable = member.roles.cache.filter((role: any) => role.id !== guild.id && !role.managed && role.editable).map((role: any) => role.id);
      try { if (removable.length) await member.roles.remove(removable, 'Cookie full backup restore: replace saved roles'); if (desired.size) await member.roles.add([...desired], 'Cookie full backup restore: apply saved roles'); counts.members++; }
      catch (error) { warnings.push(`Roles for one member could not be replaced — ${errorText(error)}`); }
      if ((index + 1) % 25 === 0 || index + 1 === snapshot.members.length) await report?.('Member Roles', index + 1, snapshot.members.length);
    });
  }
  private async restoreGuildSettings(guild: Guild, snapshot: Snapshot, channelMap: Map<string, string>, warnings: string[], critical: string[]): Promise<void> {
    const setting = snapshot.guild;
    const mapped = (id: string | null | undefined) => id ? channelMap.get(id) ?? null : null;
    const payload: any = { name: setting.name, verificationLevel: setting.verificationLevel, explicitContentFilter: setting.explicitContentFilter, defaultMessageNotifications: setting.defaultMessageNotifications, afkTimeout: setting.afkTimeout, reason: 'Cookie full backup restore' };
    if ('description' in setting) payload.description = setting.description;
    if ('afkChannelId' in setting) payload.afkChannel = mapped(setting.afkChannelId);
    if ('systemChannelId' in setting) payload.systemChannel = mapped(setting.systemChannelId);
    if ('systemChannelFlags' in setting) payload.systemChannelFlags = BigInt(setting.systemChannelFlags);
    if ('rulesChannelId' in setting) payload.rulesChannel = mapped(setting.rulesChannelId);
    if ('publicUpdatesChannelId' in setting) payload.publicUpdatesChannel = mapped(setting.publicUpdatesChannelId);
    if ('safetyAlertsChannelId' in setting) payload.safetyAlertsChannel = mapped(setting.safetyAlertsChannelId);
    if ('preferredLocale' in setting) payload.preferredLocale = setting.preferredLocale;
    if ('premiumProgressBarEnabled' in setting) payload.premiumProgressBarEnabled = setting.premiumProgressBarEnabled;
    try { await guild.edit(payload); }
    catch (error) { critical.push(`Guild settings could not be restored — ${errorText(error)}`); }
    if (setting.assets?.icon || setting.assets?.banner || setting.assets?.splash) warnings.push('Server icon, banner and splash are recorded for comparison but are not automatically re-uploaded from a backup.');
  }

  private async capture(guild: Guild): Promise<Snapshot> {
    const members = await guild.members.fetch(), activeRoomIds = new Set((await this.repo.activeRooms(guild.id)).map((room) => room.channel_id));
    const managedRoles = [...guild.roles.cache.values()].filter((role) => role.managed);
    const roles = [...guild.roles.cache.values()].filter((role) => !role.managed && role.id !== guild.id).map((role) => ({ id: role.id, name: role.name, color: role.color, hoist: role.hoist, permissions: role.permissions.bitfield.toString(), mentionable: role.mentionable, position: role.position, unicodeEmoji: role.unicodeEmoji ?? null }));
    const unsupported: string[] = managedRoles.length ? [`${managedRoles.length} managed/integration role${managedRoles.length === 1 ? '' : 's'} are preserved, not included.`] : [];
    const controlledFeatures = guild.features.filter((feature) => feature !== 'COMMUNITY');
    if (controlledFeatures.length) unsupported.push(`Discord-controlled guild features are captured for comparison only: ${controlledFeatures.join(', ')}.`);
    const allChannels = [...guild.channels.cache.values()], temporary = allChannels.filter((channel) => activeRoomIds.has(channel.id));
    if (temporary.length) unsupported.push(`${temporary.length} temporary Cookie voice room${temporary.length === 1 ? '' : 's'} are excluded.`);
    const omitted = allChannels.filter((channel) => !supportedChannelTypes.includes(channel.type) && !activeRoomIds.has(channel.id)).length;
    if (omitted) unsupported.push(`${omitted} unsupported channel type${omitted === 1 ? '' : 's'} are not included.`);
    const channels = allChannels.filter((channel) => supportedChannelTypes.includes(channel.type) && !activeRoomIds.has(channel.id)).map((channel: any) => {
      const forum = channel.type === ChannelType.GuildForum || channel.type === ChannelType.GuildMedia;
      if (!forum && channel.videoQualityMode !== null && channel.videoQualityMode !== undefined) unsupported.push(`Video quality mode for ${channel.name} is not restored.`);
      return { id: channel.id, type: channel.type, name: channel.name, parentId: channel.parentId, position: channel.position, topic: channel.topic ?? null, nsfw: channel.nsfw ?? false, rateLimitPerUser: channel.rateLimitPerUser ?? 0, defaultThreadRateLimitPerUser: channel.defaultThreadRateLimitPerUser ?? null, bitrate: channel.bitrate ?? null, userLimit: channel.userLimit ?? null, rtcRegion: channel.rtcRegion ?? null, availableTags: forum ? (channel.availableTags ?? []).map((tag: any) => ({ name: tag.name, moderated: !!tag.moderated, emojiId: tag.emojiId ?? null, emojiName: tag.emojiName ?? null })) : undefined, defaultReactionEmoji: forum && channel.defaultReactionEmoji ? { emojiId: channel.defaultReactionEmoji.emojiId ?? null, emojiName: channel.defaultReactionEmoji.emojiName ?? null } : null, defaultSortOrder: forum ? channel.defaultSortOrder ?? null : null, defaultForumLayout: forum ? channel.defaultForumLayout ?? null : null, overwrites: [...channel.permissionOverwrites.cache.values()].map((overwrite: any) => ({ id: overwrite.id, type: overwrite.type, allow: overwrite.allow.bitfield.toString(), deny: overwrite.deny.bitfield.toString() })) };
    });
    const [settings, autoroles, join, verification, staff] = await Promise.all([this.repo.settings(guild.id), this.repo.autoroles(guild.id), this.repo.joinChannel(guild.id), this.repo.verification(guild.id), this.repo.staffMembers(guild.id)]);
    return { schemaVersion: BACKUP_SCHEMA_VERSION, guild: { id: guild.id, name: guild.name, description: (guild as any).description ?? null, verificationLevel: guild.verificationLevel, explicitContentFilter: guild.explicitContentFilter, defaultMessageNotifications: guild.defaultMessageNotifications, afkTimeout: guild.afkTimeout, afkChannelId: guild.afkChannelId, systemChannelId: guild.systemChannelId, systemChannelFlags: guild.systemChannelFlags.bitfield.toString(), rulesChannelId: guild.rulesChannelId, publicUpdatesChannelId: guild.publicUpdatesChannelId, safetyAlertsChannelId: guild.safetyAlertsChannelId, preferredLocale: guild.preferredLocale, premiumProgressBarEnabled: guild.premiumProgressBarEnabled, communityEnabled: guild.features.includes('COMMUNITY'), features: guild.features, everyonePermissions: guild.roles.everyone.permissions.bitfield.toString(), assets: { icon: guild.iconURL(), banner: guild.bannerURL(), splash: guild.splashURL() } }, roles, channels, members: members.filter((member) => !member.user.bot).map((member) => ({ userId: member.id, roleIds: member.roles.cache.filter((role) => !role.managed && role.id !== guild.id).map((role) => role.id) })), cookie: { settings, autoroles, staff, joinChannelId: join, verification: { ...verification, channel_id: null, message_id: null } }, unsupported };
  }
  private validate(snapshot: Snapshot): void { if (snapshot.schemaVersion !== BACKUP_SCHEMA_VERSION || !snapshot.guild?.id || !Array.isArray(snapshot.roles) || !Array.isArray(snapshot.channels) || !Array.isArray(snapshot.members)) throw new Error('Backup snapshot schema is invalid.'); if (!/^\d+$/.test(String(snapshot.guild.everyonePermissions ?? '0'))) throw new Error('Backup snapshot permissions are invalid.'); const roleIds = new Set<string>(); for (const role of snapshot.roles) { if (!role?.id || !role.name || roleIds.has(role.id) || !/^\d+$/.test(String(role.permissions))) throw new Error('Backup snapshot role structure is invalid.'); roleIds.add(role.id); } const channelIds = new Set(snapshot.channels.map((channel) => channel?.id)); if (channelIds.size !== snapshot.channels.length) throw new Error('Backup snapshot channel structure is invalid.'); for (const channel of snapshot.channels) { if (!channel?.id || !channel.name || !supportedChannelTypes.includes(channel.type) || !Array.isArray(channel.overwrites) || channel.parentId && (!channelIds.has(channel.parentId) || snapshot.channels.find((parent) => parent.id === channel.parentId)?.type !== ChannelType.GuildCategory)) throw new Error('Backup snapshot channel structure is invalid.'); for (const overwrite of channel.overwrites) if (!overwrite?.id || ![0, 1].includes(overwrite.type) || !/^\d+$/.test(overwrite.allow) || !/^\d+$/.test(overwrite.deny)) throw new Error('Backup snapshot overwrite is invalid.'); } for (const member of snapshot.members) if (!member?.userId || !Array.isArray(member.roleIds) || !member.roleIds.every((id) => roleIds.has(id))) throw new Error('Backup snapshot member assignments are invalid.'); }
  private load(backup: GuildBackup): Snapshot { if (backup.snapshot_schema_version !== BACKUP_SCHEMA_VERSION) throw new Error('This backup uses an unsupported snapshot version.'); if (createHash('sha256').update(backup.snapshot_json).digest('hex') !== backup.snapshot_hash) throw new Error('Backup integrity check failed.'); let snapshot: Snapshot; try { snapshot = JSON.parse(backup.snapshot_json) as Snapshot; } catch { throw new Error('Backup snapshot is malformed.'); } this.validate(snapshot); return snapshot; }
  private async requireAccess(backupId: string, userId: string): Promise<GuildBackup> { const backup = await this.repo.backup(backupId); if (!backup) throw new Error('That backup no longer exists.'); if (!await this.canAccess(backup, userId)) throw new Error('You are not authorized to access that backup.'); return backup; }
  private channelOptions(saved: any, parentId?: string): any { const forum = saved.type === ChannelType.GuildForum || saved.type === ChannelType.GuildMedia; const options: any = { name: saved.name, parent: parentId, topic: saved.topic ?? undefined, nsfw: saved.nsfw, rateLimitPerUser: saved.rateLimitPerUser, bitrate: saved.bitrate ?? undefined, userLimit: saved.userLimit ?? undefined, rtcRegion: saved.rtcRegion ?? undefined }; if (forum) Object.assign(options, { defaultThreadRateLimitPerUser: saved.defaultThreadRateLimitPerUser ?? undefined, availableTags: saved.availableTags ?? undefined, defaultReactionEmoji: saved.defaultReactionEmoji ?? undefined, defaultSortOrder: saved.defaultSortOrder ?? undefined, defaultForumLayout: saved.defaultForumLayout ?? undefined }); return options; }
  private async restoreCookieConfig(guild: Guild, cookie: any, roleMap: Map<string, string>, channelMap: Map<string, string>, warnings: string[], requestedBy: string): Promise<void> { const userRole = cookie.autoroles.user_role_id ? roleMap.get(cookie.autoroles.user_role_id) ?? null : null, botRole = cookie.autoroles.bot_role_id ? roleMap.get(cookie.autoroles.bot_role_id) ?? null : null, verificationRole = cookie.verification.role_id ? roleMap.get(cookie.verification.role_id) ?? null : null; await this.repo.updateSetting(guild.id, 'autorole_enabled', cookie.settings.autorole_enabled); await this.repo.updateSetting(guild.id, 'voice_rooms_enabled', cookie.settings.voice_rooms_enabled); await this.repo.updateSetting(guild.id, 'moderation_enabled', cookie.settings.moderation_enabled); await this.repo.updateSetting(guild.id, 'ticket_category_id', cookie.settings.ticket_category_id ? channelMap.get(cookie.settings.ticket_category_id) ?? null : null); await this.repo.setAutorole(guild.id, 'user_role_id', userRole); await this.repo.setAutorole(guild.id, 'bot_role_id', botRole); await this.repo.setJoinChannel(guild.id, cookie.joinChannelId ? channelMap.get(cookie.joinChannelId) ?? null : null); if (Array.isArray(cookie.staff)) await this.repo.replaceStaff(guild.id, cookie.staff, requestedBy); else warnings.push('Cookie Staff was not captured by this older backup and was preserved.'); const enabled = cookie.verification.enabled && !(cookie.settings.autorole_enabled && userRole) && !!verificationRole; if (cookie.verification.enabled && !enabled) warnings.push('Verification was disabled because restored User AutoRole configuration conflicts with Verification or its role was unavailable.'); await this.repo.updateVerification(guild.id, { enabled: enabled ? 1 : 0, role_id: verificationRole, title: cookie.verification.title, description: cookie.verification.description, color: cookie.verification.color, button_text: cookie.verification.button_text, channel_id: null, message_id: null }); }
  private async finalValidate(destination: Guild, snapshot: Snapshot, roleMap: Map<string, string>, channelMap: Map<string, string>, warnings: string[], critical: string[]): Promise<void> {
    for (const saved of snapshot.roles) { const role: any = roleMap.has(saved.id) ? destination.roles.cache.get(roleMap.get(saved.id)!) : undefined; if (!role) critical.push(`Final validation: role ${saved.name} is unavailable.`); else if (role.name !== saved.name || role.permissions.bitfield.toString() !== saved.permissions) critical.push(`Final validation: role ${saved.name} does not match its backup.`); }
    for (const saved of snapshot.channels) { const id = channelMap.get(saved.id), channel: any = id ? destination.channels.cache.get(id) : undefined; if (!channel) { critical.push(`Final validation: channel ${saved.name} is unavailable.`); continue; } if (channel.type !== saved.type || channel.name !== saved.name || (saved.parentId && channel.parentId !== channelMap.get(saved.parentId))) critical.push(`Final validation: channel ${saved.name} does not match its backup.`); const expected = mapPermissionOverwrites(saved.overwrites, roleMap, new Set(destination.members.cache.keys())).mapped; for (const overwrite of expected) { const actual = channel.permissionOverwrites?.cache.get(overwrite.id); if (!actual || actual.allow.bitfield !== overwrite.allow || actual.deny.bitfield !== overwrite.deny) critical.push(`Final validation: permission overwrite for ${saved.name} does not match its backup.`); } }
    if (typeof snapshot.guild.communityEnabled === 'boolean') { const community = snapshot.guild.communityEnabled; if (((destination as any).features ?? []).includes('COMMUNITY') !== community) critical.push(`Final validation: Community state should be ${community ? 'enabled' : 'disabled'}.`); }
    const [settings, autoroles, join, verification] = await Promise.all([this.repo.settings(destination.id), this.repo.autoroles(destination.id), this.repo.joinChannel(destination.id), this.repo.verification(destination.id)]); for (const roleId of [autoroles.user_role_id, autoroles.bot_role_id, verification.role_id]) if (roleId && !destination.roles.cache.has(roleId)) warnings.push('Final validation: a Cookie role reference is unavailable.'); for (const channelId of [settings.ticket_category_id, join]) if (channelId && !destination.channels.cache.has(channelId)) warnings.push('Final validation: a Cookie channel reference is unavailable.'); }
  private async forEachBounded<T>(items: T[], limit: number, work: (item: T, index: number) => Promise<void>): Promise<void> { let next = 0; await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => { while (next < items.length) { const index = next++; await work(items[index], index); } })); }
}
