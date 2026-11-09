-- Log channels chosen in /admin -> Logs, and the log mode chosen once per server by its owner:
-- 'global' (logs go to the log server) or 'server'. NULL = not chosen yet / not set.
ALTER TABLE guild_settings ADD COLUMN log_channel_id TEXT;
ALTER TABLE guild_settings ADD COLUMN member_log_channel_id TEXT;
ALTER TABLE guild_settings ADD COLUMN log_mode TEXT CHECK (log_mode IN ('global', 'server'));
-- The log server of global mode (at most one).
CREATE TABLE log_hub (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  guild_id TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
-- Server-specific channels inside the log server, for servers in global mode.
CREATE TABLE log_routes (
  source_guild_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  PRIMARY KEY (source_guild_id, kind)
);
