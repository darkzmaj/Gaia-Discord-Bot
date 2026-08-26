# Cookie

Cookie is a production-oriented, multi-server Discord bot with guild-isolated configuration, staff moderation, automatic roles, join-to-create voice rooms, and a deliberately global embed template library.

## Install and run

1. Install Node.js 20.17 or newer.
2. Run `npm install`.
3. Copy `.env.example` to `.env` and add `DISCORD_TOKEN`.
4. Run `npm run migrate` (the app also migrates on startup).
5. Use `npm run dev` for development, or `npm run build && npm start` for production.

`DATABASE_PATH` defaults to `./data/cookie.sqlite`. SQLite is run in WAL mode with foreign keys enabled; use a persistent, backed-up disk volume in production. Cookie always bulk-overwrites its five global Guild Install/Guild-context commands, then removes legacy guild-scoped Cookie commands once per command schema fingerprint. There is no guild-registration mode. Set optional `APPLICATION_ID` as a safety check against the authenticated application ID.

## Discord setup

Enable the **Server Members Intent**. Install Cookie using an OAuth2 URL with both `bot` and `applications.commands` scopes; the latter is required for slash commands to appear. Request only these bot permissions: View Channels, Send Messages, Embed Links, Attach Files, Use Application Commands, Manage Roles, Manage Channels, Move Members, Connect, and Speak. Cookie does not need Administrator. `Manage Channels` is needed for temporary rooms and member-specific mute overwrites; `Manage Roles` is needed for AutoRole and Verification. If commands are missing, verify that the token, authenticated application ID, and OAuth application are the same application.

## Commands and permissions

Administrators are checked at runtime against Discord's actual Administrator permission and automatically count as Cookie staff. Cookie staff is internal, per-guild authorization; it does not grant any Discord role or guild permission.

| Command | Access |
| --- | --- |
| `/admin` (interactive administration dashboard) | Administrator |
| `/mute`, `/unmute` | Cookie Staff or Administrator |
| `/embed` (interactive global embed manager) | Administrator |
| `/unverify` | Administrator |

`/admin` has no options: it opens an ephemeral dashboard with AutoRole, Voice Rooms, Staff, and Moderation sections. Each section uses native Discord role, user, and channel pickers, and changes persist immediately. `/embed` likewise opens an ephemeral manager for creating, editing, sending, listing, and viewing global templates.

## Storage model

All guild-specific operations are keyed by `guild_id`: settings/toggles, autoroles, staff, voice configuration and rooms, room preferences and access lists, and active mutes. The database enforces one active room per owner within a guild. Saved `global_embeds` are intentionally the only cross-guild data.

## Voice rooms

Set a join-to-create channel from `/admin` → Voice Rooms. Joining it creates the lowest available `Room N`, moves the member, and posts owner-only controls in voice chat. Owners can lock/unlock, set a 1–25 limit (or `0` to remove it), and add/remove allow or deny access. Preferences survive room deletion and restarts and are scoped to both guild and owner. Deny takes precedence over allow.

If an owner leaves a non-empty room, Cookie picks a remaining human member, atomically transfers ownership, reapplies that new owner's saved preferences, and attempts to DM them. Empty rooms are deleted. Disabling Voice Rooms cleans up active temporary rooms. On startup, Cookie removes stale room records and reconciles occupied rooms.

## Mutes and ticket exception

Cookie uses **member-specific channel permission overwrites** instead of native timeouts or a mute role. It records and restores each affected user overwrite, so unrelated existing permissions remain intact. Non-ticket channels deny communication/voice participation; channels under the configured `/admin` → Moderation ticket category remain governed by the ticket system. New channels receive restrictions for active mutes. Discord Administrators cannot be muted this way because Discord bypasses channel overwrites.

Mutes accept `s`, `m`, `h`, and `d` combinations (for example `1d 4h 30m`) up to 30 days. They are persisted, restored when a member rejoins, and expired by recovery-aware scheduling after restarts. Configure the ticket category before the first mute so Cookie can set the initial overwrites correctly. Existing custom channel overwrites may require an administrator to review them.

## Backups

