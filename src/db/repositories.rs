use super::now;
use anyhow::{anyhow, bail, Result};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, params_from_iter, Connection, Params, Row, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuildSettings {
    pub guild_id: String,
    pub autorole_enabled: i64,
    pub voice_rooms_enabled: i64,
    pub moderation_enabled: i64,
    pub ticket_category_id: Option<String>,
    /// This server's own (or, on the log server, default) log channels; `None` = not set.
    pub log_channel_id: Option<String>,
    pub member_log_channel_id: Option<String>,
    /// "global" | "server"; `None` until the server owner chooses.
    pub log_mode: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoicePreference {
    pub guild_id: String,
    pub user_id: String,
    pub user_limit: i64,
    pub is_locked: i64,
    pub custom_name: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActiveRoom {
    pub guild_id: String,
    pub channel_id: String,
    pub owner_id: String,
    pub control_channel_id: Option<String>,
    pub control_message_id: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActiveMute {
    pub guild_id: String,
    pub user_id: String,
    pub moderator_id: String,
    pub reason: Option<String>,
    pub started_at: String,
    pub expires_at: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerificationConfig {
    pub guild_id: String,
    pub enabled: i64,
    pub role_id: Option<String>,
    pub title: String,
    pub description: String,
    pub color: i64,
    pub button_text: String,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuildBackup {
    pub backup_id: String,
    pub source_guild_id: String,
    pub source_guild_name: String,
    pub created_at: String,
    pub created_by: Option<String>,
    /// "automatic" | "manual" | "safety"
    pub creation_type: String,
    pub snapshot_schema_version: i64,
    pub snapshot_json: String,
    pub snapshot_hash: String,
    pub snapshot_size: i64,
    pub role_count: i64,
    pub channel_count: i64,
    pub member_count: i64,
    pub overwrite_count: i64,
    pub source_removed_at: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuildBackupAsset {
    pub backup_id: String,
    /// "icon" | "banner" | "splash" | "discovery_splash"
    pub kind: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub data: Vec<u8>,
    pub created_at: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupSettings {
    pub guild_id: String,
    pub automatic_enabled: i64,
    pub last_success_at: Option<String>,
    pub last_attempt_at: Option<String>,
    pub source_removed_at: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandSyncState {
    pub application_id: String,
    pub schema_fingerprint: String,
    pub guild_cleanup_complete: i64,
    pub updated_at: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedAttachment {
    pub id: String,
    pub template_id: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub data: Vec<u8>,
    pub created_by_id: String,
    pub created_at: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduledEmbedSend {
    pub id: String,
    pub template_id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub scheduled_for_utc: String,
    pub timezone: String,
    pub created_by_user_id: String,
    pub created_at: String,
    pub updated_by_user_id: String,
    pub updated_at: String,
    /// "scheduled" | "processing" | "sent" | "cancelled" | "failed" | "missed"
    pub status: String,
    pub sent_at: Option<String>,
    pub sent_message_id: Option<String>,
    pub failure_code: Option<String>,
    pub failure_reason: Option<String>,
    pub attempt_count: i64,
    pub last_attempt_at: Option<String>,
    pub processing_started_at: Option<String>,
    pub template_name_snapshot: String,
    pub mention_json: Option<String>,
    pub mention_outcome: Option<String>,
}

/// Return shape of `autoroles`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Autoroles {
    pub user_role_id: Option<String>,
    pub bot_role_id: Option<String>,
}
/// Return shape of `access`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomAccess {
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
}
/// Return shape of `embed` / `embed_by_id`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedTemplate {
    pub name: String,
    pub payload_json: String,
    pub template_id: String,
}
/// Row shape of `sent_embeds`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SentEmbed {
    pub id: i64,
    pub channel_id: String,
    pub message_id: String,
}
/// Row shape of `mute_restores`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MuteRestore {
    pub channel_id: String,
    pub existed: i64,
    pub allow_bits: String,
    pub deny_bits: String,
}

/// TS `Partial<Pick<ScheduledEmbedSend, …>>`. `None` = key absent; `Some(None)` = set NULL.
#[derive(Debug, Clone, Default)]
pub struct ScheduleUpdate {
    pub template_id: Option<String>,
    pub channel_id: Option<String>,
    pub scheduled_for_utc: Option<String>,
    pub timezone: Option<String>,
    pub status: Option<String>,
    pub sent_at: Option<Option<String>>,
    pub sent_message_id: Option<Option<String>>,
    pub failure_code: Option<Option<String>>,
    pub failure_reason: Option<Option<String>>,
    pub attempt_count: Option<i64>,
    pub last_attempt_at: Option<Option<String>>,
    pub processing_started_at: Option<Option<String>>,
    pub template_name_snapshot: Option<String>,
    pub mention_json: Option<String>,
    pub mention_outcome: Option<Option<String>>,
}
/// TS `Partial<Pick<VerificationConfig, …>>`.
#[derive(Debug, Clone, Default)]
pub struct VerificationUpdate {
    pub enabled: Option<i64>,
    pub role_id: Option<Option<String>>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub color: Option<i64>,
    pub button_text: Option<String>,
    pub channel_id: Option<Option<String>>,
    pub message_id: Option<Option<String>>,
}
/// TS `Partial<Pick<BackupSettings, …>>`.
#[derive(Debug, Clone, Default)]
pub struct BackupSettingsUpdate {
    pub automatic_enabled: Option<i64>,
    pub last_success_at: Option<Option<String>>,
    pub last_attempt_at: Option<Option<String>>,
    pub source_removed_at: Option<Option<String>>,
}
/// TS inline parameter of `createRestoreOperation`.
#[derive(Debug, Clone, Default)]
pub struct NewRestoreOperation {
    pub restore_id: String,
    pub backup_id: String,
    pub destination_guild_id: String,
    pub requested_by: String,
    pub executor_user_id: Option<String>,
    pub started_at: Option<String>,
    pub status: String,
    pub current_phase: String,
    pub progress_json: String,
    pub errors_json: String,
}
/// TS inline parameter of `updateRestoreOperation`.
#[derive(Debug, Clone, Default)]
pub struct RestoreOperationUpdate {
    pub status: Option<String>,
    pub current_phase: Option<String>,
    pub progress_json: Option<String>,
    pub errors_json: Option<String>,
    pub completed: Option<bool>,
}

// ---- row helpers ----

fn row_json(row: &Row) -> rusqlite::Result<serde_json::Value> {
    let stmt = row.as_ref();
    let mut map = serde_json::Map::new();
    for i in 0..stmt.column_count() {
        let v = match row.get_ref(i)? {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Integer(n) => n.into(),
            ValueRef::Real(f) => f.into(),
            ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
            ValueRef::Blob(b) => b.iter().map(|&x| serde_json::Value::from(x)).collect(),
        };
        map.insert(stmt.column_name(i)?.to_string(), v);
    }
    Ok(serde_json::Value::Object(map))
}
/// Generic row mapping by column name (blob-free structs only; blob rows use explicit mappers).
fn all<T: DeserializeOwned>(c: &Connection, sql: &str, p: impl Params) -> Result<Vec<T>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(p, row_json)?.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|v| Ok(serde_json::from_value(v)?)).collect()
}
fn get<T: DeserializeOwned>(c: &Connection, sql: &str, p: impl Params) -> Result<Option<T>> {
    Ok(all(c, sql, p)?.into_iter().next())
}
fn strings(c: &Connection, sql: &str, p: impl Params) -> Result<Vec<String>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
fn attachment_row(r: &Row) -> rusqlite::Result<EmbedAttachment> {
    Ok(EmbedAttachment {
        id: r.get("id")?,
        template_id: r.get("template_id")?,
        filename: r.get("filename")?,
        content_type: r.get("content_type")?,
        size_bytes: r.get("size_bytes")?,
        sha256: r.get("sha256")?,
        data: r.get("data")?,
        created_by_id: r.get("created_by_id")?,
        created_at: r.get("created_at")?,
    })
}
fn asset_row(r: &Row) -> rusqlite::Result<GuildBackupAsset> {
    Ok(GuildBackupAsset {
        backup_id: r.get("backup_id")?,
        kind: r.get("kind")?,
        content_type: r.get("content_type")?,
        size_bytes: r.get("size_bytes")?,
        sha256: r.get("sha256")?,
        data: r.get("data")?,
        created_at: r.get("created_at")?,
    })
}
fn is_unique(e: &rusqlite::Error) -> bool {
    e.to_string().contains("UNIQUE")
}
/// Column names are interpolated into SQL, so only whitelisted keys are accepted
/// (TS enforced this through its literal union types).
fn checked<'a>(key: &'a str, allowed: &[&str]) -> Result<&'a str> {
    if allowed.contains(&key) {
        Ok(key)
    } else {
        bail!("Invalid field {key}")
    }
}
fn push<T: Into<Value>>(sets: &mut Vec<(&'static str, Value)>, key: &'static str, v: Option<T>) {
    if let Some(v) = v {
        sets.push((key, v.into()));
    }
}

fn ensure_guild(c: &Connection, guild_id: &str) -> Result<()> {
    let stamp = now();
    c.execute("INSERT OR IGNORE INTO guild_settings(guild_id,created_at,updated_at) VALUES (?,?,?)", params![guild_id, stamp, stamp])?;
    c.execute("INSERT OR IGNORE INTO guild_autoroles(guild_id,updated_at) VALUES (?,?)", params![guild_id, stamp])?;
    c.execute("INSERT OR IGNORE INTO guild_voice_config(guild_id,updated_at) VALUES (?,?)", params![guild_id, stamp])?;
    c.execute("INSERT OR IGNORE INTO verification_config(guild_id,color,updated_at) VALUES (?,?,?)", params![guild_id, crate::discord::GAIA_PURPLE, stamp])?;
    Ok(())
}
fn preference(c: &Connection, guild_id: &str, user_id: &str) -> Result<VoicePreference> {
    c.execute("INSERT OR IGNORE INTO voice_room_preferences(guild_id,user_id,updated_at) VALUES (?,?,?)", params![guild_id, user_id, now()])?;
    get(c, "SELECT * FROM voice_room_preferences WHERE guild_id=? AND user_id=?", params![guild_id, user_id])?
        .ok_or_else(|| anyhow!("voice room preference missing"))
}
fn backup_settings(c: &Connection, guild_id: &str) -> Result<BackupSettings> {
    c.execute("INSERT OR IGNORE INTO guild_backup_settings(guild_id) VALUES (?)", params![guild_id])?;
    get(c, "SELECT * FROM guild_backup_settings WHERE guild_id=?", params![guild_id])?
        .ok_or_else(|| anyhow!("backup settings missing"))
}
fn update_backup_settings(c: &Connection, guild_id: &str, f: &BackupSettingsUpdate) -> Result<()> {
    backup_settings(c, guild_id)?;
    let mut sets = Vec::new();
    push(&mut sets, "automatic_enabled", f.automatic_enabled);
    push(&mut sets, "last_success_at", f.last_success_at.clone());
    push(&mut sets, "last_attempt_at", f.last_attempt_at.clone());
    push(&mut sets, "source_removed_at", f.source_removed_at.clone());
    if sets.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "UPDATE guild_backup_settings SET {} WHERE guild_id=?",
        sets.iter().map(|(k, _)| format!("{k}=?")).collect::<Vec<_>>().join(",")
    );
    let values = sets.into_iter().map(|(_, v)| v).chain([Value::from(guild_id.to_string())]);
    c.execute(&sql, params_from_iter(values))?;
    Ok(())
}

pub struct GaiaRepository {
    db: Mutex<Connection>,
}

impl GaiaRepository {
    pub fn new(conn: Connection) -> Self {
        Self { db: Mutex::new(conn) }
    }
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ensure_guild(&self, guild_id: &str) -> Result<()> {
        ensure_guild(&self.conn(), guild_id)
    }
    pub fn settings(&self, guild_id: &str) -> Result<GuildSettings> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        get(&c, "SELECT * FROM guild_settings WHERE guild_id = ?", params![guild_id])?.ok_or_else(|| anyhow!("guild settings missing"))
    }
    /// key: "autorole_enabled" | "voice_rooms_enabled" | "moderation_enabled" | "ticket_category_id" | "log_channel_id"
    /// | "member_log_channel_id" | "log_mode"
    pub fn update_setting(&self, guild_id: &str, key: &str, value: Value) -> Result<()> {
        let key = checked(
            key,
            &["autorole_enabled", "voice_rooms_enabled", "moderation_enabled", "ticket_category_id", "log_channel_id", "member_log_channel_id", "log_mode"],
        )?;
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute(&format!("UPDATE guild_settings SET {key} = ?, updated_at = ? WHERE guild_id = ?"), params![value, now(), guild_id])?;
        Ok(())
    }
    /// key: "autorole_enabled" | "voice_rooms_enabled" | "moderation_enabled"
    pub fn toggle(&self, guild_id: &str, key: &str) -> Result<bool> {
        let key = checked(key, &["autorole_enabled", "voice_rooms_enabled", "moderation_enabled"])?;
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute(&format!("UPDATE guild_settings SET {key} = 1 - {key}, updated_at = ? WHERE guild_id = ?"), params![now(), guild_id])?;
        let v: i64 = c.query_row(&format!("SELECT {key} FROM guild_settings WHERE guild_id = ?"), params![guild_id], |r| r.get(0))?;
        Ok(v == 1)
    }
    /// kind: "user_role_id" | "bot_role_id"
    pub fn set_autorole(&self, guild_id: &str, kind: &str, role_id: Option<&str>) -> Result<()> {
        let kind = checked(kind, &["user_role_id", "bot_role_id"])?;
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute(&format!("UPDATE guild_autoroles SET {kind} = ?, updated_at = ? WHERE guild_id = ?"), params![role_id, now(), guild_id])?;
        Ok(())
    }
    pub fn autoroles(&self, guild_id: &str) -> Result<Autoroles> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        get(&c, "SELECT user_role_id, bot_role_id FROM guild_autoroles WHERE guild_id = ?", params![guild_id])?.ok_or_else(|| anyhow!("autoroles missing"))
    }
    pub fn add_staff(&self, guild_id: &str, user_id: &str, added_by: &str) -> Result<()> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute("INSERT OR IGNORE INTO guild_staff(guild_id,user_id,added_by_id,created_at) VALUES (?,?,?,?)", params![guild_id, user_id, added_by, now()])?;
        Ok(())
    }
    pub fn remove_staff(&self, guild_id: &str, user_id: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM guild_staff WHERE guild_id = ? AND user_id = ?", params![guild_id, user_id])? == 1)
    }
    pub fn is_staff(&self, guild_id: &str, user_id: &str) -> Result<bool> {
        let c = self.conn();
        let mut stmt = c.prepare("SELECT 1 FROM guild_staff WHERE guild_id = ? AND user_id = ?")?;
        Ok(stmt.exists(params![guild_id, user_id])?)
    }
    pub fn staff_count(&self, guild_id: &str) -> Result<i64> {
        Ok(self.conn().query_row("SELECT COUNT(*) AS count FROM guild_staff WHERE guild_id = ?", params![guild_id], |r| r.get(0))?)
    }
    pub fn staff_members(&self, guild_id: &str) -> Result<Vec<String>> {
        strings(&self.conn(), "SELECT user_id FROM guild_staff WHERE guild_id=? ORDER BY created_at", params![guild_id])
    }
    pub fn replace_staff(&self, guild_id: &str, user_ids: &[String], added_by: &str) -> Result<()> {
        let mut c = self.conn();
        ensure_guild(&c, guild_id)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM guild_staff WHERE guild_id=?", params![guild_id])?;
        let mut seen = Vec::new();
        for user_id in user_ids {
            if seen.contains(&user_id) {
                continue;
            }
            seen.push(user_id);
            tx.execute("INSERT INTO guild_staff(guild_id,user_id,added_by_id,created_at) VALUES (?,?,?,?)", params![guild_id, user_id, added_by, now()])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn set_join_channel(&self, guild_id: &str, channel_id: Option<&str>) -> Result<()> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute("UPDATE guild_voice_config SET join_channel_id=?, updated_at=? WHERE guild_id=?", params![channel_id, now(), guild_id])?;
        Ok(())
    }
    pub fn join_channel(&self, guild_id: &str) -> Result<Option<String>> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        let row: Option<Option<String>> = c
            .query_row("SELECT join_channel_id FROM guild_voice_config WHERE guild_id=?", params![guild_id], |r| r.get(0))
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
        Ok(row.flatten())
    }
    pub fn active_room(&self, guild_id: &str, channel_id: &str) -> Result<Option<ActiveRoom>> {
        get(&self.conn(), "SELECT * FROM active_voice_rooms WHERE guild_id=? AND channel_id=?", params![guild_id, channel_id])
    }
    pub fn active_room_for_owner(&self, guild_id: &str, owner_id: &str) -> Result<Option<ActiveRoom>> {
        get(&self.conn(), "SELECT * FROM active_voice_rooms WHERE guild_id=? AND owner_id=?", params![guild_id, owner_id])
    }
    pub fn create_room(&self, room: &ActiveRoom) -> Result<bool> {
        match self.conn().execute(
            "INSERT INTO active_voice_rooms(guild_id,channel_id,owner_id,control_channel_id,control_message_id,created_at) VALUES (?,?,?,?,?,?)",
            params![room.guild_id, room.channel_id, room.owner_id, room.control_channel_id, room.control_message_id, now()],
        ) {
            Ok(_) => Ok(true),
            Err(e) if is_unique(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn set_room_control_message(&self, guild_id: &str, channel_id: &str, message_id: Option<&str>) -> Result<()> {
        self.conn().execute("UPDATE active_voice_rooms SET control_message_id=? WHERE guild_id=? AND channel_id=?", params![message_id, guild_id, channel_id])?;
        Ok(())
    }
    pub fn transfer_room(&self, guild_id: &str, channel_id: &str, from_owner_id: &str, to_owner_id: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE active_voice_rooms SET owner_id=? WHERE guild_id=? AND channel_id=? AND owner_id=?",
            params![to_owner_id, guild_id, channel_id, from_owner_id],
        )? == 1)
    }
    pub fn delete_room(&self, guild_id: &str, channel_id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM active_voice_rooms WHERE guild_id=? AND channel_id=?", params![guild_id, channel_id])?;
        Ok(())
    }
    pub fn active_rooms(&self, guild_id: Option<&str>) -> Result<Vec<ActiveRoom>> {
        let c = self.conn();
        match guild_id.filter(|g| !g.is_empty()) {
            Some(g) => all(&c, "SELECT * FROM active_voice_rooms WHERE guild_id=?", params![g]),
            None => all(&c, "SELECT * FROM active_voice_rooms", []),
        }
    }
    pub fn preference(&self, guild_id: &str, user_id: &str) -> Result<VoicePreference> {
        preference(&self.conn(), guild_id, user_id)
    }
    /// key: "user_limit" | "is_locked" | "custom_name"
    pub fn update_preference(&self, guild_id: &str, user_id: &str, key: &str, value: Value) -> Result<()> {
        let key = checked(key, &["user_limit", "is_locked", "custom_name"])?;
        let c = self.conn();
        preference(&c, guild_id, user_id)?;
        c.execute(&format!("UPDATE voice_room_preferences SET {key}=?,updated_at=? WHERE guild_id=? AND user_id=?"), params![value, now(), guild_id, user_id])?;
        Ok(())
    }
    pub fn reset_preference(&self, guild_id: &str, user_id: &str) -> Result<()> {
        let mut c = self.conn();
        preference(&c, guild_id, user_id)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE voice_room_preferences SET user_limit=0,is_locked=0,custom_name=NULL,updated_at=? WHERE guild_id=? AND user_id=?", params![now(), guild_id, user_id])?;
        tx.execute("DELETE FROM voice_room_allowed_users WHERE guild_id=? AND owner_id=?", params![guild_id, user_id])?;
        tx.execute("DELETE FROM voice_room_denied_users WHERE guild_id=? AND owner_id=?", params![guild_id, user_id])?;
        tx.commit()?;
        Ok(())
    }
    /// kind: "allowed" | "denied" (TS param name `type`)
    pub fn set_access(&self, guild_id: &str, owner_id: &str, user_id: &str, kind: &str, present: bool) -> Result<()> {
        let c = self.conn();
        preference(&c, guild_id, owner_id)?;
        let table = if kind == "allowed" { "voice_room_allowed_users" } else { "voice_room_denied_users" };
        if present {
            c.execute(&format!("INSERT OR IGNORE INTO {table}(guild_id,owner_id,user_id) VALUES (?,?,?)"), params![guild_id, owner_id, user_id])?;
        } else {
            c.execute(&format!("DELETE FROM {table} WHERE guild_id=? AND owner_id=? AND user_id=?"), params![guild_id, owner_id, user_id])?;
        }
        Ok(())
    }
    pub fn access(&self, guild_id: &str, owner_id: &str) -> Result<RoomAccess> {
        let c = self.conn();
        Ok(RoomAccess {
            allowed: strings(&c, "SELECT user_id FROM voice_room_allowed_users WHERE guild_id=? AND owner_id=?", params![guild_id, owner_id])?,
            denied: strings(&c, "SELECT user_id FROM voice_room_denied_users WHERE guild_id=? AND owner_id=?", params![guild_id, owner_id])?,
        })
    }
    pub fn add_mute(&self, mute: &ActiveMute) -> Result<bool> {
        match self.conn().execute(
            "INSERT INTO active_mutes(guild_id,user_id,moderator_id,reason,started_at,expires_at) VALUES (?,?,?,?,?,?)",
            params![mute.guild_id, mute.user_id, mute.moderator_id, mute.reason, mute.started_at, mute.expires_at],
        ) {
            Ok(_) => Ok(true),
            Err(e) if is_unique(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn mute(&self, guild_id: &str, user_id: &str) -> Result<Option<ActiveMute>> {
        get(&self.conn(), "SELECT * FROM active_mutes WHERE guild_id=? AND user_id=?", params![guild_id, user_id])
    }
    /// `at` defaults to now.
    pub fn due_mutes(&self, at: Option<&str>) -> Result<Vec<ActiveMute>> {
        let at = at.map(str::to_string).unwrap_or_else(now);
        all(&self.conn(), "SELECT * FROM active_mutes WHERE expires_at <= ?", params![at])
    }
    pub fn remove_mute(&self, guild_id: &str, user_id: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM active_mutes WHERE guild_id=? AND user_id=?", params![guild_id, user_id])? == 1)
    }
    // ---- embed templates: every read/write is scoped to the owning guild ----

    /// `replace` defaults to false. Names are unique per guild, case-insensitively.
    pub fn save_embed(&self, guild_id: &str, name: &str, json: &str, actor: &str, replace: Option<bool>) -> Result<bool> {
        let c = self.conn();
        let existing = c.prepare("SELECT 1 FROM embed_templates WHERE guild_id=? AND name=? COLLATE NOCASE")?.exists(params![guild_id, name])?;
        if existing && !replace.unwrap_or(false) {
            return Ok(false);
        }
        if existing {
            c.execute(
                "UPDATE embed_templates SET payload_json=?,updated_at=? WHERE guild_id=? AND name=? COLLATE NOCASE",
                params![json, now(), guild_id, name],
            )?;
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            c.execute(
                "INSERT INTO embed_templates(template_id,guild_id,name,payload_json,created_by_id,created_at,updated_at) VALUES (?,?,?,?,?,?,?)",
                params![id, guild_id, name, json, actor, now(), now()],
            )?;
        }
        Ok(true)
    }
    pub fn embed(&self, guild_id: &str, name: &str) -> Result<Option<EmbedTemplate>> {
        get(
            &self.conn(),
            "SELECT name,payload_json,template_id FROM embed_templates WHERE guild_id=? AND name=? COLLATE NOCASE",
            params![guild_id, name],
        )
    }
    pub fn embed_by_id(&self, guild_id: &str, template_id: &str) -> Result<Option<EmbedTemplate>> {
        get(&self.conn(), "SELECT name,payload_json,template_id FROM embed_templates WHERE guild_id=? AND template_id=?", params![guild_id, template_id])
    }
    /// Defaults: offset 0, limit 20.
    pub fn embeds(&self, guild_id: &str, offset: Option<i64>, limit: Option<i64>) -> Result<Vec<String>> {
        strings(
            &self.conn(),
            "SELECT name FROM embed_templates WHERE guild_id=? ORDER BY name COLLATE NOCASE LIMIT ? OFFSET ?",
            params![guild_id, limit.unwrap_or(20), offset.unwrap_or(0)],
        )
    }
    pub fn attachments(&self, guild_id: &str, template_id: &str) -> Result<Vec<EmbedAttachment>> {
        let c = self.conn();
        let mut stmt = c.prepare(
            "SELECT a.* FROM embed_attachments a JOIN embed_templates t ON t.template_id=a.template_id WHERE t.guild_id=? AND a.template_id=? ORDER BY a.filename COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![guild_id, template_id], attachment_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    #[cfg(test)]
    pub fn attachment(&self, guild_id: &str, template_id: &str, filename: &str) -> Result<Option<EmbedAttachment>> {
        Ok(self.attachments(guild_id, template_id)?.into_iter().find(|a| a.filename == filename))
    }
    #[cfg(test)]
    pub fn schedules_for_template(&self, template_id: &str, statuses: Option<&[&str]>) -> Result<Vec<ScheduledEmbedSend>> {
        let statuses = statuses.unwrap_or(&["scheduled", "processing"]);
        if statuses.is_empty() {
            return Ok(vec![]);
        }
        let sql = format!(
            "SELECT * FROM scheduled_embed_sends WHERE template_id=? AND status IN ({}) ORDER BY scheduled_for_utc",
            vec!["?"; statuses.len()].join(",")
        );
        all(&self.conn(), &sql, params_from_iter(std::iter::once(&template_id).chain(statuses.iter())))
    }
    /// Upserts on (template, filename) when the template belongs to `guild_id`, enforcing the per-template
    /// count and total-size caps (a same-named file being replaced does not count against them).
    /// Errors are user-facing.
    pub fn save_attachment(&self, guild_id: &str, a: &EmbedAttachment, max_count: usize, max_total_bytes: usize) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !tx.prepare("SELECT 1 FROM embed_templates WHERE guild_id=? AND template_id=?")?.exists(params![guild_id, a.template_id])? {
            bail!("That embed no longer exists.");
        }
        let (count, total): (i64, i64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(size_bytes),0) FROM embed_attachments WHERE template_id=? AND filename<>?",
            params![a.template_id, a.filename],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if count as usize >= max_count {
            bail!("A template can hold at most {max_count} attachments. Remove one first.");
        }
        if (total + a.size_bytes) as usize > max_total_bytes {
            bail!("Template attachment total would exceed the {} MB limit.", max_total_bytes / 1024 / 1024);
        }
        tx.execute(
            "INSERT INTO embed_attachments(id,template_id,filename,content_type,size_bytes,sha256,data,created_by_id,created_at) VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT(template_id,filename) DO UPDATE SET content_type=excluded.content_type,size_bytes=excluded.size_bytes,sha256=excluded.sha256,data=excluded.data,created_by_id=excluded.created_by_id,created_at=excluded.created_at",
            params![a.id, a.template_id, a.filename, a.content_type, a.size_bytes, a.sha256, a.data, a.created_by_id, a.created_at],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn delete_attachment(&self, guild_id: &str, template_id: &str, filename: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM embed_attachments WHERE template_id=? AND filename=? AND template_id IN (SELECT template_id FROM embed_templates WHERE guild_id=?)",
            params![template_id, filename, guild_id],
        )? == 1)
    }
    /// Fails unless the template belongs to the schedule's guild.
    pub fn create_schedule(&self, s: &ScheduledEmbedSend) -> Result<()> {
        let mention = s.mention_json.clone().unwrap_or_else(|| r#"{"roles":[],"everyone":false,"here":false}"#.into());
        let inserted = self.conn().execute(
            "INSERT INTO scheduled_embed_sends(id,template_id,guild_id,channel_id,scheduled_for_utc,timezone,created_by_user_id,created_at,updated_by_user_id,updated_at,status,sent_at,sent_message_id,failure_code,failure_reason,attempt_count,last_attempt_at,processing_started_at,template_name_snapshot,mention_json) SELECT ?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,? WHERE EXISTS(SELECT 1 FROM embed_templates WHERE template_id=?2 AND guild_id=?3)",
            params![
                s.id, s.template_id, s.guild_id, s.channel_id, s.scheduled_for_utc, s.timezone, s.created_by_user_id, s.created_at,
                s.updated_by_user_id, s.updated_at, s.status, s.sent_at, s.sent_message_id, s.failure_code, s.failure_reason,
                s.attempt_count, s.last_attempt_at, s.processing_started_at, s.template_name_snapshot, mention
            ],
        )?;
        if inserted != 1 {
            bail!("That embed no longer exists.");
        }
        Ok(())
    }
    pub fn schedule(&self, guild_id: &str, id: &str) -> Result<Option<ScheduledEmbedSend>> {
        get(&self.conn(), "SELECT * FROM scheduled_embed_sends WHERE id=? AND guild_id=?", params![id, guild_id])
    }
    /// `limit` defaults to 25.
    pub fn upcoming_schedules(&self, guild_id: &str, limit: Option<i64>) -> Result<Vec<ScheduledEmbedSend>> {
        all(
            &self.conn(),
            "SELECT * FROM scheduled_embed_sends WHERE guild_id=? AND status=? ORDER BY scheduled_for_utc LIMIT ?",
            params![guild_id, "scheduled", limit.unwrap_or(25)],
        )
    }
    /// `limit` defaults to 25.
    pub fn schedule_history(&self, guild_id: &str, limit: Option<i64>) -> Result<Vec<ScheduledEmbedSend>> {
        all(
            &self.conn(),
            "SELECT * FROM scheduled_embed_sends WHERE guild_id=? AND status<>? ORDER BY updated_at DESC LIMIT ?",
            params![guild_id, "scheduled", limit.unwrap_or(25)],
        )
    }
    pub fn update_schedule(&self, id: &str, actor: &str, f: &ScheduleUpdate) -> Result<bool> {
        let mut sets = Vec::new();
        push(&mut sets, "template_id", f.template_id.clone());
        push(&mut sets, "channel_id", f.channel_id.clone());
        push(&mut sets, "scheduled_for_utc", f.scheduled_for_utc.clone());
        push(&mut sets, "timezone", f.timezone.clone());
        push(&mut sets, "status", f.status.clone());
        push(&mut sets, "sent_at", f.sent_at.clone());
        push(&mut sets, "sent_message_id", f.sent_message_id.clone());
        push(&mut sets, "failure_code", f.failure_code.clone());
        push(&mut sets, "failure_reason", f.failure_reason.clone());
        push(&mut sets, "attempt_count", f.attempt_count);
        push(&mut sets, "last_attempt_at", f.last_attempt_at.clone());
        push(&mut sets, "processing_started_at", f.processing_started_at.clone());
        push(&mut sets, "template_name_snapshot", f.template_name_snapshot.clone());
        push(&mut sets, "mention_json", f.mention_json.clone());
        push(&mut sets, "mention_outcome", f.mention_outcome.clone());
        if sets.is_empty() {
            return Ok(false);
        }
        let sql = format!(
            "UPDATE scheduled_embed_sends SET {},updated_by_user_id=?,updated_at=? WHERE id=?",
            sets.iter().map(|(k, _)| format!("{k}=?")).collect::<Vec<_>>().join(",")
        );
        let values = sets.into_iter().map(|(_, v)| v).chain([Value::from(actor.to_string()), Value::from(now()), Value::from(id.to_string())]);
        Ok(self.conn().execute(&sql, params_from_iter(values))? == 1)
    }
    pub fn cancel_schedule(&self, id: &str, guild_id: &str, actor: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE scheduled_embed_sends SET status=?,updated_by_user_id=?,updated_at=? WHERE id=? AND guild_id=? AND status=?",
            params!["cancelled", actor, now(), id, guild_id, "scheduled"],
        )? == 1)
    }
    /// Atomic SQLite claim: only one process can transition a due row to processing.
    pub fn claim_due_schedule(&self, now_utc: &str) -> Result<Option<ScheduledEmbedSend>> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate: Option<ScheduledEmbedSend> = get(
            &tx,
            "SELECT * FROM scheduled_embed_sends WHERE status=? AND scheduled_for_utc<=? ORDER BY scheduled_for_utc LIMIT 1",
            params!["scheduled", now_utc],
        )?;
        let Some(mut candidate) = candidate else {
            tx.commit()?;
            return Ok(None);
        };
        let stamp = now();
        let claimed = tx.execute(
            "UPDATE scheduled_embed_sends SET status=?,attempt_count=attempt_count+1,last_attempt_at=?,processing_started_at=?,updated_at=? WHERE id=? AND status=?",
            params!["processing", stamp, stamp, stamp, candidate.id, "scheduled"],
        )?;
        tx.commit()?;
        if claimed != 1 {
            return Ok(None);
        }
        candidate.status = "processing".into();
        candidate.attempt_count += 1;
        candidate.last_attempt_at = Some(stamp.clone());
        candidate.processing_started_at = Some(stamp);
        Ok(Some(candidate))
    }
    pub fn reconcile_stale_schedule_processing(&self, cutoff_utc: &str) -> Result<i64> {
        Ok(self.conn().execute(
            "UPDATE scheduled_embed_sends SET status=?,failure_code=?,failure_reason=?,updated_at=? WHERE status=? AND processing_started_at<?",
            params![
                "failed",
                "AMBIGUOUS_PROCESSING",
                "Gaia restarted after claiming this send; delivery is intentionally not retried automatically to avoid duplicates.",
                now(),
                "processing",
                cutoff_utc
            ],
        )? as i64)
    }
    /// Only records sends of a template owned by `guild_id`.
    pub fn track_sent_embed(&self, embed_id: &str, guild_id: &str, channel_id: &str, message_id: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO sent_embed_messages(embed_id,guild_id,channel_id,message_id,created_at) SELECT ?1,?2,?3,?4,?5 WHERE EXISTS(SELECT 1 FROM embed_templates WHERE template_id=?1 AND guild_id=?2)",
            params![embed_id, guild_id, channel_id, message_id, now()],
        )?;
        Ok(())
    }
    pub fn sent_embeds(&self, embed_id: &str, guild_id: &str) -> Result<Vec<SentEmbed>> {
        all(
            &self.conn(),
            "SELECT s.id,s.channel_id,s.message_id FROM sent_embed_messages s JOIN embed_templates t ON t.template_id=s.embed_id WHERE s.embed_id=? AND s.guild_id=? AND t.guild_id=s.guild_id",
            params![embed_id, guild_id],
        )
    }
    pub fn remove_sent_embed(&self, id: i64) -> Result<()> {
        self.conn().execute("DELETE FROM sent_embed_messages WHERE id=?", params![id])?;
        Ok(())
    }
    pub fn remove_sent_embeds_in_channel(&self, guild_id: &str, channel_id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM sent_embed_messages WHERE guild_id=? AND channel_id=?", params![guild_id, channel_id])?;
        Ok(())
    }
    pub fn save_mute_restore(&self, guild_id: &str, user_id: &str, channel_id: &str, existed: bool, allow: &str, deny: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO mute_channel_restorations(guild_id,user_id,channel_id,existed,allow_bits,deny_bits) VALUES (?,?,?,?,?,?)",
            params![guild_id, user_id, channel_id, existed as i64, allow, deny],
        )?;
        Ok(())
    }
    pub fn mute_restores(&self, guild_id: &str, user_id: &str) -> Result<Vec<MuteRestore>> {
        all(
            &self.conn(),
            "SELECT channel_id,existed,allow_bits,deny_bits FROM mute_channel_restorations WHERE guild_id=? AND user_id=?",
            params![guild_id, user_id],
        )
    }
    pub fn clear_mute_restores(&self, guild_id: &str, user_id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM mute_channel_restorations WHERE guild_id=? AND user_id=?", params![guild_id, user_id])?;
        Ok(())
    }
    pub fn remove_mute_restores_for_channel(&self, guild_id: &str, channel_id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM mute_channel_restorations WHERE guild_id=? AND channel_id=?", params![guild_id, channel_id])?;
        Ok(())
    }
    pub fn active_mutes(&self, guild_id: &str) -> Result<Vec<ActiveMute>> {
        all(&self.conn(), "SELECT * FROM active_mutes WHERE guild_id=? ORDER BY expires_at", params![guild_id])
    }
    pub fn verification(&self, guild_id: &str) -> Result<VerificationConfig> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        get(&c, "SELECT * FROM verification_config WHERE guild_id=?", params![guild_id])?.ok_or_else(|| anyhow!("verification config missing"))
    }
    /// Read-only lookup for hot paths (no ensure_guild, no writes); `None` when the guild has no row yet.
    pub fn verification_lookup(&self, guild_id: &str) -> Result<Option<VerificationConfig>> {
        get(&self.conn(), "SELECT * FROM verification_config WHERE guild_id=?", params![guild_id])
    }
    pub fn update_verification(&self, guild_id: &str, f: &VerificationUpdate) -> Result<()> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        let mut sets = Vec::new();
        push(&mut sets, "enabled", f.enabled);
        push(&mut sets, "role_id", f.role_id.clone());
        push(&mut sets, "title", f.title.clone());
        push(&mut sets, "description", f.description.clone());
        push(&mut sets, "color", f.color);
        push(&mut sets, "button_text", f.button_text.clone());
        push(&mut sets, "channel_id", f.channel_id.clone());
        push(&mut sets, "message_id", f.message_id.clone());
        if sets.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "UPDATE verification_config SET {},updated_at=? WHERE guild_id=?",
            sets.iter().map(|(k, _)| format!("{k}=?")).collect::<Vec<_>>().join(",")
        );
        let values = sets.into_iter().map(|(_, v)| v).chain([Value::from(now()), Value::from(guild_id.to_string())]);
        c.execute(&sql, params_from_iter(values))?;
        Ok(())
    }
    // ---- log routing (see services::log_routing) ----

    pub fn log_hub(&self) -> Result<Option<String>> {
        Ok(self.conn().query_row("SELECT guild_id FROM log_hub WHERE id = 1", [], |r| r.get(0)).ok())
    }
    /// Moving the log server drops the old one's server-specific channels: they were channels in it.
    pub fn set_log_hub(&self, guild_id: Option<&str>) -> Result<()> {
        let c = self.conn();
        c.execute("DELETE FROM log_routes", [])?;
        match guild_id {
            Some(g) => c.execute(
                "INSERT INTO log_hub(id, guild_id, updated_at) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET guild_id = ?1, updated_at = ?2",
                params![g, now()],
            )?,
            None => c.execute("DELETE FROM log_hub", [])?,
        };
        Ok(())
    }
    pub fn log_route(&self, source_guild_id: &str, kind: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT channel_id FROM log_routes WHERE source_guild_id = ? AND kind = ?", params![source_guild_id, kind], |r| r.get(0))
            .ok())
    }
    pub fn set_log_route(&self, source_guild_id: &str, kind: &str, channel_id: Option<&str>) -> Result<()> {
        let c = self.conn();
        match channel_id {
            Some(ch) => c.execute(
                "INSERT INTO log_routes(source_guild_id, kind, channel_id) VALUES (?1, ?2, ?3) ON CONFLICT(source_guild_id, kind) DO UPDATE SET channel_id = ?3",
                params![source_guild_id, kind, ch],
            )?,
            None => c.execute("DELETE FROM log_routes WHERE source_guild_id = ? AND kind = ?", params![source_guild_id, kind])?,
        };
        Ok(())
    }
    /// Servers whose owner chose global mode.
    pub fn global_guilds(&self) -> Result<Vec<String>> {
        let c = self.conn();
        let mut stmt = c.prepare("SELECT guild_id FROM guild_settings WHERE log_mode = 'global' ORDER BY guild_id")?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }
    /// A deleted channel stops being any server's log channel or server-specific channel.
    pub fn forget_log_channel(&self, channel_id: &str) -> Result<()> {
        let c = self.conn();
        for column in ["log_channel_id", "member_log_channel_id"] {
            c.execute(&format!("UPDATE guild_settings SET {column} = NULL WHERE {column} = ?"), params![channel_id])?;
        }
        c.execute("DELETE FROM log_routes WHERE channel_id = ?", params![channel_id])?;
        Ok(())
    }

    pub fn purge_guild(&self, guild_id: &str) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM log_routes WHERE source_guild_id = ?", params![guild_id])?;
        if tx.execute("DELETE FROM log_hub WHERE guild_id = ?", params![guild_id])? > 0 {
            tx.execute("DELETE FROM log_routes", [])?;
        }
        for table in [
            "sent_embed_messages",
            "mute_channel_restorations",
            "active_mutes",
            "active_voice_rooms",
            "voice_room_allowed_users",
            "voice_room_denied_users",
            "voice_room_preferences",
            "guild_settings",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE guild_id=?"), params![guild_id])?;
        }
        // Embed templates belong to the guild: drop them with their schedules (history included), attachments
        // and any tracking rows. Explicit deletes, so this does not depend on foreign_keys being ON.
        const OWNED: &str = "SELECT template_id FROM embed_templates WHERE guild_id=?1";
        tx.execute(&format!("DELETE FROM sent_embed_messages WHERE embed_id IN ({OWNED})"), params![guild_id])?;
        tx.execute(&format!("DELETE FROM scheduled_embed_sends WHERE guild_id=?1 OR template_id IN ({OWNED})"), params![guild_id])?;
        tx.execute(&format!("DELETE FROM embed_attachments WHERE template_id IN ({OWNED})"), params![guild_id])?;
        tx.execute("DELETE FROM embed_templates WHERE guild_id=?", params![guild_id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn backup_settings(&self, guild_id: &str) -> Result<BackupSettings> {
        backup_settings(&self.conn(), guild_id)
    }
    pub fn update_backup_settings(&self, guild_id: &str, fields: &BackupSettingsUpdate) -> Result<()> {
        update_backup_settings(&self.conn(), guild_id, fields)
    }
    pub fn backups(&self, source_guild_id: &str) -> Result<Vec<GuildBackup>> {
        all(&self.conn(), "SELECT * FROM guild_backups WHERE source_guild_id=? ORDER BY created_at DESC", params![source_guild_id])
    }
    /// Manual backups a user created, for the creator fallback once Gaia has left the source guild.
    pub fn manual_backups_created_by(&self, user_id: &str) -> Result<Vec<GuildBackup>> {
        all(&self.conn(), "SELECT * FROM guild_backups WHERE created_by=? AND creation_type='manual' ORDER BY created_at DESC", params![user_id])
    }
    pub fn backup(&self, backup_id: &str) -> Result<Option<GuildBackup>> {
        get(&self.conn(), "SELECT * FROM guild_backups WHERE backup_id=?", params![backup_id])
    }
    /// `assets` defaults to empty.
    pub fn store_backup(&self, backup: &GuildBackup, protected_backup_id: Option<&str>, assets: Option<&[GuildBackupAsset]>) -> Result<()> {
        let mut c = self.conn();
        // Backups can be created before the guildCreate listener has finished.
        // Establish the parent configuration row first so every related write is valid.
        ensure_guild(&c, &backup.source_guild_id)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let b = backup;
        tx.execute(
            "INSERT INTO guild_backups(backup_id,source_guild_id,source_guild_name,created_at,created_by,creation_type,snapshot_schema_version,snapshot_json,snapshot_hash,snapshot_size,role_count,channel_count,member_count,overwrite_count,source_removed_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                b.backup_id, b.source_guild_id, b.source_guild_name, b.created_at, b.created_by, b.creation_type, b.snapshot_schema_version,
                b.snapshot_json, b.snapshot_hash, b.snapshot_size, b.role_count, b.channel_count, b.member_count, b.overwrite_count, b.source_removed_at
            ],
        )?;
        for a in assets.unwrap_or(&[]) {
            tx.execute(
                "INSERT INTO guild_backup_assets(backup_id,kind,content_type,size_bytes,sha256,data,created_at) VALUES (?,?,?,?,?,?,?)",
                params![a.backup_id, a.kind, a.content_type, a.size_bytes, a.sha256, a.data, a.created_at],
            )?;
        }
        let retained = strings(&tx, "SELECT backup_id FROM guild_backups WHERE source_guild_id=? ORDER BY created_at DESC", params![b.source_guild_id])?;
        // Insertion-ordered set, like the TS `Set`.
        let mut keep: Vec<String> = retained.iter().take(2).cloned().collect();
        if let Some(protected) = protected_backup_id.filter(|p| !p.is_empty()) {
            if !keep.iter().any(|k| k == protected) {
                keep.push(protected.to_string());
            }
            while keep.len() > 2 {
                let candidate = retained.iter().find(|id| **id != b.backup_id && *id != protected && keep.contains(id));
                match candidate {
                    Some(cand) => keep.retain(|k| k != cand),
                    None => break,
                }
            }
        }
        for old in &retained {
            if !keep.contains(old) {
                tx.execute(
                    "DELETE FROM guild_backups WHERE backup_id=? AND NOT EXISTS (SELECT 1 FROM restore_operations WHERE backup_id=?)",
                    params![old, old],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn backup_assets(&self, backup_id: &str) -> Result<Vec<GuildBackupAsset>> {
        let c = self.conn();
        let mut stmt = c.prepare("SELECT * FROM guild_backup_assets WHERE backup_id=? ORDER BY kind")?;
        let rows = stmt.query_map(params![backup_id], asset_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn delete_backup(&self, backup_id: &str, source_guild_id: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM guild_backups WHERE backup_id=? AND source_guild_id=?", params![backup_id, source_guild_id])? == 1)
    }
    pub fn mark_backup_source_removed(&self, guild_id: &str) -> Result<()> {
        let c = self.conn();
        let stamp = now();
        update_backup_settings(
            &c,
            guild_id,
            &BackupSettingsUpdate { source_removed_at: Some(Some(stamp.clone())), automatic_enabled: Some(0), ..Default::default() },
        )?;
        c.execute("UPDATE guild_backups SET source_removed_at=? WHERE source_guild_id=?", params![stamp, guild_id])?;
        Ok(())
    }
    pub fn mark_backup_source_available(&self, guild_id: &str) -> Result<()> {
        let c = self.conn();
        update_backup_settings(&c, guild_id, &BackupSettingsUpdate { source_removed_at: Some(None), ..Default::default() })?;
        c.execute("UPDATE guild_backups SET source_removed_at=NULL WHERE source_guild_id=?", params![guild_id])?;
        Ok(())
    }
    pub fn purge_expired_backups(&self, at: &str) -> Result<()> {
        self.conn().execute("DELETE FROM guild_backups WHERE source_removed_at IS NOT NULL AND source_removed_at <= ?", params![at])?;
        Ok(())
    }
    pub fn create_restore_operation(&self, o: &NewRestoreOperation) -> Result<()> {
        self.conn().execute(
            "INSERT INTO restore_operations(restore_id,backup_id,destination_guild_id,requested_by,executor_user_id,started_at,status,current_phase,progress_json,errors_json) VALUES (?,?,?,?,?,?,?,?,?,?)",
            params![
                o.restore_id,
                o.backup_id,
                o.destination_guild_id,
                o.requested_by,
                o.executor_user_id.as_deref().unwrap_or(&o.requested_by),
                o.started_at.clone().unwrap_or_else(now),
                o.status,
                o.current_phase,
                o.progress_json,
                o.errors_json
            ],
        )?;
        Ok(())
    }
    pub fn update_restore_operation(&self, restore_id: &str, f: &RestoreOperationUpdate) -> Result<()> {
        let mut sets = Vec::new();
        push(&mut sets, "status", f.status.clone());
        push(&mut sets, "current_phase", f.current_phase.clone());
        push(&mut sets, "progress_json", f.progress_json.clone());
        push(&mut sets, "errors_json", f.errors_json.clone());
        if f.completed == Some(true) {
            sets.push(("completed_at", Value::from(now())));
        }
        if sets.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "UPDATE restore_operations SET {} WHERE restore_id=?",
            sets.iter().map(|(k, _)| format!("{k}=?")).collect::<Vec<_>>().join(",")
        );
        let values = sets.into_iter().map(|(_, v)| v).chain([Value::from(restore_id.to_string())]);
        self.conn().execute(&sql, params_from_iter(values))?;
        Ok(())
    }
    pub fn claim_completion_dm(&self, restore_id: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE restore_operations SET completion_dm_status='attempting',completion_dm_attempted_at=? WHERE restore_id=? AND completed_at IS NOT NULL AND completion_dm_status='pending'",
            params![now(), restore_id],
        )? == 1)
    }
    pub fn complete_completion_dm(&self, restore_id: &str, sent: bool, failure_reason: Option<&str>) -> Result<()> {
        let reason: Option<String> = if sent {
            None
        } else {
            // TS slices 300 UTF-16 units; chars are the closest safe equivalent.
            Some(failure_reason.map(|r| r.chars().take(300).collect()).unwrap_or_else(|| "Delivery failed".into()))
        };
        self.conn().execute(
            "UPDATE restore_operations SET completion_dm_status=?,completion_dm_sent_at=?,completion_dm_failure_reason=? WHERE restore_id=?",
            params![if sent { "sent" } else { "failed" }, if sent { Some(now()) } else { None }, reason, restore_id],
        )?;
        Ok(())
    }
    pub fn interrupt_running_restores(&self) -> Result<()> {
        self.conn().execute("UPDATE restore_operations SET status='interrupted',completed_at=? WHERE status IN ('planned','running')", params![now()])?;
        Ok(())
    }
    pub fn command_sync_state(&self, application_id: &str) -> Result<Option<CommandSyncState>> {
        get(&self.conn(), "SELECT * FROM command_sync_state WHERE application_id=?", params![application_id])
    }
    pub fn save_command_sync_state(&self, application_id: &str, fingerprint: &str, cleanup_complete: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO command_sync_state(application_id,schema_fingerprint,guild_cleanup_complete,updated_at) VALUES (?,?,?,?) ON CONFLICT(application_id) DO UPDATE SET schema_fingerprint=excluded.schema_fingerprint,guild_cleanup_complete=excluded.guild_cleanup_complete,updated_at=excluded.updated_at",
            params![application_id, fingerprint, cleanup_complete as i64, now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> GaiaRepository {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::db::migrate(&mut db).unwrap();
        GaiaRepository::new(db)
    }
    fn room(g: &str, c: &str, o: &str) -> ActiveRoom {
        ActiveRoom { guild_id: g.into(), channel_id: c.into(), owner_id: o.into(), ..Default::default() }
    }

    // ---- guild isolation and constraints ----
    #[test]
    fn keeps_feature_and_autorole_settings_isolated() {
        let r = repo();
        r.set_autorole("guild-a", "user_role_id", Some("role-a")).unwrap();
        r.toggle("guild-a", "voice_rooms_enabled").unwrap();
        assert_eq!(r.autoroles("guild-b").unwrap(), Autoroles::default());
        assert_eq!(r.settings("guild-b").unwrap().voice_rooms_enabled, 1);
        assert_eq!(r.settings("guild-a").unwrap().voice_rooms_enabled, 0);
        assert!(r.update_setting("guild-a", "guild_id; DROP", Value::Integer(1)).is_err());
    }
    #[test]
    fn one_active_room_per_owner_per_guild() {
        let r = repo();
        assert!(r.create_room(&room("a", "c1", "u")).unwrap());
        assert!(!r.create_room(&room("a", "c2", "u")).unwrap());
        assert!(r.create_room(&room("b", "c3", "u")).unwrap());
    }
    #[test]
    fn persists_preferences_and_access() {
        let r = repo();
        r.update_preference("a", "owner", "user_limit", Value::Integer(25)).unwrap();
        r.set_access("a", "owner", "guest", "allowed", true).unwrap();
        r.set_access("a", "owner", "guest", "denied", true).unwrap();
        assert_eq!(r.preference("a", "owner").unwrap().user_limit, 25);
        assert!(r.access("a", "owner").unwrap().denied.contains(&"guest".to_string()));
    }
    #[test]
    fn stores_staff_per_guild() {
        let r = repo();
        r.add_staff("a", "u", "admin").unwrap();
        assert!(r.is_staff("a", "u").unwrap());
        assert!(!r.is_staff("b", "u").unwrap());
        assert!(r.remove_staff("a", "u").unwrap());
        assert!(!r.is_staff("a", "u").unwrap());
        r.replace_staff("a", &["x".into(), "y".into(), "x".into()], "admin").unwrap();
        assert_eq!(r.staff_count("a").unwrap(), 2);
    }
    #[test]
    fn transfers_only_when_owner_matches() {
        let r = repo();
        r.create_room(&room("a", "room", "old")).unwrap();
        assert!(!r.transfer_room("a", "room", "not-old", "new").unwrap());
        assert!(r.transfer_room("a", "room", "old", "new").unwrap());
        assert_eq!(r.active_room("a", "room").unwrap().unwrap().owner_id, "new");
    }
    #[test]
    fn resets_only_current_guild_preferences() {
        let r = repo();
        r.update_preference("a", "owner", "is_locked", Value::Integer(1)).unwrap();
        r.update_preference("a", "owner", "user_limit", Value::Integer(10)).unwrap();
        r.set_access("a", "owner", "guest", "allowed", true).unwrap();
        r.update_preference("b", "owner", "user_limit", Value::Integer(7)).unwrap();
        r.reset_preference("a", "owner").unwrap();
        let p = r.preference("a", "owner").unwrap();
        assert_eq!((p.is_locked, p.user_limit), (0, 0));
        assert_eq!(r.access("a", "owner").unwrap(), RoomAccess::default());
        assert_eq!(r.preference("b", "owner").unwrap().user_limit, 7);
    }
    #[test]
    fn custom_room_names_guild_specific() {
        let r = repo();
        r.update_preference("a", "owner", "custom_name", Value::Text("Gaming".into())).unwrap();
        assert_eq!(r.preference("a", "owner").unwrap().custom_name.as_deref(), Some("Gaming"));
        assert_eq!(r.preference("b", "owner").unwrap().custom_name, None);
        r.reset_preference("a", "owner").unwrap();
        assert_eq!(r.preference("a", "owner").unwrap().custom_name, None);
    }
    #[test]
    fn verification_defaults_and_isolation() {
        let r = repo();
        assert_eq!(r.verification("a").unwrap().enabled, 0);
        assert_eq!(r.verification("a").unwrap().color, i64::from(crate::discord::GAIA_PURPLE));
        r.update_verification("a", &VerificationUpdate { enabled: Some(1), role_id: Some(Some("verified".into())), ..Default::default() }).unwrap();
        let a = r.verification("a").unwrap();
        assert_eq!((a.enabled, a.role_id.as_deref()), (1, Some("verified")));
        let b = r.verification("b").unwrap();
        assert_eq!((b.enabled, b.role_id), (0, None));
    }
    #[test]
    fn verification_deployment_persists_and_clears() {
        let r = repo();
        r.update_verification(
            "a",
            &VerificationUpdate {
                channel_id: Some(Some("verify-channel".into())),
                message_id: Some(Some("verify-message".into())),
                title: Some("Welcome".into()),
                description: Some("Read the rules".into()),
                button_text: Some("Verify now".into()),
                color: Some(0x57f287),
                ..Default::default()
            },
        )
        .unwrap();
        r.update_verification("a", &VerificationUpdate { title: Some("Updated title".into()), button_text: Some("Join".into()), ..Default::default() }).unwrap();
        let v = r.verification("a").unwrap();
        assert_eq!(v.channel_id.as_deref(), Some("verify-channel"));
        assert_eq!(v.message_id.as_deref(), Some("verify-message"));
        assert_eq!((v.title.as_str(), v.button_text.as_str()), ("Updated title", "Join"));
        r.update_verification("a", &VerificationUpdate { channel_id: Some(None), message_id: Some(None), ..Default::default() }).unwrap();
        let v = r.verification("a").unwrap();
        assert_eq!((v.channel_id, v.message_id, v.title.as_str()), (None, None, "Updated title"));
    }
    #[test]
    fn purges_only_removed_guild() {
        let r = repo();
        r.set_autorole("a", "user_role_id", Some("role-a")).unwrap();
        r.set_autorole("b", "user_role_id", Some("role-b")).unwrap();
        r.save_embed("a", "Mine", r#"{"content":"a"}"#, "admin", None).unwrap();
        r.save_embed("b", "Mine", r#"{"content":"b"}"#, "admin", None).unwrap();
        let t = r.embed("a", "Mine").unwrap().unwrap().template_id;
        r.save_attachment("a", &attachment(&t, "x.png"), 10, 1 << 20).unwrap();
        r.create_schedule(&schedule("s", &t, "a")).unwrap();
        r.track_sent_embed(&t, "a", "c", "m").unwrap();
        r.purge_guild("a").unwrap();
        assert_eq!(r.autoroles("b").unwrap(), Autoroles { user_role_id: Some("role-b".into()), bot_role_id: None });
        assert!(r.embed("a", "Mine").unwrap().is_none(), "templates belong to the removed guild");
        assert!(r.schedules_for_template(&t, Some(&["scheduled", "sent"])).unwrap().is_empty());
        assert_eq!(r.embed("b", "Mine").unwrap().unwrap().payload_json, r#"{"content":"b"}"#);
        assert_eq!(r.autoroles("a").unwrap(), Autoroles::default());
        let c = r.conn();
        for table in ["embed_attachments", "sent_embed_messages"] {
            assert_eq!(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |x| x.get::<_, i64>(0)).unwrap(), 0, "{table}");
        }
    }

    // ---- guild backup persistence ----
    fn backup(id: &str, guild: &str, created_at: &str) -> GuildBackup {
        let snapshot = format!(r#"{{"schemaVersion":1,"guild":{{"id":"{guild}","name":"Source"}},"roles":[],"channels":[],"members":[],"cookie":{{}}}}"#);
        GuildBackup {
            backup_id: id.into(),
            source_guild_id: guild.into(),
            source_guild_name: "Source".into(),
            created_at: created_at.into(),
            created_by: Some("admin".into()),
            creation_type: "manual".into(),
            snapshot_schema_version: 1,
            snapshot_size: snapshot.len() as i64,
            snapshot_json: snapshot,
            ..Default::default()
        }
    }
    fn restore(id: &str, backup_id: &str, status: &str) -> NewRestoreOperation {
        NewRestoreOperation {
            restore_id: id.into(),
            backup_id: backup_id.into(),
            destination_guild_id: "b".into(),
            requested_by: "admin".into(),
            status: status.into(),
            current_phase: "complete".into(),
            progress_json: "{}".into(),
            errors_json: "[]".into(),
            ..Default::default()
        }
    }
    fn ids(r: &GaiaRepository, g: &str) -> Vec<String> {
        r.backups(g).unwrap().into_iter().map(|b| b.backup_id).collect()
    }
    #[test]
    fn rotates_after_third_backup() {
        let r = repo();
        r.store_backup(&backup("one", "a", "2026-01-01T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("two", "a", "2026-01-02T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("three", "a", "2026-01-03T00:00:00.000Z"), None, None).unwrap();
        assert_eq!(ids(&r, "a"), ["three", "two"]);
    }
    #[test]
    fn retains_restore_referenced_history() {
        let r = repo();
        r.store_backup(&backup("one", "a", "2026-01-01T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("two", "a", "2026-01-02T00:00:00.000Z"), None, None).unwrap();
        r.create_restore_operation(&restore("restore-one", "one", "completed")).unwrap();
        r.store_backup(&backup("three", "a", "2026-01-03T00:00:00.000Z"), None, None).unwrap();
        assert_eq!(ids(&r, "a"), ["three", "two", "one"]);
    }
    #[test]
    fn retains_protected_source_backup() {
        let r = repo();
        r.store_backup(&backup("source", "a", "2026-01-01T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("other", "a", "2026-01-02T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("safety", "a", "2026-01-03T00:00:00.000Z"), Some("source"), None).unwrap();
        assert_eq!(ids(&r, "a"), ["safety", "source"]);
    }
    fn restore_row(r: &GaiaRepository, id: &str) -> (String, Option<String>) {
        r.conn()
            .query_row("SELECT status,completed_at FROM restore_operations WHERE restore_id=?", [id], |x| Ok((x.get(0)?, x.get(1)?)))
            .unwrap()
    }
    #[test]
    fn interrupts_running_restores() {
        let r = repo();
        r.store_backup(&backup("source", "a", "2026-01-01T00:00:00.000Z"), None, None).unwrap();
        r.create_restore_operation(&restore("restore", "source", "running")).unwrap();
        r.interrupt_running_restores().unwrap();
        assert_eq!(restore_row(&r, "restore").0, "interrupted");
    }
    #[test]
    fn persists_failed_partial_terminal_state() {
        let r = repo();
        r.store_backup(&backup("source", "a", "2026-01-01T00:00:00.000Z"), None, None).unwrap();
        r.create_restore_operation(&restore("partial", "source", "planned")).unwrap();
        r.update_restore_operation(
            "partial",
            &RestoreOperationUpdate { status: Some("failed_partial".into()), current_phase: Some("channels".into()), completed: Some(true), ..Default::default() },
        )
        .unwrap();
        let (status, completed_at) = restore_row(&r, "partial");
        assert_eq!(status, "failed_partial");
        assert!(completed_at.is_some());
        assert!(r.claim_completion_dm("partial").unwrap());
        assert!(!r.claim_completion_dm("partial").unwrap());
        r.complete_completion_dm("partial", false, None).unwrap();
    }
    #[test]
    fn backup_assets_round_trip_bytes() {
        let r = repo();
        let asset = GuildBackupAsset {
            backup_id: "source".into(),
            kind: "icon".into(),
            content_type: "image/png".into(),
            size_bytes: 3,
            sha256: "x".into(),
            data: vec![1, 2, 3],
            created_at: "2026-01-01T00:00:00.000Z".into(),
        };
        r.store_backup(&backup("source", "a", "2026-01-01T00:00:00.000Z"), None, Some(&[asset])).unwrap();
        assert_eq!(r.backup_assets("source").unwrap()[0].data, vec![1, 2, 3]);
    }

    // ---- per-guild embeds ----
    fn attachment(template_id: &str, filename: &str) -> EmbedAttachment {
        EmbedAttachment {
            id: uuid::Uuid::new_v4().to_string(),
            template_id: template_id.into(),
            filename: filename.into(),
            content_type: "image/png".into(),
            size_bytes: 5,
            sha256: "h".into(),
            data: b"image".to_vec(),
            created_by_id: "admin".into(),
            created_at: now(),
        }
    }
    fn schedule(id: &str, template_id: &str, guild_id: &str) -> ScheduledEmbedSend {
        let stamp = now();
        ScheduledEmbedSend {
            id: id.into(),
            template_id: template_id.into(),
            guild_id: guild_id.into(),
            channel_id: "c".into(),
            scheduled_for_utc: stamp.clone(),
            timezone: "Europe/Ljubljana".into(),
            created_by_user_id: "admin".into(),
            created_at: stamp.clone(),
            updated_by_user_id: "admin".into(),
            updated_at: stamp,
            status: "scheduled".into(),
            template_name_snapshot: "Soon".into(),
            ..Default::default()
        }
    }
    #[test]
    fn case_insensitive_unique_embed_names_per_guild() {
        let r = repo();
        assert!(r.save_embed("a", "Welcome", r#"{"content":"hi"}"#, "admin", None).unwrap());
        assert!(!r.save_embed("a", "welcome", r#"{"content":"bye"}"#, "admin", None).unwrap());
        assert_eq!(r.embed("a", "WELCOME").unwrap().unwrap().name, "Welcome");
        assert!(r.save_embed("a", "welcome", r#"{"content":"bye"}"#, "admin", Some(true)).unwrap());
        assert_eq!(r.embed("a", "Welcome").unwrap().unwrap().payload_json, r#"{"content":"bye"}"#);
        assert_eq!(r.embeds("a", None, None).unwrap(), ["Welcome"]);
        // Another guild may use the same name independently.
        assert!(r.save_embed("b", "WELCOME", r#"{"content":"b"}"#, "admin", None).unwrap());
        assert_eq!(r.embed("a", "Welcome").unwrap().unwrap().payload_json, r#"{"content":"bye"}"#);
        assert_eq!(r.embeds("b", None, None).unwrap(), ["WELCOME"]);
    }
    #[test]
    fn guilds_cannot_see_or_touch_each_others_templates() {
        let r = repo();
        r.save_embed("b", "Secret", r#"{"content":"b only"}"#, "admin", None).unwrap();
        let t = r.embed("b", "Secret").unwrap().unwrap().template_id;
        r.save_attachment("b", &attachment(&t, "x.png"), 10, 1 << 20).unwrap();
        assert!(r.embed("a", "Secret").unwrap().is_none());
        assert!(r.embed_by_id("a", &t).unwrap().is_none());
        assert!(r.embeds("a", None, None).unwrap().is_empty());
        assert!(r.attachments("a", &t).unwrap().is_empty());
        assert!(r.save_attachment("a", &attachment(&t, "y.png"), 10, 1 << 20).is_err());
        assert!(!r.delete_attachment("a", &t, "x.png").unwrap());
        assert!(r.create_schedule(&schedule("s", &t, "a")).is_err());
        r.track_sent_embed(&t, "a", "c", "m").unwrap();
        assert!(r.sent_embeds(&t, "a").unwrap().is_empty());
        // Saving under guild A's name never overwrites guild B's template.
        assert!(r.save_embed("a", "Secret", "{}", "admin", Some(true)).unwrap());
        assert_eq!(r.embed("b", "Secret").unwrap().unwrap().payload_json, r#"{"content":"b only"}"#);
        assert_eq!(r.attachments("b", &t).unwrap().len(), 1);
        r.create_schedule(&schedule("s", &t, "b")).unwrap();
        assert!(r.schedule("a", "s").unwrap().is_none());
        assert!(r.schedule("b", "s").unwrap().is_some());
    }
    #[test]
    fn legacy_unowned_templates_are_hidden() {
        let r = repo();
        r.conn()
            .execute_batch(
                "INSERT INTO embed_templates(template_id,guild_id,name,payload_json,created_by_id,created_at,updated_at) VALUES ('old',NULL,'Old','{}','u','t','t')",
            )
            .unwrap();
        assert!(r.embeds("a", None, None).unwrap().is_empty());
        assert!(r.embed_by_id("a", "old").unwrap().is_none());
        assert!(r.save_embed("a", "Old", "{}", "admin", None).unwrap(), "legacy names do not block a guild");
    }
    #[test]
    fn tracks_sent_instances_per_guild() {
        // TS "tracks sent instances by template and guild without cross-guild leakage".
        let r = repo();
        r.save_embed("a", "Rules", r#"{"content":"rules"}"#, "admin", None).unwrap();
        r.save_embed("b", "Rules", r#"{"content":"rules"}"#, "admin", None).unwrap();
        let ta = r.embed("a", "Rules").unwrap().unwrap().template_id;
        let tb = r.embed("b", "Rules").unwrap().unwrap().template_id;
        r.track_sent_embed(&ta, "a", "channel-a", "message-a").unwrap();
        r.track_sent_embed(&tb, "b", "channel-b", "message-b").unwrap();
        let a = r.sent_embeds(&ta, "a").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].channel_id.as_str(), a[0].message_id.as_str()), ("channel-a", "message-a"));
        let b = r.sent_embeds(&tb, "b").unwrap();
        assert_eq!((b.len(), b[0].message_id.as_str()), (1, "message-b"));
        assert!(r.sent_embeds(&ta, "b").unwrap().is_empty());
    }
    #[test]
    fn stores_attachment_bytes() {
        let r = repo();
        r.save_embed("a", "Image", "{}", "admin", None).unwrap();
        let t = r.embed("a", "Image").unwrap().unwrap().template_id;
        let mut a = attachment(&t, "card.png");
        r.save_attachment("a", &a, 10, 1 << 20).unwrap();
        a.id = "att2".into();
        a.data = b"other".to_vec();
        r.save_attachment("a", &a, 1, 1 << 20).unwrap(); // upsert on (template_id, filename); a replacement fits the cap
        assert_eq!(r.attachments("a", &t).unwrap().len(), 1);
        assert_eq!(r.attachment("a", &t, "card.png").unwrap().unwrap().data, b"other");
        assert!(r.delete_attachment("a", &t, "card.png").unwrap());
        assert!(r.attachment("a", &t, "card.png").unwrap().is_none());
    }
    #[test]
    fn enforces_attachment_count_and_total_caps_on_save() {
        let r = repo();
        r.save_embed("a", "Image", "{}", "admin", None).unwrap();
        let t = r.embed("a", "Image").unwrap().unwrap().template_id;
        r.save_attachment("a", &attachment(&t, "1.png"), 2, 10).unwrap();
        r.save_attachment("a", &attachment(&t, "2.png"), 2, 10).unwrap();
        let error = r.save_attachment("a", &attachment(&t, "3.png"), 2, 100).unwrap_err();
        assert!(error.to_string().contains("at most 2 attachments"));
        r.delete_attachment("a", &t, "2.png").unwrap();
        let error = r.save_attachment("a", &attachment(&t, "3.png"), 5, 9).unwrap_err();
        assert!(error.to_string().contains("exceed"));
        assert_eq!(r.attachments("a", &t).unwrap().len(), 1);
    }
    #[test]
    fn claims_due_schedule_once_and_keeps_history() {
        // TS "claims a due schedule once and keeps history without deleting it".
        let r = repo();
        r.save_embed("a", "Soon", r#"{"content":"hi"}"#, "admin", None).unwrap();
        let t = r.embed("a", "Soon").unwrap().unwrap().template_id;
        let s = schedule("schedule", &t, "a");
        let stamp = s.scheduled_for_utc.clone();
        r.create_schedule(&s).unwrap();
        assert_eq!(r.schedules_for_template(&t, None).unwrap().len(), 1);
        let claimed = r.claim_due_schedule(&stamp).unwrap().unwrap();
        assert_eq!((claimed.status.as_str(), claimed.attempt_count), ("processing", 1));
        assert!(r.claim_due_schedule(&stamp).unwrap().is_none());
        assert!(r
            .update_schedule(
                "schedule",
                "scheduler",
                &ScheduleUpdate { status: Some("sent".into()), sent_at: Some(Some(stamp.clone())), sent_message_id: Some(Some("message".into())), ..Default::default() },
            )
            .unwrap());
        let h = r.schedule_history("a", None).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].id.as_str(), h[0].status.as_str(), h[0].sent_message_id.as_deref()), ("schedule", "sent", Some("message")));
        assert!(h[0].mention_json.is_some());
        assert!(!r.update_schedule("schedule", "x", &ScheduleUpdate::default()).unwrap());
    }
    #[test]
    fn create_schedule_persists_the_chosen_mention() {
        let r = repo();
        r.save_embed("a", "Ping", "{}", "admin", None).unwrap();
        let t = r.embed("a", "Ping").unwrap().unwrap().template_id;
        let mention = r#"{"roles":["111"],"everyone":true,"here":false}"#;
        r.create_schedule(&ScheduledEmbedSend { mention_json: Some(mention.into()), ..schedule("s", &t, "a") }).unwrap();
        assert_eq!(r.schedule("a", "s").unwrap().unwrap().mention_json.as_deref(), Some(mention));
    }
    #[test]
    fn verification_lookup_is_read_only() {
        let r = repo();
        assert!(r.verification_lookup("a").unwrap().is_none());
        assert_eq!(r.conn().query_row("SELECT COUNT(*) FROM guild_settings", [], |x| x.get::<_, i64>(0)).unwrap(), 0);
        r.verification("a").unwrap();
        assert_eq!(r.verification_lookup("a").unwrap().unwrap().guild_id, "a");
    }
}
