ALTER TABLE restore_operations ADD COLUMN executor_user_id TEXT;
ALTER TABLE restore_operations ADD COLUMN completion_dm_status TEXT NOT NULL DEFAULT 'pending' CHECK(completion_dm_status IN ('pending','attempting','sent','failed','not_applicable'));
ALTER TABLE restore_operations ADD COLUMN completion_dm_attempted_at TEXT;
ALTER TABLE restore_operations ADD COLUMN completion_dm_sent_at TEXT;
ALTER TABLE restore_operations ADD COLUMN completion_dm_failure_reason TEXT;
UPDATE restore_operations SET executor_user_id=requested_by WHERE executor_user_id IS NULL;
CREATE TABLE guild_backup_assets (
  backup_id TEXT NOT NULL REFERENCES guild_backups(backup_id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK(kind IN ('icon','banner','splash','discovery_splash')),
  content_type TEXT NOT NULL,
  size_bytes INTEGER NOT NULL CHECK(size_bytes > 0 AND size_bytes <= 8388608),
  sha256 TEXT NOT NULL,
  data BLOB NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(backup_id, kind)
);
