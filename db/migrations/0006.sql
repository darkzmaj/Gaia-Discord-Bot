CREATE TABLE command_sync_state (
  application_id TEXT PRIMARY KEY,
  schema_fingerprint TEXT NOT NULL,
  guild_cleanup_complete INTEGER NOT NULL DEFAULT 0 CHECK(guild_cleanup_complete IN (0,1)),
  updated_at TEXT NOT NULL
);
