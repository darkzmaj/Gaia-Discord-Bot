-- SQLite cannot extend a CHECK constraint in place. Preserve every historical
-- operation while adding the explicit terminal state for destructive restores.
CREATE TABLE restore_operations_next (
  restore_id TEXT PRIMARY KEY,
  backup_id TEXT NOT NULL REFERENCES guild_backups(backup_id) ON DELETE RESTRICT,
  destination_guild_id TEXT NOT NULL,
  requested_by TEXT NOT NULL,
  started_at TEXT NOT NULL,
  completed_at TEXT,
  status TEXT NOT NULL CHECK(status IN ('planned','running','completed','completed_with_warnings','failed','failed_partial','interrupted')),
  current_phase TEXT NOT NULL,
  progress_json TEXT NOT NULL,
  errors_json TEXT NOT NULL
);
INSERT INTO restore_operations_next(restore_id,backup_id,destination_guild_id,requested_by,started_at,completed_at,status,current_phase,progress_json,errors_json)
SELECT restore_id,backup_id,destination_guild_id,requested_by,started_at,completed_at,status,current_phase,progress_json,errors_json
FROM restore_operations;
DROP TABLE restore_operations;
ALTER TABLE restore_operations_next RENAME TO restore_operations;
CREATE INDEX idx_restore_operations_destination_status ON restore_operations(destination_guild_id, status);
