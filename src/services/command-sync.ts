import { createHash } from 'node:crypto';
import { commands } from '../commands.js';
import type { CookieRepository } from '../db/repositories.js';
import { logger } from '../logger.js';

type CommandManager = { set: (commands: readonly any[]) => Promise<unknown>; fetch: (...args: any[]) => Promise<any>; permissions?: { fetch: (...args: any[]) => Promise<any> } };
type SyncGuild = { id: string; name?: string; ownerId?: string; commands: CommandManager; members?: { fetch: (id: string) => Promise<any> } };
export type CommandSyncClient = { application: { id: string; commands: CommandManager } | null; user: { id: string; tag?: string; username?: string } | null; guilds: { cache: Map<string, SyncGuild> } };
export type LiveCommand = { name: string; id: string | null; applicationId: string | null; guildId: string | null; defaultMemberPermissions: string | null; dmPermission: boolean | null; contexts: number[] | null; integrationTypes: number[] | null; nsfw: boolean; options: unknown[] };
export type CommandDiagnostic = { application: { id: string; botId: string | null; bot: string | null }; globalCommands: LiveCommand[]; guilds: { id: string; name: string; commands: LiveCommand[]; permissionOverrides: unknown; permissionOverrideError?: string; member?: Record<string, unknown> }[]; comparisons: { left: string; right: string; differences: string[] }[] };

const canonicalize = (value: unknown): unknown => Array.isArray(value) ? value.map(canonicalize) : value && typeof value === 'object' ? Object.fromEntries(Object.entries(value as Record<string, unknown>).sort(([a], [b]) => a.localeCompare(b)).map(([key, child]) => [key, canonicalize(child)])) : value;
export const commandSchemaFingerprint = (definitions = commands): string => createHash('sha256').update(JSON.stringify(canonicalize(definitions))).digest('hex');
export const commandNames = () => commands.map((command) => command.name);

export function assertCommandSchema(): void {
  const expected = ['admin', 'embed', 'mute', 'unmute', 'unverify'];
  if (JSON.stringify(commandNames()) !== JSON.stringify(expected)) throw new Error('Cookie command schema must contain exactly admin, embed, mute, unmute, and unverify.');
  const admin = commands.find((entry) => entry.name === 'admin');
  if (admin && Array.isArray(admin.options) && admin.options.length) throw new Error('admin must not have subcommands or options.');
}

export class CommandSynchronizer {
  constructor(private readonly repo: Pick<CookieRepository, 'commandSyncState' | 'saveCommandSyncState' | 'isStaff'>, private readonly configuredApplicationId?: string) {}

  async synchronize(client: CommandSyncClient, forceCleanup = false): Promise<{ applicationId: string; fingerprint: string; cleaned: number; failed: number }> {
    assertCommandSchema();
    const applicationId = this.applicationId(client), fingerprint = commandSchemaFingerprint();
    logger.info('[commands] command schema loaded', { applicationId, schema: fingerprint.slice(0, 12), scope: 'global' });
    logger.info('[commands] synchronizing global application commands', { count: commands.length });
    await client.application!.commands.set(commands);
    await this.verifyLiveGlobalCommands(client);
    logger.info('[commands] global commands synchronized', { count: commands.length });
    const state = await this.repo.commandSyncState(applicationId);
    const cleanupRequired = forceCleanup || state?.schema_fingerprint !== fingerprint || !state.guild_cleanup_complete;
    if (!cleanupRequired) return { applicationId, fingerprint, cleaned: 0, failed: 0 };
    const outcome = await this.clearLegacyGuildCommands(client.guilds.cache.values());
    await this.repo.saveCommandSyncState(applicationId, fingerprint, outcome.failed === 0);
    return { applicationId, fingerprint, ...outcome };
  }

  async clearGuildOnJoin(guild: SyncGuild): Promise<void> {
    const result = await this.clearLegacyGuildCommands([guild]);
    if (result.failed) logger.warn('[commands] joined guild legacy command cleanup failed', { guildId: guild.id });
  }

