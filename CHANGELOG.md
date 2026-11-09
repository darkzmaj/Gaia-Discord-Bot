# Changelog

All notable changes to Gaia are documented here. Gaia follows [Semantic Versioning](https://semver.org/).

## 2.0.0 — 2026-11-09

- **Logging Setup.** The first `/admin` in a server asks its owner how Gaia logs: **Global** (every server's logs go
  to one log server, each showing where it came from, with optional server-specific channels there) or
  **Per server** (logs stay in that server). A public bot is always per server. Same setup as Kori's `/ticket`.
- The activity log and the member join/leave log are picked in `/admin` → **Logs**. The `LOG_*` and `MEMBER_LOG_*`
  webhook settings are gone: remove them from `.env`.
- Log delivery goes through the bot itself, in order, with no webhooks to create or keep secret.

## 1.0.1 — 2026-11-04

- Verification honeypot: the DM now carries a single-use, one-day invite back to the server the member was kicked from, instead of one fixed link for every server. Without the Create Invite permission it tells them to contact the server's moderators. The install permission integer now includes Create Invite (`2435902483`).
- README: restore is described as what it is, a full replacement of the destination server after a safety backup, not a merge.

## 1.0.0 — 2026-11-03

First public release.

- Admin dashboard (`/admin`): AutoRole, Verification, Voice Rooms, Staff, Moderation, Backups.
- Staff moderation with `/mute` and `/unmute` (per-member channel overwrites, ticket category exception).
- Join-to-create voice rooms with owner controls and persistent preferences.
- Per-server embed template manager (`/embed`) with attachments, scheduling and tracked copies.
- Versioned, integrity-checked guild configuration backups with preview-then-confirm restore.
- One-command install (`install.sh`, `install.ps1`) and prebuilt multi-arch Docker images.
- Configurable `TIMEZONE` and `BACKUP_HOUR`.
- Gateway TLS upgraded to rustls 0.23 (fixes RustSec advisories in rustls 0.22 / rustls-webpki 0.102).
