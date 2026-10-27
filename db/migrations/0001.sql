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
