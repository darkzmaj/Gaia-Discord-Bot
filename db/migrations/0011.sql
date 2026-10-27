-- Embed templates become per-guild. SQLite cannot drop the old global PRIMARY KEY(name), so the template
-- table and its three dependants are rebuilt and copied (works with foreign_keys ON: old children are
-- dropped before the old parent, so no ON DELETE action fires).
--
-- Legacy (pre-0011) templates are adopted by a guild only when that is unambiguous:
--   * every recorded send/schedule of the template happened in one guild -> that guild;
--   * the template was never sent or scheduled and this instance knows exactly one guild -> that guild;
--   * otherwise guild_id stays NULL and the template is hidden from every guild (no cross-guild leak).
CREATE TABLE embed_templates (
  template_id TEXT PRIMARY KEY,
  guild_id TEXT,
  name TEXT NOT NULL COLLATE NOCASE,
  payload_json TEXT NOT NULL,
  created_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_embed_templates_guild_name ON embed_templates(guild_id, name COLLATE NOCASE);
INSERT INTO embed_templates(template_id,guild_id,name,payload_json,created_by_id,created_at,updated_at)
SELECT g.template_id,
  CASE
    WHEN u.guilds = 1 THEN u.guild_id
    WHEN u.guilds = 0 AND (SELECT COUNT(*) FROM guild_settings) = 1 THEN (SELECT guild_id FROM guild_settings)
  END,
  g.name, g.payload_json, g.created_by_id, g.created_at, g.updated_at
FROM global_embeds g
JOIN (
  SELECT e.template_id, COUNT(DISTINCT x.guild_id) AS guilds, MAX(x.guild_id) AS guild_id
  FROM global_embeds e
  LEFT JOIN (
    SELECT embed_id AS template_id, guild_id FROM sent_embed_messages
    UNION SELECT template_id, guild_id FROM scheduled_embed_sends
  ) x ON x.template_id = e.template_id
  GROUP BY e.template_id
) u ON u.template_id = g.template_id;

CREATE TABLE embed_attachments (
  id TEXT PRIMARY KEY,
  template_id TEXT NOT NULL REFERENCES embed_templates(template_id) ON DELETE CASCADE,
  filename TEXT NOT NULL,
  content_type TEXT NOT NULL,
  size_bytes INTEGER NOT NULL CHECK(size_bytes > 0 AND size_bytes <= 8388608),
  sha256 TEXT NOT NULL,
  data BLOB NOT NULL,
  created_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(template_id, filename)
);
INSERT INTO embed_attachments SELECT id,template_id,filename,content_type,size_bytes,sha256,data,created_by_id,created_at FROM global_embed_attachments;

CREATE TABLE sent_embed_messages_v11 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  embed_id TEXT NOT NULL REFERENCES embed_templates(template_id) ON DELETE CASCADE,
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  message_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(embed_id, guild_id, channel_id, message_id)
);
INSERT INTO sent_embed_messages_v11 SELECT id,embed_id,guild_id,channel_id,message_id,created_at FROM sent_embed_messages;

CREATE TABLE scheduled_embed_sends_v11 (
  id TEXT PRIMARY KEY,
  template_id TEXT NOT NULL REFERENCES embed_templates(template_id) ON DELETE RESTRICT,
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
  template_name_snapshot TEXT NOT NULL,
  mention_json TEXT NOT NULL DEFAULT '{"roles":[],"everyone":false,"here":false}',
  mention_outcome TEXT
);
INSERT INTO scheduled_embed_sends_v11 SELECT id,template_id,guild_id,channel_id,scheduled_for_utc,timezone,created_by_user_id,created_at,
  updated_by_user_id,updated_at,status,sent_at,sent_message_id,failure_code,failure_reason,attempt_count,last_attempt_at,
  processing_started_at,template_name_snapshot,mention_json,mention_outcome FROM scheduled_embed_sends;

DROP TABLE sent_embed_messages;
DROP TABLE scheduled_embed_sends;
DROP TABLE global_embed_attachments;
DROP TABLE global_embeds;
ALTER TABLE sent_embed_messages_v11 RENAME TO sent_embed_messages;
ALTER TABLE scheduled_embed_sends_v11 RENAME TO scheduled_embed_sends;
CREATE INDEX idx_embed_attachments_template ON embed_attachments(template_id);
CREATE INDEX idx_sent_embed_messages_guild_embed ON sent_embed_messages(guild_id, embed_id);
CREATE INDEX idx_scheduled_embed_sends_due ON scheduled_embed_sends(status, scheduled_for_utc);
CREATE INDEX idx_scheduled_embed_sends_guild_status ON scheduled_embed_sends(guild_id, status, scheduled_for_utc);
