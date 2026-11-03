# Changelog

All notable changes to Gaia are documented here. Gaia follows [Semantic Versioning](https://semver.org/).

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
