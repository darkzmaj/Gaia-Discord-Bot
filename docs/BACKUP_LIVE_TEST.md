# Gaia full-server restore acceptance

Run this only with a disposable token and disposable source (A) and destination (B) guilds. A restore is destructive: it removes all destination channels and non-managed roles Gaia can manage, then reconstructs the selected backup. It preserves `@everyone`, managed/integration roles, Gaia's bot role, Discord-protected objects, and active temporary Gaia rooms.

Backups made before this version did not record Community state or Rules/Updates mappings. If one contains announcement channels, Gaia aborts before deletion rather than guessing how to reconstruct Community; create a fresh backup first.

## Prepare the source

- [ ] Create roles, including duplicate display names, and place some near Gaia's hierarchy ceiling.
- [ ] Create private categories, text/voice/stage/forum/media channels, and announcement channels. Set Community on, and configure distinct Rules, Community Updates, system, AFK and safety-alert channels.
- [ ] Exercise role and member overwrites, including intentionally unsynced children. Add people to both guilds, leaving one source member absent from B.
- [ ] Configure Gaia's roles, ticket category, join-to-create channel, verification, AutoRole and feature toggles. Preserve a conflict snapshot with both Verification and User AutoRole enabled.

## Cross-guild reconstruction

- [ ] Create a manual backup of A. Check the captured counts and unsupported list.
- [ ] Add unrelated channels, roles and member role assignments to B. Start Restore Backup from `/admin` → Backups and confirm the red **Full Server Restore** warning.
- [ ] Confirm progress includes a safety backup, destination cleanup, roles, categories, Community prerequisites, announcement channels, overwrites, members, settings, Gaia configuration and final validation.
- [ ] Verify the safety backup ID is shown. Before it completes and revalidates, no destination object may be deleted.
- [ ] Verify old ordinary B channels/roles are gone; A's backed-up roles/channels are new B objects, including duplicate names. Verify Gaia's managed role and active Gaia room survived.
- [ ] Verify Community is enabled before announcement channels appear; Rules and Updates point to mapped B IDs. Check system flags, preferred locale, description, verification/filter/notification values, AFK/system channel and timeout.
- [ ] Verify category parents, positions and every role/member overwrite use B IDs. Test private-category allow/deny behavior with non-admin accounts.
- [ ] Verify B member roles were replaced (old restorable assignments removed and source mapped assignments added). Missing members and unmanageable assignments must be warnings, never ID fallbacks.
- [ ] Verify Gaia configuration points only to B roles/channels; deployment/control-message IDs are absent. A User AutoRole/Verification conflict disables verification with a warning.

## Same-guild and failure cases

- [ ] Change A after its backup, then restore the old backup into A. Verify it reconstructs rather than merges: modified restorable roles/channels are deleted and recreated with new IDs.
- [ ] Remove Manage Channels or Manage Roles and start restore. It must abort before destructive changes, after a valid safety backup where preflight is reached.
- [ ] Force a channel creation or overwrite failure. Verify the database operation is `failed_partial`, the result is red, and its sanitized Discord code/message and safety backup ID are retained.
- [ ] Corrupt one restored overwrite before final validation in a disposable test. Verify final validation produces `failed_partial`.
- [ ] Start two restores into B and attempt an automatic/manual backup during one. Verify the second restore is rejected and the backup is deferred/rejected.
- [ ] With an account that is Administrator in B but not in A, open View/Restore Backup in B. Only B's backups (and those of other guilds where it is currently Administrator) are listed, and the picker answers immediately.
- [ ] Restore a backup containing a role icon and guild icon/banner. Both are fetched only from `cdn.discordapp.com`/`media.discordapp.net`; an icon Discord no longer serves becomes a warning, not a failed restore.
## Final record

- [ ] Record all Discord/API limitations. Icon, banner and splash are captured for comparison but are not uploaded automatically; Discord-controlled/eligibility features outside Community are informational only.
- [ ] Record tested Discord REST errors and permission behavior. Unit tests cannot certify a live full reconstruction.
