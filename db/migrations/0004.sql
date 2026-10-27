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