  async inspect(client: CommandSyncClient, diagnosticGuildId?: string, diagnosticUserId?: string): Promise<CommandDiagnostic> {
    const applicationId = this.applicationId(client);
    const globalCommands = this.liveCommands(await client.application!.commands.fetch(), null);
    const guilds: CommandDiagnostic['guilds'] = [];
    for (const guild of client.guilds.cache.values()) {
      const report: CommandDiagnostic['guilds'][number] = { id: guild.id, name: guild.name ?? guild.id, commands: this.liveCommands(await guild.commands.fetch(), guild.id), permissionOverrides: [] };
      try { report.permissionOverrides = this.normalizePermissions(await client.application!.commands.permissions?.fetch?.({ guild: guild.id })); } catch (error) { report.permissionOverrideError = this.errorText(error); }
      if (diagnosticGuildId === guild.id && diagnosticUserId && guild.members) {
        const member = await guild.members.fetch(diagnosticUserId).catch(() => null);
        if (member) report.member = { id: member.id, guildOwner: guild.ownerId === member.id, administrator: !!member.permissions?.has?.('Administrator'), useApplicationCommands: !!member.permissions?.has?.('UseApplicationCommands'), effectivePermissions: member.permissions?.bitfield?.toString?.() ?? null, cookieStaff: await this.repo.isStaff?.(guild.id, member.id) ?? false, cookieAuthorization: member.permissions?.has?.('Administrator') ? 'ADMIN' : 'NONE' };
        else report.member = { id: diagnosticUserId, found: false };
      }
      guilds.push(report);
    }
    return { application: { id: applicationId, botId: client.user?.id ?? null, bot: client.user?.tag ?? client.user?.username ?? null }, globalCommands, guilds, comparisons: [this.compareLive(globalCommands, 'admin', 'unverify'), this.compareLive(globalCommands, 'embed', 'unverify')] };
  }

  private applicationId(client: CommandSyncClient): string {
    const applicationId = client.application?.id ?? client.user?.id;
    if (!applicationId) throw new Error('Discord application identity is unavailable after ready.');
    if (this.configuredApplicationId && this.configuredApplicationId !== applicationId) throw new Error('Configured application ID does not match authenticated Cookie application.');
    return applicationId;
  }
  private async clearLegacyGuildCommands(guilds: Iterable<SyncGuild>): Promise<{ cleaned: number; failed: number }> {
    const list = [...guilds]; logger.info('[commands] clearing legacy guild commands', { guilds: list.length }); let cleaned = 0, failed = 0;
    for (const guild of list) try { const existing = await guild.commands.fetch(); if (existing.size) { await guild.commands.set([]); cleaned++; } } catch { failed++; logger.warn('[commands] guild command cleanup failed', { guildId: guild.id }); }
    logger.info('[commands] guild command cleanup complete', { cleaned, failed }); return { cleaned, failed };
  }
  private async verifyLiveGlobalCommands(client: CommandSyncClient): Promise<void> { const live = this.liveCommands(await client.application!.commands.fetch(), null); if (live.length !== commands.length || live.some((command) => !commands.some((expected) => expected.name === command.name))) throw new Error('Live global command set does not match Cookie command schema.'); for (const expected of commands) { const received = live.find((command) => command.name === expected.name)!; const expectedPermissions = expected.default_member_permissions ?? null; if (received.defaultMemberPermissions !== expectedPermissions || JSON.stringify(canonicalize(received.options)) !== JSON.stringify(canonicalize(expected.options ?? [])) || JSON.stringify(received.contexts) !== JSON.stringify(expected.contexts ?? null) || JSON.stringify(received.integrationTypes) !== JSON.stringify(expected.integration_types ?? null)) throw new Error(`Live global command metadata does not match Cookie schema for /${expected.name}.`); } }
  private liveCommands(value: any, guildId: string | null): LiveCommand[] { const entries = value ? (typeof value.values === 'function' ? [...value.values()] : Array.isArray(value) ? value : []) : []; return entries.map((command: any) => ({ name: command.name, id: command.id ?? null, applicationId: command.applicationId ?? command.application_id ?? null, guildId: command.guildId ?? command.guild_id ?? guildId, defaultMemberPermissions: command.defaultMemberPermissions?.bitfield?.toString?.() ?? command.default_member_permissions ?? null, dmPermission: command.dmPermission ?? command.dm_permission ?? null, contexts: command.contexts ?? null, integrationTypes: command.integrationTypes ?? command.integration_types ?? null, nsfw: !!command.nsfw, options: command.options ?? [] })); }
  private normalizePermissions(value: any): unknown { if (!value) return []; if (typeof value.entries === 'function') return [...value.entries()].map(([commandId, permissions]) => ({ commandId, permissions })); return value; }
  private compareLive(commands: LiveCommand[], left: string, right: string): { left: string; right: string; differences: string[] } { const a = commands.find((command) => command.name === left), b = commands.find((command) => command.name === right); if (!a || !b) return { left, right, differences: ['One or both commands are missing from the live global command set.'] }; const fields: (keyof LiveCommand)[] = ['defaultMemberPermissions', 'dmPermission', 'contexts', 'integrationTypes', 'nsfw']; return { left, right, differences: fields.filter((field) => JSON.stringify(a[field]) !== JSON.stringify(b[field])).map((field) => `${field}: ${JSON.stringify(a[field])} != ${JSON.stringify(b[field])}`) }; }
  private errorText(error: unknown): string { return error instanceof Error ? error.message.replace(/[\r\n]/g, ' ').slice(0, 240) : 'unavailable'; }
}
