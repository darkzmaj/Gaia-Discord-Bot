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
