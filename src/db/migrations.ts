export interface Migration {
  version: number;
  sql: string;
}

export const migrations: Migration[] = [
  {
    version: 1,
    sql: `
CREATE TABLE guild_settings (
  guild_id TEXT PRIMARY KEY,
  autorole_enabled INTEGER NOT NULL DEFAULT 1 CHECK (autorole_enabled IN (0,1)),
  voice_rooms_enabled INTEGER NOT NULL DEFAULT 1 CHECK (voice_rooms_enabled IN (0,1)),
  moderation_enabled INTEGER NOT NULL DEFAULT 1 CHECK (moderation_enabled IN (0,1)),
  ticket_category_id TEXT,
  mute_role_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE guild_autoroles (
  guild_id TEXT PRIMARY KEY REFERENCES guild_settings(guild_id) ON DELETE CASCADE,
  user_role_id TEXT,
  bot_role_id TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE guild_staff (
  guild_id TEXT NOT NULL REFERENCES guild_settings(guild_id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  added_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, user_id)
);
CREATE TABLE guild_voice_config (
  guild_id TEXT PRIMARY KEY REFERENCES guild_settings(guild_id) ON DELETE CASCADE,
  join_channel_id TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE active_voice_rooms (
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  control_channel_id TEXT,
  created_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, channel_id),
  UNIQUE (guild_id, owner_id)
);
CREATE INDEX idx_active_voice_rooms_channel ON active_voice_rooms(channel_id);
CREATE TABLE voice_room_preferences (
  guild_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  user_limit INTEGER NOT NULL DEFAULT 0 CHECK (user_limit BETWEEN 0 AND 25),
  is_locked INTEGER NOT NULL DEFAULT 0 CHECK (is_locked IN (0,1)),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, user_id)
);
CREATE TABLE voice_room_allowed_users (
  guild_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  PRIMARY KEY (guild_id, owner_id, user_id),
  FOREIGN KEY (guild_id, owner_id) REFERENCES voice_room_preferences(guild_id, user_id) ON DELETE CASCADE
);
CREATE TABLE voice_room_denied_users (
  guild_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  PRIMARY KEY (guild_id, owner_id, user_id),
  FOREIGN KEY (guild_id, owner_id) REFERENCES voice_room_preferences(guild_id, user_id) ON DELETE CASCADE
);
CREATE TABLE active_mutes (
  guild_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  moderator_id TEXT NOT NULL,
  reason TEXT,
  started_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX idx_active_mutes_expiry ON active_mutes(expires_at);
CREATE TABLE global_embeds (
  name TEXT PRIMARY KEY COLLATE NOCASE,
  payload_json TEXT NOT NULL,
  created_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
`,
  },
  {
    version: 2,
    sql: `
ALTER TABLE active_voice_rooms ADD COLUMN control_message_id TEXT;
`,
  },
  {
    version: 3,
    sql: `
ALTER TABLE voice_room_preferences ADD COLUMN custom_name TEXT;
ALTER TABLE global_embeds ADD COLUMN template_id TEXT;
UPDATE global_embeds SET template_id=lower(hex(randomblob(16))) WHERE template_id IS NULL;
CREATE UNIQUE INDEX idx_global_embeds_template_id ON global_embeds(template_id);
CREATE TABLE sent_embed_messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  embed_id TEXT NOT NULL REFERENCES global_embeds(template_id) ON DELETE CASCADE,
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  message_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(embed_id, guild_id, channel_id, message_id)
);
CREATE INDEX idx_sent_embed_messages_guild_embed ON sent_embed_messages(guild_id, embed_id);
CREATE TABLE mute_channel_restorations (
  guild_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  existed INTEGER NOT NULL,
  allow_bits TEXT NOT NULL,
  deny_bits TEXT NOT NULL,
  PRIMARY KEY(guild_id, user_id, channel_id)
);
CREATE INDEX idx_mute_channel_restorations_guild_user ON mute_channel_restorations(guild_id, user_id);
`,
  },
  {
    version: 4,
    sql: `
CREATE TABLE verification_config (
  guild_id TEXT PRIMARY KEY REFERENCES guild_settings(guild_id) ON DELETE CASCADE,
  enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
  role_id TEXT,
  title TEXT NOT NULL DEFAULT 'Verification',
  description TEXT NOT NULL DEFAULT 'Click the button below to verify yourself.',
  color INTEGER NOT NULL DEFAULT 5763719,
  button_text TEXT NOT NULL DEFAULT 'Verify',
  channel_id TEXT,
  message_id TEXT,
  updated_at TEXT NOT NULL
);
`,
  },
  {
    version: 5,
    sql: `
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
`,
  },
  {
    version: 6,
    sql: `
CREATE TABLE command_sync_state (
  application_id TEXT PRIMARY KEY,
  schema_fingerprint TEXT NOT NULL,
  guild_cleanup_complete INTEGER NOT NULL DEFAULT 0 CHECK(guild_cleanup_complete IN (0,1)),
  updated_at TEXT NOT NULL
);
`,
  },
  {
    version: 7,
    sql: `
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
`,
  },
  {
    version: 8,
    sql: `
CREATE TABLE global_embed_attachments (
  id TEXT PRIMARY KEY,
  template_id TEXT NOT NULL REFERENCES global_embeds(template_id) ON DELETE CASCADE,
  filename TEXT NOT NULL,
  content_type TEXT NOT NULL,
  size_bytes INTEGER NOT NULL CHECK(size_bytes > 0 AND size_bytes <= 8388608),
  sha256 TEXT NOT NULL,
  data BLOB NOT NULL,
  created_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(template_id, filename)
);
CREATE INDEX idx_embed_attachments_template ON global_embed_attachments(template_id);
CREATE TABLE scheduled_embed_sends (
  id TEXT PRIMARY KEY,
  template_id TEXT NOT NULL REFERENCES global_embeds(template_id) ON DELETE RESTRICT,
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  scheduled_for_utc TEXT NOT NULL,
  timezone TEXT NOT NULL,
  created_by_user_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_by_user_id TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('scheduled','processing','sent','cancelled','failed','missed')),
  sent_at TEXT,
  sent_message_id TEXT,
  failure_code TEXT,
  failure_reason TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  last_attempt_at TEXT,
  processing_started_at TEXT,
  template_name_snapshot TEXT NOT NULL
);
CREATE INDEX idx_scheduled_embed_sends_due ON scheduled_embed_sends(status, scheduled_for_utc);
CREATE INDEX idx_scheduled_embed_sends_guild_status ON scheduled_embed_sends(guild_id, status, scheduled_for_utc);
`,
  },
  {
    version: 9,
    sql: `
ALTER TABLE scheduled_embed_sends ADD COLUMN mention_json TEXT NOT NULL DEFAULT '{"roles":[],"everyone":false,"here":false}';
ALTER TABLE scheduled_embed_sends ADD COLUMN mention_outcome TEXT;
`,
  },
  {
    version: 10,
    sql: `
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
`,
  },
];
