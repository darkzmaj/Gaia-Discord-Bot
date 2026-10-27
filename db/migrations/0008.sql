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