`/admin` → **Backups** creates versioned guild-configuration snapshots in SQLite. Each source guild retains at most two snapshots; a new snapshot is written and validated before rotation removes an older one. Automatic backups are disabled by default and, when enabled, run once daily at 03:00 in `Europe/Ljubljana`; a restart after that time creates one missed daily backup rather than duplicates.

A snapshot contains supported channels/categories and their overwrites, non-managed roles, member role assignments, selected guild settings, and Cookie configuration. It intentionally does **not** contain messages, DMs, original Discord IDs, members themselves, managed/integration roles, invites, boosts, webhooks, external integrations, other bots' data, active mutes, active temporary rooms, or a byte-for-byte server image. Snapshots are schema-versioned, SHA-256 integrity checked, and capped at 25 MB.

Restore is a destructive-operation workflow: select a backup, inspect a merge-only preview, then explicitly confirm. Cookie first persists a destination safety snapshot. It creates missing roles/channels, maps newly created IDs, applies only mapped permission overwrites, restores roles only for members present in the destination, and remaps safe Cookie configuration. It never guesses ambiguous existing objects by name and does not delete unrelated destination structure. Discord API failures can produce a partial restore; Cookie records the operation, mappings/progress, warnings, and marks running restores interrupted at startup rather than blindly retrying.

Cross-guild restore requires Administrator in the destination **and** current Administrator access in the source guild. If Cookie no longer belongs to the source, only the creator of a manual backup can access it. Backups are retained for 30 days after Cookie leaves a source guild, then deleted; this retention exists for recovery and contains member IDs/role mappings, so access is deliberately restricted.

## Embed JSON

Embed payloads are message JSON, for example `{"content":"Hello","embeds":[{"title":"Welcome","description":"..."}]}`. Cookie validates JSON syntax, Discord message/embed limits, fields, and HTTP(S) URLs before saving. Names are globally unique, case-insensitively. Cookie tracks embeds it sends from this release onward; after editing, an administrator can update tracked copies in the current guild in place. Previously sent messages are intentionally not retroactively discovered.

## Operations and troubleshooting

Cookie emits structured JSON logs and intentionally excludes secrets. Do not commit `.env` or database files. If roles fail, check Cookie's Manage Roles permission and ensure its highest role is above the target role. If room creation fails, check Manage Channels, Move Members, and Connect. If an autorole/configured channel is deleted, Cookie disables that reference rather than crashing.

Run `npm run build`, `npm run lint`, and `npm test` before deployment. Restart Cookie after deployment; it synchronizes the current command schema during ready. For an explicit production resynchronization, run `npm run commands:sync`: it validates the authenticated application ID, bulk-overwrites the five global commands, fetches them back, verifies their permissions/options, and clears legacy guild commands without touching other Discord applications. Run `npm run commands:sync:dry` to print and validate the exact offline schema without a token.

For a read-only production diagnosis, run `npm run commands:inspect`. It fetches the authenticated application identity, all global commands, each guild’s scoped commands, and API-exposed guild command permission overrides. It compares live `/admin` and `/embed` metadata against `/unverify`. Set optional `DIAGNOSTIC_GUILD_ID` and `DIAGNOSTIC_USER_ID` to report that member’s effective Administrator and Use Application Commands permissions plus Cookie Staff status. It never changes Discord state.

## Production operations

Use Node.js 20.17+ on persistent storage, run `npm ci`, `npm run migrate`, `npm run build`, then `npm start`. Copy `.env.example` to `.env`; it contains placeholders only and must never be committed. Cookie exits non-zero when required configuration is missing, and handles `SIGINT`/`SIGTERM` by stopping mute timers, disconnecting Discord, and closing SQLite. Run it under systemd, Docker, PM2, or another supervisor that restarts non-zero exits.

Back up the SQLite database while the process is stopped, or use SQLite's online backup tooling so the WAL file is included consistently. WAL mode and a five-second busy timeout are enabled. On removal from a guild, Cookie purges that guild's operational configuration, rooms, mutes, restoration records, and sent-embed tracking; global embed templates are intentionally retained. Logs are JSON written to stdout, so collect them through your process manager.
