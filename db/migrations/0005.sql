CREATE TABLE guild_backup_settings (
  guild_id TEXT PRIMARY KEY,
  automatic_enabled INTEGER NOT NULL DEFAULT 0 CHECK(automatic_enabled IN (0,1)),
  last_success_at TEXT,
  last_attempt_at TEXT,
  source_removed_at TEXT
);
CREATE TABLE guild_backups (
  backup_id TEXT PRIMARY KEY,
  source_guild_id TEXT NOT NULL,
  source_guild_name TEXT NOT NULL,
  created_at TEXT NOT NULL,
  created_by TEXT,
  creation_type TEXT NOT NULL CHECK(creation_type IN ('automatic','manual','safety')),
  snapshot_schema_version INTEGER NOT NULL,
  snapshot_json TEXT NOT NULL,
  snapshot_hash TEXT NOT NULL,
  snapshot_size INTEGER NOT NULL,
  role_count INTEGER NOT NULL,
  channel_count INTEGER NOT NULL,
  member_count INTEGER NOT NULL,
  overwrite_count INTEGER NOT NULL,
  source_removed_at TEXT
);
CREATE INDEX idx_guild_backups_source_created ON guild_backups(source_guild_id, created_at DESC);
CREATE TABLE restore_operations (
  restore_id TEXT PRIMARY KEY,
  backup_id TEXT NOT NULL REFERENCES guild_backups(backup_id) ON DELETE RESTRICT,
  destination_guild_id TEXT NOT NULL,
  requested_by TEXT NOT NULL,
  started_at TEXT NOT NULL,
  completed_at TEXT,
  status TEXT NOT NULL CHECK(status IN ('planned','running','completed','completed_with_warnings','failed','interrupted')),
  current_phase TEXT NOT NULL,
  progress_json TEXT NOT NULL,
  errors_json TEXT NOT NULL
);
CREATE INDEX idx_restore_operations_destination_status ON restore_operations(destination_guild_id, status);
