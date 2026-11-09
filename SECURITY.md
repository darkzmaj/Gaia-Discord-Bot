# Security Policy

## Supported versions

Only the latest release of Gaia receives security fixes.

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Report them privately through
[GitHub Security Advisories](https://github.com/darkzmaj/Gaia-Discord-Bot/security/advisories/new).
Include the affected version, steps to reproduce and the impact you expect. You should get a reply
within a few days; once a fix is released the advisory is published with credit, unless you prefer
otherwise.

## Operator checklist

- Keep `DISCORD_TOKEN` out of git. `.env` is ignored; the installer creates it with
  owner-only permissions. If a token leaks, reset it in the Discord Developer Portal immediately.
- Grant Gaia only the permissions listed in the README. It never needs Administrator.
- Keep Gaia's role below roles it should not manage; Discord's role hierarchy is enforced.
- Back up the `gaia-data` volume (or `DATABASE_PATH`): backups contain member IDs and role
  assignments, so store them as carefully as the database itself.
- Update regularly (re-run the installer, or `docker compose pull && docker compose up -d`).
