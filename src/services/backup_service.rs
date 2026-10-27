//! Guild snapshot/restore engine: capture, integrity, rotation, automatic daily backups and
//! destructive full-server restore with progress, safety snapshot and final validation.
use crate::ctx;
use crate::db::repositories::{
    BackupSettings, BackupSettingsUpdate, GaiaRepository, GuildBackup, GuildBackupAsset, NewRestoreOperation,
    RestoreOperationUpdate, VerificationUpdate,
};
use crate::discord::Discord;
use crate::logger;
use crate::services::audit_log_service::{AuditEvent, AuditLogService, AuditSubject};
use crate::services::authorization::is_administrator;
use crate::services::locks::KeyedLock;
use base64::Engine;
use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Europe::Ljubljana;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use serenity::builder::{CreateEmbed, CreateMessage, EditMessage};
use serenity::http::{LightMethod, Request, Route};
use serenity::model::id::{ChannelId, GuildId, MessageId, RoleId, UserId};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;

pub const BACKUP_SCHEMA_VERSION: i64 = 2;
pub const MAX_BACKUP_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_GUILD_ASSET_BYTES: usize = 8 * 1024 * 1024;

const CATEGORY: u64 = 4;
const ANNOUNCEMENT: u64 = 5;
const FORUM: u64 = 15;
const MEDIA: u64 = 16;
/// Category, Text, Voice, Announcement, Stage, Forum, Media.
const SUPPORTED_CHANNEL_TYPES: [u64; 7] = [4, 0, 2, 5, 13, 15, 16];
/// Every SystemChannelFlags bit known to discord.js 14 (1 << 0 ..= 1 << 5).
const SUPPORTED_SYSTEM_CHANNEL_FLAGS: u128 = 0b11_1111;
const ASSET_KINDS: [&str; 4] = ["icon", "banner", "splash", "discovery_splash"];

const ADMINISTRATOR: u64 = 1 << 3;
const MANAGE_CHANNELS: u64 = 1 << 4;
const MANAGE_GUILD: u64 = 1 << 5;
const VIEW_CHANNEL: u64 = 1 << 10;
const CONNECT: u64 = 1 << 20;
const MANAGE_ROLES: u64 = 1 << 28;

/// Persisted snapshot. Discord-shaped parts stay JSON so key presence (`"x" in obj`) and
/// exact values survive round trips; config lives under `gaia`; legacy `cookie` snapshots still load.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    pub guild: Value,
    pub roles: Vec<Value>,
    pub channels: Vec<Value>,
    pub members: Vec<SnapshotMember>,
    #[serde(default, alias = "cookie")]
    pub gaia: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMember {
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "roleIds")]
    pub role_ids: Vec<String>,
}

/// Restore progress callback: (phase, completed, total).
pub type RestoreProgress =
    Arc<dyn Fn(String, Option<i64>, Option<i64>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedOverwrite {
    pub id: String,
    pub kind: u8,
    pub allow: String,
    pub deny: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedOverwrite {
    pub id: String,
    pub kind: u8,
    pub allow: u128,
    pub deny: u128,
}

/// `None` = absent, `Some(None)` = explicit null.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedRoleColors {
    pub primary_color: u32,
    pub secondary_color: Option<Option<u32>>,
    pub tertiary_color: Option<Option<u32>>,
}

#[derive(Clone, Debug)]
pub struct RestoreResult {
    pub restore_id: String,
    /// "completed" | "completed_with_warnings" | "failed_partial" | "failed"
    pub status: String,
    pub warnings: Vec<String>,
    pub critical_failures: Vec<String>,
    pub phase_timings_ms: Vec<(String, i64)>,
    pub safety_backup_id: Option<String>,
    pub roles: i64,
    pub channels: i64,
    pub overwrites: i64,
    pub members: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreWarningGroup {
    pub category: &'static str,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RestorePreview {
    pub backup: GuildBackup,
    pub snapshot: Snapshot,
    pub roles_to_create: i64,
    pub channels_to_create: i64,
    pub overwrites: i64,
    pub member_assignments: i64,
    pub unsupported: Vec<String>,
}

#[derive(Debug)]
struct PartialRestoreError(String);
impl std::fmt::Display for PartialRestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PartialRestoreError {}

#[derive(Debug)]
pub struct BackupAlreadyRunningError;
impl std::fmt::Display for BackupAlreadyRunningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("A backup of this server is currently being created.")
    }
}
impl std::error::Error for BackupAlreadyRunningError {}

#[derive(Debug, Clone)]
pub struct MemberSynchronizationError {
    pub message: String,
    pub rate_limited: bool,
}
impl std::fmt::Display for MemberSynchronizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for MemberSynchronizationError {}

// ---------------------------------------------------------------- JS-semantics helpers

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// JS `String(value)` for JSON values.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Array(items) => items.iter().map(|v| if v.is_null() { String::new() } else { js_string(v) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
        other => other.to_string(),
    }
}

fn sql_value(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(i64::from(*b)),
        Value::Number(n) => n.as_i64().map_or_else(|| Sql::Real(n.as_f64().unwrap_or(0.0)), Sql::Integer),
        Value::String(s) => Sql::Text(s.clone()),
        other => Sql::Text(other.to_string()),
    }
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn str_of(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 { one.into() } else { many.into() }
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn iso_ms(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text).ok().map(|d| d.timestamp_millis())
}

fn error_name(_error: &anyhow::Error) -> &'static str {
    "Error"
}

fn snowflake(text: &str) -> anyhow::Result<u64> {
    text.parse::<u64>().ok().filter(|v| *v != 0).ok_or_else(|| anyhow::anyhow!("Invalid Discord ID {text}"))
}

/// Reduces any failure to a safe operator message: `Discord error <code>: <message>`.
pub fn error_text(error: &anyhow::Error) -> String {
    let (code, message) = match error.downcast_ref::<serenity::Error>() {
        Some(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response))) => {
            (Some(response.error.code.to_string()), response.error.message.clone())
        }
        _ => (None, error.to_string()),
    };
    format_error_text(code, &message)
}

fn error_text_str(message: Option<&str>) -> String {
    format_error_text(None, message.unwrap_or("unknown Discord error"))
}

fn format_error_text(code: Option<String>, message: &str) -> String {
    let message = truncate(&message.replace(['\r', '\n'], " "), 280);
    format!("Discord error{}: {message}", code.map(|c| format!(" {c}")).unwrap_or_default())
}

// ---------------------------------------------------------------- pure snapshot logic

/// Backups store a decimal string for JSON safety; returns the exact bitfield (empty for zero).
pub fn system_channel_flags_from_snapshot(value: &Value) -> anyhow::Result<u64> {
    let serialized = match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => n
            .as_i64()
            .filter(|v| v.unsigned_abs() < (1 << 53))
            .map(|v| v.to_string())
            .or_else(|| n.as_u64().filter(|v| *v < (1 << 53)).map(|v| v.to_string())),
        _ => None,
    };
    let Some(serialized) = serialized.filter(|s| digits(s)) else {
        anyhow::bail!("systemChannelFlags must be a non-negative decimal bitfield.");
    };
    match serialized.parse::<u128>() {
        Ok(bits) if bits & !SUPPORTED_SYSTEM_CHANNEL_FLAGS == 0 => Ok(bits as u64),
        _ => anyhow::bail!("systemChannelFlags contains flags unsupported by this discord.js version."),
    }
}

fn valid_role_color(value: &Value) -> Option<u32> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && (0.0..=f64::from(0xffffff)).contains(&number)).then_some(number as u32)
}

/// Normalizes schema-v1 legacy `color` and schema-v2 enhanced role colors.
pub fn role_colors_from_snapshot(saved: &Value) -> Option<SavedRoleColors> {
    let enhanced = saved.get("colors").filter(|v| v.is_object());
    let primary = enhanced
        .and_then(|e| e.get("primaryColor"))
        .filter(|v| !v.is_null())
        .or_else(|| saved.get("color"))
        .and_then(valid_role_color)?;
    let pick = |key: &str| match enhanced.and_then(|e| e.get(key)) {
        Some(Value::Null) => Some(None),
        Some(value) => valid_role_color(value).map(Some),
        None => None,
    };
    Some(SavedRoleColors { primary_color: primary, secondary_color: pick("secondaryColor"), tertiary_color: pick("tertiaryColor") })
}

fn normalized_bitfield(value: &Value) -> String {
    let text = js_string(value);
    text.parse::<u128>().map(|v| v.to_string()).unwrap_or(text)
}

/// Discord REST role body. `icon` holds the source URL; it is resolved to a data URI on create.
pub fn role_create_options(saved: &Value, include_cosmetics: bool) -> Value {
    let mut options = Map::new();
    options.insert("name".into(), saved.get("name").cloned().unwrap_or(Value::Null));
    options.insert("hoist".into(), json!(saved.get("hoist") == Some(&Value::Bool(true))));
    options.insert("permissions".into(), json!(normalized_bitfield(saved.get("permissions").unwrap_or(&Value::Null))));
    options.insert("mentionable".into(), json!(saved.get("mentionable") == Some(&Value::Bool(true))));
    if !include_cosmetics {
        return Value::Object(options);
    }
    if let Some(colors) = role_colors_from_snapshot(saved) {
        let mut body = Map::new();
        body.insert("primary_color".into(), json!(colors.primary_color));
        if let Some(value) = colors.secondary_color {
            body.insert("secondary_color".into(), json!(value));
        }
        if let Some(value) = colors.tertiary_color {
            body.insert("tertiary_color".into(), json!(value));
        }
        options.insert("colors".into(), Value::Object(body));
    }
    if let Some(emoji) = saved.get("unicodeEmoji").and_then(str_of) {
        options.insert("unicode_emoji".into(), json!(emoji));
    }
    if let Some(icon) = saved.get("iconUrl").and_then(str_of) {
        options.insert("icon".into(), json!(icon));
    }
    Value::Object(options)
}

fn warning_category(warning: &str) -> &'static str {
    let text = warning.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if any(&["managed", "integration"]) {
        "Managed/integration roles"
    } else if any(&["protected channel", "cannot be deleted", "could not be deleted"]) {
        "Protected channels"
    } else if any(&["hierarchy", "highest role", "above gaia"]) {
        "Hierarchy limitations"
    } else if any(&["community", "discord-controlled", "guild feature"]) {
        "Discord-controlled guild features"
    } else if any(&["unsupported", "not captured", "malformed", "cosmetic", "older backup"]) {
        "Unsupported properties"
    } else {
        "Other"
    }
}

/// Keeps final restore UX compact while full warnings remain in restore_operations.
pub fn group_restore_warnings(warnings: &[String]) -> Vec<RestoreWarningGroup> {
    let mut groups: Vec<RestoreWarningGroup> = Vec::new();
    for warning in warnings {
        let category = warning_category(warning);
        match groups.iter_mut().find(|g| g.category == category) {
            Some(group) => group.warnings.push(warning.clone()),
            None => groups.push(RestoreWarningGroup { category, warnings: vec![warning.clone()] }),
        }
    }
    groups
}

fn compact_warning_summary(warnings: &[String]) -> String {
    let groups = group_restore_warnings(warnings);
    if groups.is_empty() {
        return "None".into();
    }
    truncate(&groups.iter().map(|g| format!("{}: {}", g.category, g.warnings.len())).collect::<Vec<_>>().join("\n"), 1_000)
}

fn notable_warnings(warnings: &[String]) -> String {
    let text = truncate(
        &group_restore_warnings(warnings).iter().take(3).map(|g| format!("• {}: {}", g.category, g.warnings[0])).collect::<Vec<_>>().join("\n"),
        1_000,
    );
    if text.is_empty() { "None".into() } else { text }
}

/// Maps only destination IDs. Missing targets are deliberately skipped, never guessed.
pub fn map_permission_overwrites(
    saved: &[SavedOverwrite],
    role_map: &HashMap<String, String>,
    destination_member_ids: &HashSet<String>,
) -> (Vec<MappedOverwrite>, usize) {
    let mut skipped = 0;
    let mapped = saved
        .iter()
        .filter_map(|overwrite| {
            let id = if overwrite.kind == 0 {
                role_map.get(&overwrite.id).cloned()
            } else {
                destination_member_ids.contains(&overwrite.id).then(|| overwrite.id.clone())
            };
            let Some(id) = id.filter(|id| !id.is_empty()) else {
                skipped += 1;
                return None;
            };
            Some(MappedOverwrite {
                id,
                kind: overwrite.kind,
                allow: overwrite.allow.parse().unwrap_or(0),
                deny: overwrite.deny.parse().unwrap_or(0),
            })
        })
        .collect();
    (mapped, skipped)
}

fn saved_overwrites(channel: &Value) -> Vec<SavedOverwrite> {
    channel["overwrites"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|o| SavedOverwrite {
                    id: js_string(&o["id"]),
                    kind: o["type"].as_u64().unwrap_or(0) as u8,
                    allow: js_string(&o["allow"]),
                    deny: js_string(&o["deny"]),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn overwrite_count(channel: &Value) -> usize {
    channel["overwrites"].as_array().map_or(0, Vec::len)
}

fn ljubljana_hour(date: DateTime<Utc>) -> u32 {
    date.with_timezone(&Ljubljana).hour()
}

fn date_key(date: DateTime<Utc>) -> String {
    date.with_timezone(&Ljubljana).format("%Y-%m-%d").to_string()
}

/// One automatic backup per Europe/Ljubljana day, from 03:00 on (missed runs catch up later that day).
pub fn automatic_backup_due(settings: &BackupSettings, now: DateTime<Utc>) -> bool {
    if ljubljana_hour(now) < 3 || settings.automatic_enabled == 0 {
        return false;
    }
    let done_today = settings
        .last_success_at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .is_some_and(|at| date_key(at.with_timezone(&Utc)) == date_key(now));
    !done_today
}

fn validate(snapshot: &Value) -> anyhow::Result<()> {
    let version_ok = snapshot["schemaVersion"].as_i64().is_some_and(|v| v == 1 || v == BACKUP_SCHEMA_VERSION)
        && snapshot["schemaVersion"].is_number();
    let guild = &snapshot["guild"];
    if !version_ok
        || !truthy(&guild["id"])
        || !snapshot["roles"].is_array()
        || !snapshot["channels"].is_array()
        || !snapshot["members"].is_array()
    {
        anyhow::bail!("Backup snapshot schema is invalid.");
    }
    let everyone = guild.get("everyonePermissions").filter(|v| !v.is_null()).cloned().unwrap_or(json!("0"));
    if !digits(&js_string(&everyone)) {
        anyhow::bail!("Backup snapshot permissions are invalid.");
    }
    if let Some(flags) = guild.get("systemChannelFlags") {
        if system_channel_flags_from_snapshot(flags).is_err() {
            anyhow::bail!("Backup snapshot system channel flags are invalid.");
        }
    }
    let mut role_ids = HashSet::new();
    for role in snapshot["roles"].as_array().unwrap() {
        let id = js_string(&role["id"]);
        if !truthy(&role["id"]) || !truthy(&role["name"]) || role_ids.contains(&id) || !digits(&js_string(&role["permissions"])) {
            anyhow::bail!("Backup snapshot role structure is invalid.");
        }
        role_ids.insert(id);
    }
    let channels = snapshot["channels"].as_array().unwrap();
    let channel_ids: HashSet<String> = channels.iter().map(|c| js_string(&c["id"])).collect();
    if channel_ids.len() != channels.len() {
        anyhow::bail!("Backup snapshot channel structure is invalid.");
    }
    for channel in channels {
        let parent = &channel["parentId"];
        let parent_ok = !truthy(parent) || {
            let parent_id = js_string(parent);
            channel_ids.contains(&parent_id)
                && channels.iter().find(|c| js_string(&c["id"]) == parent_id).and_then(|c| c["type"].as_u64()) == Some(CATEGORY)
        };
        if !truthy(&channel["id"])
            || !truthy(&channel["name"])
            || !channel["type"].as_u64().is_some_and(|t| SUPPORTED_CHANNEL_TYPES.contains(&t))
            || !channel["overwrites"].is_array()
            || !parent_ok
        {
            anyhow::bail!("Backup snapshot channel structure is invalid.");
        }
        for overwrite in channel["overwrites"].as_array().unwrap() {
            if !truthy(&overwrite["id"])
                || !matches!(overwrite["type"].as_u64(), Some(0 | 1))
                || !digits(&js_string(&overwrite["allow"]))
                || !digits(&js_string(&overwrite["deny"]))
            {
                anyhow::bail!("Backup snapshot overwrite is invalid.");
            }
        }
    }
    for member in snapshot["members"].as_array().unwrap() {
        let valid = truthy(&member["userId"])
            && member["roleIds"].as_array().is_some_and(|ids| ids.iter().all(|id| role_ids.contains(&js_string(id))));
        if !valid {
            anyhow::bail!("Backup snapshot member assignments are invalid.");
        }
    }
    Ok(())
}

fn parse_snapshot(value: Value) -> anyhow::Result<Snapshot> {
    validate(&value)?;
    serde_json::from_value(value).map_err(|_| anyhow::anyhow!("Backup snapshot is malformed."))
}

/// Verifies version, SHA-256 integrity and structure of a stored backup.
pub fn load(backup: &GuildBackup) -> anyhow::Result<Snapshot> {
    if backup.snapshot_schema_version != 1 && backup.snapshot_schema_version != BACKUP_SCHEMA_VERSION {
        anyhow::bail!("This backup uses an unsupported snapshot version.");
    }
    if sha256_hex(backup.snapshot_json.as_bytes()) != backup.snapshot_hash {
        anyhow::bail!("Backup integrity check failed.");
    }
    let value: Value = serde_json::from_str(&backup.snapshot_json).map_err(|_| anyhow::anyhow!("Backup snapshot is malformed."))?;
    parse_snapshot(value)
}

fn validate_assets(snapshot: &Snapshot, assets: &[GuildBackupAsset]) -> anyhow::Result<()> {
    if snapshot.schema_version < 2 {
        return Ok(());
    }
    let Some(descriptors) = snapshot.guild.get("assets").and_then(Value::as_object) else { return Ok(()) };
    for (kind, descriptor) in descriptors {
        if descriptor["state"] != "captured" {
            continue;
        }
        let asset = assets.iter().rev().find(|a| &a.kind == kind);
        let valid = asset.is_some_and(|asset| {
            Value::String(asset.sha256.clone()) == descriptor["sha256"]
                && descriptor["sizeBytes"].as_i64() == Some(asset.size_bytes)
                && sha256_hex(&asset.data) == asset.sha256
        });
        if !valid {
            anyhow::bail!("Backup {} asset integrity check failed.", kind.replacen('_', " ", 1));
        }
    }
    Ok(())
}

fn assert_reconstructable(snapshot: &Snapshot) -> anyhow::Result<()> {
    if snapshot.channels.iter().any(|c| c["type"].as_u64() == Some(ANNOUNCEMENT)) && !snapshot.guild["communityEnabled"].is_boolean() {
        anyhow::bail!(
            "This older backup did not capture Community state or its required channel mappings, so it cannot safely reconstruct announcement channels. Create a fresh backup with the current Gaia version before a full restore."
        );
    }
    Ok(())
}

/// Discord REST channel-create body from a saved channel.
fn channel_options(saved: &Value, parent_id: Option<&str>) -> Value {
    let kind = saved["type"].as_u64();
    let forum = kind == Some(FORUM) || kind == Some(MEDIA);
    let mut options = Map::new();
    options.insert("name".into(), saved["name"].clone());
    if let Some(parent) = parent_id {
        options.insert("parent_id".into(), json!(parent));
    }
    let present = |key: &str| saved.get(key).filter(|v| !v.is_null()).cloned();
    for (key, api) in [
        ("topic", "topic"),
        ("nsfw", "nsfw"),
        ("rateLimitPerUser", "rate_limit_per_user"),
        ("bitrate", "bitrate"),
        ("userLimit", "user_limit"),
        ("rtcRegion", "rtc_region"),
    ] {
        if let Some(value) = present(key) {
            options.insert(api.into(), value);
        }
    }
    if forum {
        if let Some(value) = present("defaultThreadRateLimitPerUser") {
            options.insert("default_thread_rate_limit_per_user".into(), value);
        }
        if let Some(Value::Array(tags)) = present("availableTags") {
            let tags: Vec<Value> = tags
                .iter()
                .map(|t| json!({ "name": t["name"], "moderated": t["moderated"], "emoji_id": t["emojiId"], "emoji_name": t["emojiName"] }))
                .collect();
            options.insert("available_tags".into(), json!(tags));
        }
        if let Some(emoji) = present("defaultReactionEmoji") {
            options.insert("default_reaction_emoji".into(), json!({ "emoji_id": emoji["emojiId"], "emoji_name": emoji["emojiName"] }));
        }
        if let Some(value) = present("defaultSortOrder") {
            options.insert("default_sort_order".into(), value);
        }
        if let Some(value) = present("defaultForumLayout") {
            options.insert("default_forum_layout".into(), value);
        }
    }
    options.insert("type".into(), saved["type"].clone());
    Value::Object(options)
}

fn human_duration(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    if seconds < 60 { format!("{seconds}s") } else { format!("{}m {}s", seconds / 60, seconds % 60) }
}

fn cdn(path: &str, id: &str, hash: Option<&str>, dynamic: bool) -> Option<String> {
    let hash = hash.filter(|h| !h.is_empty())?;
    let ext = if dynamic && hash.starts_with("a_") { "gif" } else { "webp" };
    Some(format!("https://cdn.discordapp.com/{path}/{id}/{hash}.{ext}?size=4096"))
}

// ---------------------------------------------------------------- live Discord state (REST)

/// Raw Discord REST view of a guild: guild JSON (with roles), channels and the bot member.
struct GuildState {
    id: String,
    raw: Value,
    channels: Vec<Value>,
    me: Option<(String, Vec<String>)>,
}

type Rank = (i64, std::cmp::Reverse<u64>);

impl GuildState {
    fn name(&self) -> String {
        self.raw["name"].as_str().unwrap_or_default().to_string()
    }
    fn roles(&self) -> &[Value] {
        self.raw["roles"].as_array().map_or(&[], Vec::as_slice)
    }
    fn role(&self, id: &str) -> Option<&Value> {
        self.roles().iter().find(|r| r["id"] == id)
    }
    fn channel(&self, id: &str) -> Option<&Value> {
        self.channels.iter().find(|c| c["id"] == id)
    }
    fn features(&self) -> Vec<String> {
        self.raw["features"].as_array().map(|f| f.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
    }
    fn role_permissions(role: &Value) -> u64 {
        role["permissions"].as_str().and_then(|p| p.parse().ok()).unwrap_or(0)
    }
    fn rank(role: &Value) -> Rank {
        (role["position"].as_i64().unwrap_or(0), std::cmp::Reverse(js_string(&role["id"]).parse().unwrap_or(0)))
    }
    fn me_roles(&self) -> Vec<&Value> {
        let Some((_, ids)) = &self.me else { return Vec::new() };
        self.roles().iter().filter(|r| ids.iter().any(|id| r["id"] == id.as_str())).collect()
    }
    fn is_owner(&self) -> bool {
        self.me.as_ref().is_some_and(|(id, _)| self.raw["owner_id"] == id.as_str())
    }
    fn base_permissions(&self) -> u64 {
        if self.me.is_none() {
            return 0;
        }
        if self.is_owner() {
            return u64::MAX;
        }
        let mut perms = self.role(&self.id).map_or(0, Self::role_permissions);
        for role in self.me_roles() {
            perms |= Self::role_permissions(role);
        }
        if perms & ADMINISTRATOR != 0 { u64::MAX } else { perms }
    }
    fn channel_permissions(&self, channel: &Value) -> u64 {
        let base = self.base_permissions();
        if base == u64::MAX {
            return base;
        }
        let Some((me_id, role_ids)) = &self.me else { return 0 };
        let overwrites = channel["permission_overwrites"].as_array().map_or(&[][..], Vec::as_slice);
        let bits = |o: &Value, key: &str| o[key].as_str().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let mut perms = base;
        if let Some(o) = overwrites.iter().find(|o| o["id"] == self.id.as_str()) {
            perms = (perms & !bits(o, "deny")) | bits(o, "allow");
        }
        let (mut allow, mut deny) = (0, 0);
        for o in overwrites.iter().filter(|o| o["type"] == 0 && role_ids.iter().any(|r| o["id"] == r.as_str())) {
            allow |= bits(o, "allow");
            deny |= bits(o, "deny");
        }
        perms = (perms & !deny) | allow;
        if let Some(o) = overwrites.iter().find(|o| o["type"] == 1 && o["id"] == me_id.as_str()) {
            perms = (perms & !bits(o, "deny")) | bits(o, "allow");
        }
        perms
    }
    fn highest(&self) -> Option<&Value> {
        self.me.as_ref()?;
        self.me_roles().into_iter().chain(self.role(&self.id)).max_by_key(|r| Self::rank(r))
    }
    /// discord.js `Role#editable`.
    fn role_editable(&self, role: &Value) -> bool {
        if role["managed"] == true || self.base_permissions() & MANAGE_ROLES == 0 {
            return false;
        }
        self.highest().is_some_and(|h| Self::rank(h) > Self::rank(role))
    }
    /// discord.js `GuildChannel#deletable`.
    fn channel_deletable(&self, channel: &Value) -> bool {
        let id = &channel["id"];
        if self.raw["rules_channel_id"] == *id || self.raw["public_updates_channel_id"] == *id {
            return false;
        }
        if self.is_owner() {
            return true;
        }
        let perms = self.channel_permissions(channel);
        let voice = matches!(channel["type"].as_u64(), Some(2 | 13));
        let needed = VIEW_CHANNEL | MANAGE_CHANNELS | if voice { CONNECT } else { 0 };
        perms & needed == needed
    }
    fn channel_overwrites(channel: &Value) -> &[Value] {
        channel["permission_overwrites"].as_array().map_or(&[], Vec::as_slice)
    }
    /// discord.js `GuildChannel#permissionsLocked`.
    fn permissions_locked(&self, channel: &Value) -> Option<bool> {
        let parent = self.channel(channel["parent_id"].as_str()?)?;
        let (own, theirs) = (Self::channel_overwrites(channel), Self::channel_overwrites(parent));
        Some(
            own.len() == theirs.len()
                && own.iter().all(|o| {
                    theirs.iter().any(|t| t["id"] == o["id"] && js_string(&t["allow"]) == js_string(&o["allow"]) && js_string(&t["deny"]) == js_string(&o["deny"]))
                }),
        )
    }
    fn guild_channel_references(&self, channel_id: &str) -> Vec<&'static str> {
        [
            ("rulesChannel", "rules_channel_id"),
            ("publicUpdatesChannel", "public_updates_channel_id"),
            ("safetyAlertsChannel", "safety_alerts_channel_id"),
            ("systemChannel", "system_channel_id"),
            ("afkChannel", "afk_channel_id"),
            ("widgetChannel", "widget_channel_id"),
        ]
        .into_iter()
        .filter(|(_, key)| self.raw[*key] == channel_id)
        .map(|(label, _)| label)
        .collect()
    }
}

fn is_supported(channel: &Value) -> bool {
    channel["type"].as_u64().is_some_and(|t| SUPPORTED_CHANNEL_TYPES.contains(&t))
}

fn categories_last(channels: &mut [Value]) {
    channels.sort_by_key(|c| c["type"].as_u64() == Some(CATEGORY));
}

#[derive(Clone, Debug)]
struct MemberEntry {
    id: String,
    bot: bool,
    roles: Vec<String>,
}

impl From<&serenity::model::guild::Member> for MemberEntry {
    fn from(member: &serenity::model::guild::Member) -> Self {
        Self { id: member.user.id.to_string(), bot: member.user.bot, roles: member.roles.iter().map(|r| r.to_string()).collect() }
    }
}

type MemberList = Arc<Vec<MemberEntry>>;

struct AcquiredMembers {
    members: MemberList,
    source: &'static str,
    duration_ms: i64,
}

type MemberSync = Arc<OnceCell<Result<MemberList, MemberSynchronizationError>>>;

struct RestoreProgressDm {
    attempted: bool,
    message: Option<(ChannelId, MessageId)>,
    last_edit_at: i64,
    phase: Option<String>,
    phase_started_at: i64,
}

/// Removes a key from an in-progress set when dropped (completion, error or cancellation).
struct ActiveGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    key: String,
}
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.key);
    }
}

/// JS `Map` with insertion order (persisted in progress_json).
#[derive(Default)]
struct OrderedMap {
    order: Vec<String>,
    map: HashMap<String, String>,
}
impl OrderedMap {
    fn set(&mut self, key: &str, value: String) {
        if self.map.insert(key.to_string(), value).is_none() {
            self.order.push(key.to_string());
        }
    }
    fn get(&self, key: &str) -> Option<&String> {
        self.map.get(key)
    }
    fn entries(&self) -> Vec<(String, String)> {
        self.order.iter().map(|k| (k.clone(), self.map[k].clone())).collect()
    }
    fn values(&self) -> HashSet<String> {
        self.map.values().cloned().collect()
    }
}

#[derive(Default, Clone, Copy)]
struct Counts {
    roles: i64,
    channels: i64,
    overwrites: i64,
    members: i64,
}

// ---------------------------------------------------------------- service

pub struct BackupService {
    repo: Arc<GaiaRepository>,
    discord: Discord,
    audit: Option<Arc<AuditLogService>>,
    locks: KeyedLock,
    active_restores: Mutex<HashSet<String>>,
    active_backups: Mutex<HashSet<String>>,
    member_syncs: Mutex<HashMap<String, MemberSync>>,
    member_snapshots: Mutex<HashMap<String, (MemberList, Instant)>>,
    timer: Mutex<Option<JoinHandle<()>>>,
    client: reqwest::Client,
}

impl BackupService {
    pub fn new(repo: Arc<GaiaRepository>, discord: Discord, audit: Option<Arc<AuditLogService>>) -> Self {
        Self {
            repo,
            discord,
            audit,
            locks: KeyedLock::default(),
            active_restores: Mutex::default(),
            active_backups: Mutex::default(),
            member_syncs: Mutex::default(),
            member_snapshots: Mutex::default(),
            timer: Mutex::default(),
            client: reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default(),
        }
    }

    /// Runs the automatic scheduler now and every 30 minutes.
    pub fn start(self: &Arc<Self>) {
        let service = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30 * 60));
            let mut first = true;
            loop {
                interval.tick().await;
                if let Err(error) = service.run_automatic(Utc::now()).await {
                    let message = if first { "initial backup scheduler failed" } else { "backup scheduler failed" };
                    logger::error(message, ctx! {"error" => error_name(&error)});
                }
                first = false;
            }
        });
        if let Some(old) = self.timer.lock().unwrap().replace(handle) {
            old.abort();
        }
    }

    /// Used only for shutdown observability; restore state itself remains durable in SQLite.
    pub fn active_restore_destinations(&self) -> Vec<String> {
        self.active_restores.lock().unwrap().iter().cloned().collect()
    }

    pub fn stop(&self) {
        if let Some(handle) = self.timer.lock().unwrap().take() {
            handle.abort();
        }
    }

    fn record(&self, event: AuditEvent) {
        if let Some(audit) = &self.audit {
            audit.record(event);
        }
    }

    pub async fn run_automatic(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        if ljubljana_hour(now) < 3 {
            return Ok(());
        }
        for guild_id in self.discord.cache.guilds() {
            let key = guild_id.to_string();
            if self.active_restores.lock().unwrap().contains(&key) || self.active_backups.lock().unwrap().contains(&key) {
                continue;
            }
            let settings = self.repo.backup_settings(&key)?;
            if !automatic_backup_due(&settings, now) {
                continue;
            }
            self.repo.update_backup_settings(
                &key,
                &BackupSettingsUpdate { last_attempt_at: Some(Some(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))), ..Default::default() },
            )?;
            if let Err(error) = self.create(guild_id, None, "automatic", None).await {
                logger::warn("automatic backup failed", ctx! {"guildId" => key, "error" => error_name(&error)});
            }
        }
        let cutoff = now - chrono::Duration::days(30);
        self.repo.purge_expired_backups(&cutoff.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))?;
        Ok(())
    }

    /// `kind`: "manual" | "automatic" | "safety".
    pub async fn create(&self, guild_id: GuildId, created_by: Option<&str>, kind: &str, protected_backup_id: Option<&str>) -> anyhow::Result<GuildBackup> {
        let key = guild_id.to_string();
        if kind != "safety" && self.active_restores.lock().unwrap().contains(&key) {
            anyhow::bail!("Backup creation is paused while a full server restore is running.");
        }
        if !self.active_backups.lock().unwrap().insert(key.clone()) {
            return Err(BackupAlreadyRunningError.into());
        }
        let _guard = ActiveGuard { set: &self.active_backups, key: key.clone() };
        let started = Instant::now();
        self.locks
            .run(&format!("backup:{key}"), async {
                let backup_id = uuid::Uuid::new_v4().to_string();
                let member_snapshot = self.acquire_members(guild_id).await?;
                let state = self.fetch_state(guild_id).await?;
                let (snapshot, assets) = self.capture(&state, &backup_id, &member_snapshot.members).await?;
                let snapshot_json = serde_json::to_string(&snapshot)?;
                validate(&serde_json::from_str(&snapshot_json)?)?;
                let snapshot_size = snapshot_json.len();
                if snapshot_size > MAX_BACKUP_BYTES {
                    anyhow::bail!("Snapshot exceeds the {} MB limit.", MAX_BACKUP_BYTES / 1024 / 1024);
                }
                let backup = GuildBackup {
                    backup_id: backup_id.clone(),
                    source_guild_id: key.clone(),
                    source_guild_name: state.name(),
                    created_at: crate::db::now(),
                    created_by: created_by.map(str::to_string),
                    creation_type: kind.to_string(),
                    snapshot_schema_version: BACKUP_SCHEMA_VERSION,
                    snapshot_hash: sha256_hex(snapshot_json.as_bytes()),
                    snapshot_json,
                    snapshot_size: snapshot_size as i64,
                    role_count: snapshot.roles.len() as i64,
                    channel_count: snapshot.channels.len() as i64,
                    member_count: snapshot.members.len() as i64,
                    overwrite_count: snapshot.channels.iter().map(overwrite_count).sum::<usize>() as i64,
                    source_removed_at: None,
                };
                self.repo.store_backup(&backup, protected_backup_id, Some(&assets))?;
                if kind == "automatic" {
                    self.repo.update_backup_settings(
                        &key,
                        &BackupSettingsUpdate { last_success_at: Some(Some(backup.created_at.clone())), ..Default::default() },
                    )?;
                }
                logger::info(
                    "backup snapshot created",
                    ctx! {
                        "guildId" => key,
                        "roles" => backup.role_count,
                        "channels" => backup.channel_count,
                        "members" => backup.member_count,
                        "overwrites" => backup.overwrite_count,
                        "assets" => assets.len(),
                        "snapshotBytes" => snapshot_size,
                        "memberSource" => member_snapshot.source,
                        "memberAcquisitionMs" => member_snapshot.duration_ms,
                        "totalBackupMs" => started.elapsed().as_millis() as i64,
                    },
                );
                if kind == "manual" || kind == "automatic" {
                    self.record(AuditEvent {
                        title: if kind == "manual" { "Manual Backup Created" } else { "Automatic Backup Created" }.into(),
                        category: "Backups".into(),
                        color: "success".into(),
                        priority: if kind == "manual" { "normal" } else { "low" }.into(),
                        guild: Some(AuditSubject::named(key.clone(), backup.source_guild_name.clone())),
                        actor: created_by.map(AuditSubject::id),
                        details: vec![
                            ("Roles".into(), json!(backup.role_count)),
                            ("Channels".into(), json!(backup.channel_count)),
                            ("Overwrites".into(), json!(backup.overwrite_count)),
                            ("Members".into(), json!(backup.member_count)),
                            ("Backup".into(), json!(backup.backup_id)),
                        ],
                        ..Default::default()
                    });
                }
                Ok(backup)
            })
            .await
    }

    async fn is_admin(&self, guild_id: GuildId, user_id: &str) -> bool {
        let Ok(uid) = snowflake(user_id).map(UserId::new) else { return false };
        let cached = self.discord.cache.guild(guild_id).and_then(|g| g.members.get(&uid).cloned());
        let member = match cached {
            Some(member) => member,
            None => match self.discord.http.get_member(guild_id, uid).await {
                Ok(member) => member,
                Err(_) => return false,
            },
        };
        is_administrator(&self.discord, &member)
    }

    pub async fn can_access(&self, backup: &GuildBackup, user_id: &str) -> bool {
        let source = snowflake(&backup.source_guild_id).ok().map(GuildId::new).filter(|id| self.discord.cache.guild(*id).is_some());
        if let Some(source) = source {
            return self.is_admin(source, user_id).await;
        }
        backup.creation_type == "manual" && backup.created_by.as_deref() == Some(user_id)
    }

    fn member_cache_is_complete(&self, guild_id: GuildId, count: usize) -> bool {
        // ponytail: no cached guild means no member_count to compare; the REST listing is then accepted as authoritative.
        self.discord.cache.guild(guild_id).is_none_or(|g| count as u64 >= g.member_count)
    }

    async fn acquire_members(&self, guild_id: GuildId) -> Result<AcquiredMembers, MemberSynchronizationError> {
        let started = Instant::now();
        let key = guild_id.to_string();
        let elapsed = |s: Instant| s.elapsed().as_millis() as i64;
        if let Some((members, expires)) = self.member_snapshots.lock().unwrap().get(&key).cloned() {
            if expires > Instant::now() {
                return Ok(AcquiredMembers { members, source: "cache", duration_ms: elapsed(started) });
            }
        }
        let live: Option<(Vec<MemberEntry>, u64)> =
            self.discord.cache.guild(guild_id).map(|g| (g.members.values().map(MemberEntry::from).collect(), g.member_count));
        if let Some((members, expected)) = live {
            if members.len() as u64 >= expected {
                let members = Arc::new(members);
                self.member_snapshots.lock().unwrap().insert(key, (members.clone(), Instant::now() + Duration::from_secs(30)));
                return Ok(AcquiredMembers { members, source: "cache", duration_ms: elapsed(started) });
            }
        }
        let cell = self.member_syncs.lock().unwrap().entry(key.clone()).or_default().clone();
        let result = cell.get_or_init(|| self.synchronize_members(guild_id)).await.clone();
        {
            let mut syncs = self.member_syncs.lock().unwrap();
            if syncs.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
                syncs.remove(&key);
            }
        }
        result.map(|members| AcquiredMembers { members, source: "synchronized", duration_ms: elapsed(started) })
    }

    async fn fetch_all_members(&self, guild_id: GuildId) -> serenity::Result<Vec<MemberEntry>> {
        let mut all = Vec::new();
        let mut after = None;
        loop {
            let page = self.discord.http.get_guild_members(guild_id, Some(1000), after).await?;
            after = page.last().map(|m| m.user.id.get());
            all.extend(page.iter().map(MemberEntry::from));
            if page.len() < 1000 {
                return Ok(all);
            }
        }
    }

    async fn synchronize_members(&self, guild_id: GuildId) -> Result<MemberList, MemberSynchronizationError> {
        let mut retried = false;
        loop {
            match self.fetch_all_members(guild_id).await {
                Ok(members) => {
                    if !self.member_cache_is_complete(guild_id, members.len()) {
                        return Err(MemberSynchronizationError {
                            message: "Discord member synchronization completed without a complete member dataset.".into(),
                            rate_limited: false,
                        });
                    }
                    let members = Arc::new(members);
                    self.member_snapshots
                        .lock()
                        .unwrap()
                        .insert(guild_id.to_string(), (members.clone(), Instant::now() + Duration::from_secs(30)));
                    return Ok(members);
                }
                Err(error) => {
                    let rate_limited = logger::discord_status(&error) == Some(429);
                    if !retried && rate_limited {
                        retried = true;
                        logger::warn(
                            "backup member synchronization gateway-rate-limited",
                            ctx! {"guildId" => guild_id.to_string(), "retryAfterMs" => 15_000},
                        );
                        tokio::time::sleep(Duration::from_millis(15_000)).await;
                        continue;
                    }
                    let detail = error_text(&anyhow::Error::from(error));
                    logger::error(
                        "backup member synchronization failed",
                        ctx! {"guildId" => guild_id.to_string(), "error" => detail.clone(), "retried" => retried},
                    );
                    return Err(MemberSynchronizationError { message: detail, rate_limited });
                }
            }
        }
    }

    pub async fn available_to(&self, user_id: &str) -> anyhow::Result<Vec<GuildBackup>> {
        let mut result = Vec::new();
        for backup in self.repo.all_backups()? {
            if self.can_access(&backup, user_id).await {
                result.push(backup);
            }
        }
        Ok(result)
    }

    pub async fn delete(&self, backup_id: &str, user_id: &str) -> anyhow::Result<bool> {
        let backup = self.require_access(backup_id, user_id).await?;
        let deleted = self.repo.delete_backup(&backup.backup_id, &backup.source_guild_id)?;
        if deleted {
            self.record(AuditEvent {
                title: "Backup Deleted".into(),
                category: "Backups".into(),
                color: "danger".into(),
                priority: "normal".into(),
                guild: Some(AuditSubject::named(backup.source_guild_id.clone(), backup.source_guild_name.clone())),
                actor: Some(AuditSubject::id(user_id)),
                details: vec![("Backup".into(), json!(backup.backup_id))],
                ..Default::default()
            });
        }
        Ok(deleted)
    }

    pub async fn preview(&self, backup_id: &str, _destination: GuildId, user_id: &str) -> anyhow::Result<RestorePreview> {
        let backup = self.require_access(backup_id, user_id).await?;
        let snapshot = load(&backup)?;
        Ok(RestorePreview {
            roles_to_create: snapshot.roles.len() as i64,
            channels_to_create: snapshot.channels.len() as i64,
            overwrites: backup.overwrite_count,
            member_assignments: snapshot.members.len() as i64,
            unsupported: snapshot.unsupported.clone().unwrap_or_default(),
            backup,
            snapshot,
        })
    }

    pub async fn restore(&self, backup_id: &str, destination: GuildId, user_id: &str, report: Option<RestoreProgress>) -> anyhow::Result<RestoreResult> {
        let key = destination.to_string();
        if self.active_restores.lock().unwrap().contains(&key) {
            anyhow::bail!("Another backup restore is currently modifying this server.");
        }
        if !self.is_admin(destination, user_id).await {
            anyhow::bail!("You need Administrator permission in the destination guild.");
        }
        self.require_access(backup_id, user_id).await?;
        self.active_restores.lock().unwrap().insert(key.clone());
        let _guard = ActiveGuard { set: &self.active_restores, key: key.clone() };
        self.locks.run(&format!("restore:{key}"), self.restore_locked(backup_id, destination, user_id, report)).await
    }

    async fn restore_locked(&self, backup_id: &str, destination: GuildId, user_id: &str, report: Option<RestoreProgress>) -> anyhow::Result<RestoreResult> {
        if !self.is_admin(destination, user_id).await {
            anyhow::bail!("You need Administrator permission in the destination guild.");
        }
        let backup = self.require_access(backup_id, user_id).await?;
        let snapshot = load(&backup)?;
        let assets = self.repo.backup_assets(&backup.backup_id)?;
        let dest_key = destination.to_string();
        if self.repo.backup_settings(&dest_key)?.source_removed_at.is_some() {
            anyhow::bail!("Destination guild is unavailable.");
        }
        let dest_name = match self.discord.cache.guild(destination).map(|g| g.name.clone()) {
            Some(name) => name,
            None => self.fetch_state(destination).await.map(|s| s.name()).unwrap_or_default(),
        };
        let started_at = crate::db::now();
        let started_at_ms = iso_ms(&started_at).unwrap_or_else(now_ms);
        let restore_id = uuid::Uuid::new_v4().to_string();
        let mut run = Run {
            svc: self,
            guild_id: destination,
            key: dest_key.clone(),
            dest_name,
            backup: backup.clone(),
            restore_id: restore_id.clone(),
            user_id: user_id.to_string(),
            started_at_ms,
            last_progress_at_ms: started_at_ms,
            phase_started_at_ms: started_at_ms,
            phase: "safety backup".into(),
            phase_timings: Vec::new(),
            operation: None,
            warnings: snapshot.unsupported.clone().unwrap_or_default(),
            critical: Vec::new(),
            role_map: OrderedMap::default(),
            channel_map: OrderedMap::default(),
            counts: Counts::default(),
            safety_backup_id: None,
            destructive: false,
            unremovable: Vec::new(),
            member_ids: HashSet::new(),
            report,
            dm: RestoreProgressDm { attempted: false, message: None, last_edit_at: 0, phase: None, phase_started_at: started_at_ms },
        };
        run.role_map.set(&js_string(&snapshot.guild["id"]), dest_key.clone());
        let (progress_json, errors_json) = run.operation();
        self.repo.create_restore_operation(&NewRestoreOperation {
            restore_id: restore_id.clone(),
            backup_id: backup.backup_id.clone(),
            destination_guild_id: dest_key.clone(),
            requested_by: user_id.into(),
            executor_user_id: Some(user_id.into()),
            started_at: Some(started_at.clone()),
            status: "planned".into(),
            current_phase: run.phase.clone(),
            progress_json,
            errors_json,
        })?;
        self.record(AuditEvent {
            title: "Restore Started".into(),
            category: "Backups".into(),
            color: "danger".into(),
            priority: "critical".into(),
            guild: Some(AuditSubject::named(dest_key.clone(), run.dest_name.clone())),
            actor: Some(AuditSubject::id(user_id)),
            details: vec![
                ("Restore".into(), json!(restore_id)),
                ("Source Guild".into(), json!(backup.source_guild_name)),
                ("Source Backup".into(), json!(backup.backup_id)),
            ],
            ..Default::default()
        });
        run.dm = self.start_restore_progress_dm(&backup, destination, &run.dest_name, user_id, &restore_id, started_at_ms).await;
        logger::info("[restore] started", ctx! {"restoreId" => restore_id, "guildId" => dest_key, "sourceBackupId" => backup.backup_id});

        let outcome = run.execute(&snapshot, &assets).await;
        let duration = || human_duration(now_ms() - started_at_ms);
        match outcome {
            Ok(result) => {
                self.notify_completion(&run, &snapshot, &started_at, &result, "complete", None).await;
                Ok(result)
            }
            Err(error) => {
                run.close_phase();
                let partial = error.downcast_ref::<PartialRestoreError>().is_some();
                if partial || run.destructive {
                    if !partial {
                        run.critical.push(format!("{} — {}", run.phase, error_text(&error)));
                    }
                    let (progress_json, errors_json) = run.operation();
                    self.repo.update_restore_operation(
                        &restore_id,
                        &RestoreOperationUpdate {
                            status: Some("failed_partial".into()),
                            current_phase: Some(run.phase.clone()),
                            progress_json: Some(progress_json),
                            errors_json: Some(errors_json),
                            completed: Some(true),
                        },
                    )?;
                    let result = run.result("failed_partial", run.critical.clone());
                    run.log_completed(&result.status);
                    self.record(AuditEvent {
                        title: "Restore Failed Partially".into(),
                        category: "Backups".into(),
                        color: "danger".into(),
                        priority: "critical".into(),
                        guild: Some(AuditSubject::named(dest_key.clone(), run.dest_name.clone())),
                        actor: Some(AuditSubject::id(user_id)),
                        reason: Some(run.critical.first().cloned().unwrap_or_else(|| "Restore completed with critical failures.".into())),
                        details: vec![
                            ("Restore".into(), json!(restore_id)),
                            ("Phase".into(), json!(run.phase)),
                            ("Warnings".into(), json!(run.warnings.len())),
                            ("Duration".into(), json!(duration())),
                        ],
                        ..Default::default()
                    });
                    let phase = run.phase.clone();
                    self.notify_completion(&run, &snapshot, &started_at, &result, &phase, None).await;
                    return Ok(result);
                }
                let failure = format!("{} — {}", run.phase, error_text(&error));
                let (progress_json, _) = run.operation();
                let errors_json = serde_json::to_string(&run.warnings.iter().cloned().chain([failure.clone()]).collect::<Vec<_>>())?;
                self.repo.update_restore_operation(
                    &restore_id,
                    &RestoreOperationUpdate {
                        status: Some("failed".into()),
                        current_phase: Some(run.phase.clone()),
                        progress_json: Some(progress_json),
                        errors_json: Some(errors_json),
                        completed: Some(true),
                    },
                )?;
                run.log_completed("failed");
                self.record(AuditEvent {
                    title: "Restore Failed".into(),
                    category: "Backups".into(),
                    color: "danger".into(),
                    priority: "critical".into(),
                    guild: Some(AuditSubject::named(dest_key.clone(), run.dest_name.clone())),
                    actor: Some(AuditSubject::id(user_id)),
                    reason: Some(error_text(&error)),
                    details: vec![("Restore".into(), json!(restore_id)), ("Phase".into(), json!(run.phase)), ("Duration".into(), json!(duration()))],
                    ..Default::default()
                });
                let result = run.result("failed", run.critical.iter().cloned().chain([failure]).collect());
                let phase = run.phase.clone();
                self.notify_completion(&run, &snapshot, &started_at, &result, &phase, Some(&error)).await;
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn restore_progress_embed(
        backup: &GuildBackup,
        destination: GuildId,
        dest_name: &str,
        phase: &str,
        started_at_ms: i64,
        completed: Option<i64>,
        total: Option<i64>,
        phase_started_at: i64,
    ) -> CreateEmbed {
        let now = now_ms();
        let elapsed_ms = (now - started_at_ms).max(0);
        let progress = match (completed, total) {
            (Some(c), Some(t)) if t > 0 => format!("{}% ({c}/{t})", ((c as f64 / t as f64) * 100.0).round() as i64),
            _ => "Calculating...".into(),
        };
        let phase_elapsed_ms = (now - phase_started_at).max(0);
        let eta_ms = match (completed, total) {
            (Some(c), Some(t)) if c >= 2 && t > c && phase_elapsed_ms >= 3_000 => {
                Some((((t - c) as f64 * phase_elapsed_ms as f64) / c as f64).round() as i64)
            }
            _ => None,
        };
        CreateEmbed::new()
            .color(crate::discord::GAIA_PURPLE)
            .title("Server Restore Running")
            .field("Source backup", format!("{} ({})", backup.source_guild_name, truncate(&backup.backup_id, 8)), true)
            .field("Destination", if dest_name.is_empty() { destination.to_string() } else { dest_name.to_string() }, true)
            .field("Current phase", phase, true)
            .field("Progress", progress, true)
            .field("Elapsed", human_duration(elapsed_ms), true)
            .field("Estimated remaining", eta_ms.map_or_else(|| "Calculating...".into(), human_duration), true)
    }

    async fn start_restore_progress_dm(
        &self,
        backup: &GuildBackup,
        destination: GuildId,
        dest_name: &str,
        user_id: &str,
        restore_id: &str,
        started_at_ms: i64,
    ) -> RestoreProgressDm {
        let mut progress =
            RestoreProgressDm { attempted: true, message: None, last_edit_at: 0, phase: Some("Safety Backup".into()), phase_started_at: started_at_ms };
        let embed = Self::restore_progress_embed(backup, destination, dest_name, "Safety Backup", started_at_ms, None, None, progress.phase_started_at);
        match self.send_dm(user_id, embed).await {
            Ok(message) => {
                progress.message = Some(message);
                progress.last_edit_at = now_ms();
                logger::info(
                    "[restore] progress DM started",
                    ctx! {"restoreId" => restore_id, "guildId" => destination.to_string(), "userId" => user_id},
                );
            }
            Err(error) => logger::warn(
                "[restore] progress DM unavailable",
                ctx! {"restoreId" => restore_id, "guildId" => destination.to_string(), "userId" => user_id, "reason" => error_text(&error)},
            ),
        }
        progress
    }

    async fn send_dm(&self, user_id: &str, embed: CreateEmbed) -> anyhow::Result<(ChannelId, MessageId)> {
        let user = UserId::new(snowflake(user_id)?);
        let channel = user.create_dm_channel(self.discord.http.as_ref()).await?;
        let message = channel.id.send_message(self.discord.http.as_ref(), CreateMessage::new().embed(embed)).await?;
        Ok((message.channel_id, message.id))
    }

    async fn edit_dm(&self, message: (ChannelId, MessageId), embed: CreateEmbed) -> anyhow::Result<()> {
        message.0.edit_message(self.discord.http.as_ref(), message.1, EditMessage::new().embed(embed)).await?;
        Ok(())
    }

    async fn notify_completion(
        &self,
        run: &Run<'_>,
        snapshot: &Snapshot,
        started_at: &str,
        result: &RestoreResult,
        phase: &str,
        failure: Option<&anyhow::Error>,
    ) {
        match self.repo.claim_completion_dm(&result.restore_id) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                logger::warn("restore completion DM failed", ctx! {"restoreId" => result.restore_id, "reason" => error_text(&error)});
                return;
            }
        }
        let finished_at = now_ms();
        let duration = (finished_at - iso_ms(started_at).unwrap_or(finished_at)).max(0);
        let failed = result.status == "failed" || result.status == "failed_partial";
        let title = if failed {
            "Server Restore Failed"
        } else if !result.warnings.is_empty() {
            "Restore Completed With Warnings"
        } else {
            "Server Restore Completed"
        };
        let color = if failed { 0xed4245 } else if !result.warnings.is_empty() { 0xfee75c } else { 0x57f287 };
        let backup = &run.backup;
        let mut embed = CreateEmbed::new()
            .color(color)
            .title(title)
            .field("Destination", if run.dest_name.is_empty() { run.key.clone() } else { run.dest_name.clone() }, true)
            .field("Source backup", format!("{} ({})", backup.source_guild_name, truncate(&backup.backup_id, 8)), true)
            .field("Backup created", format!("<t:{}:F>", iso_ms(&backup.created_at).unwrap_or(0).div_euclid(1000)), false)
            .field(if failed { "Failure time" } else { "Completion time" }, format!("<t:{}:F>", finished_at.div_euclid(1000)), true)
            .field("Duration", human_duration(duration), true);
        if failed {
            let reason = match failure {
                Some(error) => error_text(error),
                None => error_text_str(result.critical_failures.last().map(String::as_str)),
            };
            embed = embed
                .field("Phase", phase, true)
                .field("Safety backup", if result.safety_backup_id.is_some() { "Succeeded" } else { "Did not complete" }, true)
                .field("Destination mutation", if run.destructive { "Started" } else { "Not started" }, true)
                .field("Reason", truncate(&reason, 900), false);
        } else {
            let timings = truncate(
                &result.phase_timings_ms.iter().map(|(name, ms)| format!("{name}: {}", human_duration(*ms))).collect::<Vec<_>>().join("\n"),
                1_000,
            );
            embed = embed
                .field("Roles restored", result.roles.to_string(), true)
                .field("Channels restored", result.channels.to_string(), true)
                .field("Permission overwrites", result.overwrites.to_string(), true)
                .field("Member-role assignments", result.members.to_string(), true)
                .field("Guild settings", "Validated", true)
                .field("Gaia configuration", "Validated", true)
                .field("Community", if truthy(&snapshot.guild["communityEnabled"]) { "Enabled" } else { "Disabled" }, true)
                .field("Warning summary", compact_warning_summary(&result.warnings), false)
                .field("Notable warnings", notable_warnings(&result.warnings), false)
                .field("Phase timings", if timings.is_empty() { "No phase timings recorded.".into() } else { timings }, false);
        }
        let delivery: anyhow::Result<()> = async {
            if run.dm.attempted && run.dm.message.is_none() {
                anyhow::bail!("The initial restore progress DM was unavailable.");
            }
            match run.dm.message {
                Some(message) => self.edit_dm(message, embed).await,
                None => self.send_dm(&run.user_id, embed).await.map(|_| ()),
            }
        }
        .await;
        match delivery {
            Ok(()) => {
                let _ = self.repo.complete_completion_dm(&result.restore_id, true, None);
                logger::info(
                    "[restore] progress DM finalized",
                    ctx! {"restoreId" => result.restore_id, "guildId" => run.key, "userId" => run.user_id},
                );
            }
            Err(error) => {
                let reason = error_text(&error);
                let _ = self.repo.complete_completion_dm(&result.restore_id, false, Some(&reason));
                logger::warn(
                    "restore completion DM failed",
                    ctx! {"restoreId" => result.restore_id, "guildId" => run.key, "userId" => run.user_id, "reason" => reason},
                );
            }
        }
    }

    async fn fetch_state(&self, guild_id: GuildId) -> anyhow::Result<GuildState> {
        let http = &self.discord.http;
        let raw: Value = http.fire(Request::new(Route::Guild { guild_id }, LightMethod::Get)).await?;
        let channels: Vec<Value> = http.fire(Request::new(Route::GuildChannels { guild_id }, LightMethod::Get)).await?;
        let bot_id = self.discord.cache.current_user().id;
        let me = http.get_member(guild_id, bot_id).await.ok().map(|m| (bot_id.to_string(), m.roles.iter().map(|r| r.to_string()).collect()));
        Ok(GuildState { id: guild_id.to_string(), raw, channels, me })
    }

    async fn capture(&self, state: &GuildState, backup_id: &str, members: &[MemberEntry]) -> anyhow::Result<(Snapshot, Vec<GuildBackupAsset>)> {
        let guild_id = &state.id;
        let active_room_ids: HashSet<String> = self.repo.active_rooms(Some(guild_id))?.into_iter().map(|r| r.channel_id).collect();
        let managed_roles = state.roles().iter().filter(|r| r["managed"] == true).count();
        let managed_ids: HashSet<String> = state.roles().iter().filter(|r| r["managed"] == true).map(|r| js_string(&r["id"])).collect();
        let roles: Vec<Value> = state
            .roles()
            .iter()
            .filter(|r| r["managed"] != true && r["id"] != guild_id.as_str())
            .map(|role| {
                let mut out = Map::new();
                out.insert("id".into(), role["id"].clone());
                out.insert("name".into(), role["name"].clone());
                let colors = &role["colors"];
                if let Some(primary) = valid_role_color(&colors["primary_color"]) {
                    let opt = |v: &Value| valid_role_color(v).map_or(Value::Null, |c| json!(c));
                    out.insert(
                        "colors".into(),
                        json!({ "primaryColor": primary, "secondaryColor": opt(&colors["secondary_color"]), "tertiaryColor": opt(&colors["tertiary_color"]) }),
                    );
                }
                out.insert("hoist".into(), json!(role["hoist"] == true));
                out.insert("permissions".into(), json!(normalized_bitfield(&role["permissions"])));
                out.insert("mentionable".into(), json!(role["mentionable"] == true));
                out.insert("position".into(), role["position"].clone());
                let icon = cdn("role-icons", &js_string(&role["id"]), role["icon"].as_str(), false);
                out.insert("iconUrl".into(), icon.map_or(Value::Null, Value::String));
                out.insert("unicodeEmoji".into(), role.get("unicode_emoji").cloned().unwrap_or(Value::Null));
                Value::Object(out)
            })
            .collect();
        let mut unsupported: Vec<String> = Vec::new();
        if managed_roles > 0 {
            unsupported.push(format!(
                "{managed_roles} managed/integration role{} are preserved, not included.",
                plural(managed_roles, "", "s")
            ));
        }
        let features = state.features();
        let controlled: Vec<&String> = features.iter().filter(|f| *f != "COMMUNITY").collect();
        if !controlled.is_empty() {
            unsupported.push(format!(
                "Discord-controlled guild features are captured for comparison only: {}.",
                controlled.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        let in_room = |c: &Value| active_room_ids.contains(&js_string(&c["id"]));
        let temporary = state.channels.iter().filter(|c| in_room(c)).count();
        if temporary > 0 {
            unsupported.push(format!("{temporary} temporary Gaia voice room{} are excluded.", plural(temporary, "", "s")));
        }
        let omitted = state.channels.iter().filter(|c| !is_supported(c) && !in_room(c)).count();
        if omitted > 0 {
            unsupported.push(format!("{omitted} unsupported channel type{} are not included.", plural(omitted, "", "s")));
        }
        let mut channels = Vec::new();
        for channel in state.channels.iter().filter(|c| is_supported(c) && !in_room(c)) {
            let kind = channel["type"].as_u64();
            let forum = kind == Some(FORUM) || kind == Some(MEDIA);
            if !forum && !channel["video_quality_mode"].is_null() {
                unsupported.push(format!("Video quality mode for {} is not restored.", js_string(&channel["name"])));
            }
            let or_null = |key: &str| channel.get(key).cloned().unwrap_or(Value::Null);
            let parent = or_null("parent_id");
            let mut out = Map::new();
            out.insert("id".into(), channel["id"].clone());
            out.insert("type".into(), channel["type"].clone());
            out.insert("name".into(), channel["name"].clone());
            out.insert("parentId".into(), parent.clone());
            out.insert("permissionSync".into(), json!(truthy(&parent) && state.permissions_locked(channel) == Some(true)));
            out.insert("position".into(), channel["position"].clone());
            out.insert("topic".into(), or_null("topic"));
            out.insert("nsfw".into(), channel.get("nsfw").filter(|v| !v.is_null()).cloned().unwrap_or(json!(false)));
            out.insert("rateLimitPerUser".into(), channel.get("rate_limit_per_user").filter(|v| !v.is_null()).cloned().unwrap_or(json!(0)));
            out.insert("defaultThreadRateLimitPerUser".into(), or_null("default_thread_rate_limit_per_user"));
            out.insert("bitrate".into(), or_null("bitrate"));
            out.insert("userLimit".into(), or_null("user_limit"));
            out.insert("rtcRegion".into(), or_null("rtc_region"));
            if forum {
                let tags: Vec<Value> = channel["available_tags"]
                    .as_array()
                    .map(|tags| {
                        tags.iter()
                            .map(|t| json!({ "name": t["name"], "moderated": truthy(&t["moderated"]), "emojiId": t["emoji_id"], "emojiName": t["emoji_name"] }))
                            .collect()
                    })
                    .unwrap_or_default();
                out.insert("availableTags".into(), json!(tags));
            }
            let reaction = &channel["default_reaction_emoji"];
            out.insert(
                "defaultReactionEmoji".into(),
                if forum && reaction.is_object() { json!({ "emojiId": reaction["emoji_id"], "emojiName": reaction["emoji_name"] }) } else { Value::Null },
            );
            out.insert("defaultSortOrder".into(), if forum { or_null("default_sort_order") } else { Value::Null });
            out.insert("defaultForumLayout".into(), if forum { or_null("default_forum_layout") } else { Value::Null });
            let overwrites: Vec<Value> = GuildState::channel_overwrites(channel)
                .iter()
                .map(|o| json!({ "id": o["id"], "type": o["type"], "allow": js_string(&o["allow"]), "deny": js_string(&o["deny"]) }))
                .collect();
            out.insert("overwrites".into(), json!(overwrites));
            channels.push(Value::Object(out));
        }
        let settings = self.repo.settings(guild_id)?;
        let autoroles = self.repo.autoroles(guild_id)?;
        let join = self.repo.join_channel(guild_id)?;
        let mut verification = serde_json::to_value(self.repo.verification(guild_id)?)?;
        verification["channel_id"] = Value::Null;
        verification["message_id"] = Value::Null;
        let staff = self.repo.staff_members(guild_id)?;

        let raw = &state.raw;
        let urls = [
            ("icon", cdn("icons", guild_id, raw["icon"].as_str(), true)),
            ("banner", cdn("banners", guild_id, raw["banner"].as_str(), true)),
            ("splash", cdn("splashes", guild_id, raw["splash"].as_str(), false)),
            ("discovery_splash", cdn("discovery-splashes", guild_id, raw["discovery_splash"].as_str(), false)),
        ];
        let captured = self.capture_guild_assets(&urls, backup_id, &mut unsupported).await?;
        let mut asset_descriptors = Map::new();
        for (kind, url) in &urls {
            let descriptor = captured
                .iter()
                .find(|(a, _)| a.kind == *kind)
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| if url.is_some() { json!({ "state": "unavailable" }) } else { json!({ "state": "absent" }) });
            asset_descriptors.insert((*kind).into(), descriptor);
        }
        let flags = raw["system_channel_flags"].as_u64().unwrap_or(0);
        let everyone = state.role(guild_id).map_or_else(|| "0".to_string(), |r| normalized_bitfield(&r["permissions"]));
        let opt = |key: &str| raw.get(key).cloned().unwrap_or(Value::Null);
        let guild = json!({
            "id": guild_id,
            "name": raw["name"],
            "description": opt("description"),
            "verificationLevel": opt("verification_level"),
            "explicitContentFilter": opt("explicit_content_filter"),
            "defaultMessageNotifications": opt("default_message_notifications"),
            "afkTimeout": opt("afk_timeout"),
            "afkChannelId": opt("afk_channel_id"),
            "systemChannelId": opt("system_channel_id"),
            "systemChannelFlags": flags.to_string(),
            "rulesChannelId": opt("rules_channel_id"),
            "publicUpdatesChannelId": opt("public_updates_channel_id"),
            "safetyAlertsChannelId": opt("safety_alerts_channel_id"),
            "preferredLocale": opt("preferred_locale"),
            "premiumProgressBarEnabled": opt("premium_progress_bar_enabled"),
            "communityEnabled": features.iter().any(|f| f == "COMMUNITY"),
            "features": features,
            "everyonePermissions": everyone,
            "assets": asset_descriptors,
        });
        let members = members
            .iter()
            .filter(|m| !m.bot)
            .map(|m| SnapshotMember {
                user_id: m.id.clone(),
                role_ids: m.roles.iter().filter(|r| !managed_ids.contains(*r) && *r != guild_id).cloned().collect(),
            })
            .collect();
        let snapshot = Snapshot {
            schema_version: BACKUP_SCHEMA_VERSION,
            guild,
            roles,
            channels,
            members,
            gaia: json!({
                "settings": settings,
                "autoroles": autoroles,
                "staff": staff,
                "joinChannelId": join,
                "verification": verification,
            }),
            unsupported: Some(unsupported),
        };
        Ok((snapshot, captured.into_iter().map(|(a, _)| a).collect()))
    }

    async fn download_asset(&self, url: &str) -> anyhow::Result<(String, Vec<u8>)> {
        let response = self.client.get(url).send().await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().trim().to_lowercase())
            .unwrap_or_default();
        let advertised = response.content_length().unwrap_or(0) as usize;
        if !response.status().is_success() || !["image/png", "image/jpeg", "image/webp", "image/gif"].contains(&content_type.as_str()) {
            anyhow::bail!("Discord did not provide a supported image response.");
        }
        let mb = MAX_GUILD_ASSET_BYTES / 1024 / 1024;
        if advertised > MAX_GUILD_ASSET_BYTES {
            anyhow::bail!("Asset exceeds {mb} MB.");
        }
        let data = response.bytes().await?.to_vec();
        if data.is_empty() || data.len() > MAX_GUILD_ASSET_BYTES {
            anyhow::bail!("Asset must be between 1 byte and {mb} MB.");
        }
        Ok((content_type, data))
    }

    async fn capture_guild_assets(
        &self,
        source: &[(&str, Option<String>)],
        backup_id: &str,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<Vec<(GuildBackupAsset, Value)>> {
        let mut assets = Vec::new();
        for (kind, url) in source {
            let Some(url) = url else { continue };
            match self.download_asset(url).await {
                Ok((content_type, data)) => {
                    let sha256 = sha256_hex(&data);
                    let descriptor =
                        json!({ "state": "captured", "kind": kind, "contentType": content_type, "sizeBytes": data.len(), "sha256": sha256 });
                    assets.push((
                        GuildBackupAsset {
                            backup_id: backup_id.into(),
                            kind: (*kind).into(),
                            content_type,
                            size_bytes: data.len() as i64,
                            sha256,
                            data,
                            created_at: crate::db::now(),
                        },
                        descriptor,
                    ));
                }
                Err(error) => {
                    let detail = truncate(&error.to_string().replace(['\r', '\n'], " "), 180);
                    if *kind == "icon" {
                        anyhow::bail!("Guild icon could not be captured: {detail}");
                    }
                    warnings.push(format!("{} was not captured and cannot be restored: {detail}", kind.replacen('_', " ", 1)));
                }
            }
        }
        Ok(assets)
    }

    async fn require_access(&self, backup_id: &str, user_id: &str) -> anyhow::Result<GuildBackup> {
        let Some(backup) = self.repo.backup(backup_id)? else {
            anyhow::bail!("That backup no longer exists.");
        };
        if !self.can_access(&backup, user_id).await {
            anyhow::bail!("You are not authorized to access that backup.");
        }
        Ok(backup)
    }

    /// Resolves a URL (role icon) to a data URI like discord.js `DataResolver.resolveImage`.
    async fn data_uri(&self, url: &str) -> anyhow::Result<String> {
        let response = self.client.get(url).send().await?.error_for_status()?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png")
            .split(';')
            .next()
            .unwrap_or("image/png")
            .to_string();
        let data = response.bytes().await?;
        Ok(format!("data:{content_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(&data)))
    }

    async fn create_role(&self, guild_id: GuildId, mut body: Value) -> anyhow::Result<String> {
        if let Some(url) = body.get("icon").and_then(Value::as_str).map(str::to_string) {
            body["icon"] = json!(self.data_uri(&url).await?);
        }
        let role = self.discord.http.create_role(guild_id, &body, Some("Gaia full backup restore")).await?;
        Ok(role.id.to_string())
    }
}

// ---------------------------------------------------------------- one restore run

struct Run<'a> {
    svc: &'a BackupService,
    guild_id: GuildId,
    key: String,
    dest_name: String,
    backup: GuildBackup,
    restore_id: String,
    user_id: String,
    started_at_ms: i64,
    last_progress_at_ms: i64,
    phase_started_at_ms: i64,
    phase: String,
    phase_timings: Vec<(String, i64)>,
    operation: Option<String>,
    warnings: Vec<String>,
    critical: Vec<String>,
    role_map: OrderedMap,
    channel_map: OrderedMap,
    counts: Counts,
    safety_backup_id: Option<String>,
    destructive: bool,
    unremovable: Vec<(String, String)>,
    member_ids: HashSet<String>,
    report: Option<RestoreProgress>,
    dm: RestoreProgressDm,
}

const REASON: &str = "Gaia full backup restore";

impl Run<'_> {
    fn http(&self) -> &serenity::http::Http {
        &self.svc.discord.http
    }

    fn operation(&self) -> (String, String) {
        let mut progress = Map::new();
        progress.insert("roleMap".into(), json!(self.role_map.entries()));
        progress.insert("channelMap".into(), json!(self.channel_map.entries()));
        progress.insert("roles".into(), json!(self.counts.roles));
        progress.insert("channels".into(), json!(self.counts.channels));
        progress.insert("overwrites".into(), json!(self.counts.overwrites));
        progress.insert("members".into(), json!(self.counts.members));
        if let Some(id) = &self.safety_backup_id {
            progress.insert("safetyBackupId".into(), json!(id));
        }
        if let Some(op) = &self.operation {
            progress.insert("lastOperation".into(), json!(op));
        }
        let timings: Map<String, Value> = self.phase_timings.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        progress.insert("phaseTimingsMs".into(), Value::Object(timings));
        let errors: Vec<&String> = self.warnings.iter().chain(&self.critical).collect();
        (Value::Object(progress).to_string(), json!(errors).to_string())
    }

    fn update(&self, status: Option<&str>, phase: bool, completed: bool) -> anyhow::Result<()> {
        let (progress_json, errors_json) = self.operation();
        self.svc.repo.update_restore_operation(
            &self.restore_id,
            &RestoreOperationUpdate {
                status: status.map(str::to_string),
                current_phase: phase.then(|| self.phase.clone()),
                progress_json: Some(progress_json),
                errors_json: Some(errors_json),
                completed: completed.then_some(true),
            },
        )
    }

    fn close_phase(&mut self) {
        let duration_ms = (now_ms() - self.phase_started_at_ms).max(0);
        match self.phase_timings.iter_mut().find(|(k, _)| *k == self.phase) {
            Some((_, total)) => *total += duration_ms,
            None => self.phase_timings.push((self.phase.clone(), duration_ms)),
        }
        if duration_ms >= 30_000 {
            logger::warn(
                "[restore] slow phase",
                ctx! {"restoreId" => self.restore_id, "guildId" => self.key, "phase" => self.phase, "elapsedMs" => now_ms() - self.started_at_ms, "durationMs" => duration_ms},
            );
        }
    }

    fn set_phase(&mut self, next: &str) {
        self.close_phase();
        self.phase = next.to_string();
        self.phase_started_at_ms = now_ms();
        logger::info(
            "[restore] phase",
            ctx! {"restoreId" => self.restore_id, "guildId" => self.key, "phase" => self.phase, "elapsedMs" => self.phase_started_at_ms - self.started_at_ms},
        );
    }

    /// Logs the Discord REST failure before reducing it to a safe operator message.
    fn restore_error(&mut self, operation: &str, error: &anyhow::Error, context: logger::Ctx) -> String {
        self.operation = Some(operation.to_string());
        let mut all = ctx! {
            "service" => "backup.restore",
            "restoreId" => self.restore_id,
            "phase" => self.phase,
            "operation" => operation,
            "guildId" => self.key,
        };
        all.extend(context);
        logger::log_discord_error("backup.restore", error, all);
        error_text(error)
    }

    fn throw_if_critical(&self) -> anyhow::Result<()> {
        match self.critical.last() {
            Some(last) => Err(PartialRestoreError(last.clone()).into()),
            None => Ok(()),
        }
    }

    fn result(&self, status: &str, critical_failures: Vec<String>) -> RestoreResult {
        RestoreResult {
            restore_id: self.restore_id.clone(),
            status: status.into(),
            warnings: self.warnings.clone(),
            critical_failures,
            phase_timings_ms: self.phase_timings.clone(),
            safety_backup_id: self.safety_backup_id.clone(),
            roles: self.counts.roles,
            channels: self.counts.channels,
            overwrites: self.counts.overwrites,
            members: self.counts.members,
        }
    }

    fn log_completed(&self, status: &str) {
        let context = ctx! {
            "restoreId" => self.restore_id,
            "guildId" => self.key,
            "status" => status,
            "elapsedMs" => now_ms() - self.started_at_ms,
            "phaseTimingsMs" => Value::Object(self.phase_timings.iter().map(|(k, v)| (k.clone(), json!(v))).collect()).to_string(),
            "warningCount" => self.warnings.len(),
        };
        if status == "failed" || status == "failed_partial" {
            logger::warn("[restore] completed", context);
        } else {
            logger::info("[restore] completed", context);
        }
    }

    async fn report(&mut self, label: &str, completed: Option<i64>, total: Option<i64>) {
        let now = now_ms();
        let elapsed_ms = now - self.started_at_ms;
        let counter = match (completed, total) {
            (Some(c), Some(t)) => format!(" {c}/{t}"),
            _ => String::new(),
        };
        logger::info(
            &format!("[restore] {}{counter}", label.to_lowercase()),
            ctx! {"restoreId" => self.restore_id, "guildId" => self.key, "elapsedMs" => elapsed_ms, "completed" => completed, "total" => total},
        );
        if now - self.last_progress_at_ms >= 30_000 {
            logger::warn(
                "[restore] progress interval exceeded threshold",
                ctx! {"restoreId" => self.restore_id, "guildId" => self.key, "phase" => label, "elapsedMs" => elapsed_ms, "intervalMs" => now - self.last_progress_at_ms},
            );
        }
        self.last_progress_at_ms = now;
        self.update_progress_dm(label, completed, total, completed == Some(0) || completed == total).await;
        if let Some(report) = &self.report {
            report(label.to_string(), completed, total).await;
        }
    }

    async fn update_progress_dm(&mut self, phase: &str, completed: Option<i64>, total: Option<i64>, force: bool) {
        let Some(message) = self.dm.message else { return };
        let now = now_ms();
        let phase_changed = self.dm.phase.as_deref() != Some(phase);
        if phase_changed {
            self.dm.phase = Some(phase.to_string());
            self.dm.phase_started_at = now;
        }
        if !force && !phase_changed && now - self.dm.last_edit_at < 5_000 {
            return;
        }
        self.dm.last_edit_at = now;
        let embed = BackupService::restore_progress_embed(
            &self.backup,
            self.guild_id,
            &self.dest_name,
            phase,
            self.started_at_ms,
            completed,
            total,
            self.dm.phase_started_at,
        );
        if let Err(error) = self.svc.edit_dm(message, embed).await {
            self.dm.message = None;
            logger::warn(
                "[restore] progress DM edit unavailable",
                ctx! {"restoreId" => self.restore_id, "guildId" => self.key, "reason" => error_text(&error)},
            );
        }
    }

    async fn execute(&mut self, snapshot: &Snapshot, assets: &[GuildBackupAsset]) -> anyhow::Result<RestoreResult> {
        validate_assets(snapshot, assets)?;
        self.report("Safety Backup", None, None).await;
        let protected = (self.backup.source_guild_id == self.key).then(|| self.backup.backup_id.clone());
        let safety = self.svc.create(self.guild_id, Some(&self.user_id), "safety", protected.as_deref()).await?;
        // The row is loaded again after commit: its hash, JSON and schema must all be valid before deletion starts.
        load(&safety)?;
        self.safety_backup_id = Some(safety.backup_id.clone());
        self.preflight().await?;
        assert_reconstructable(snapshot)?;
        self.phase = "destination cleanup".into();
        self.update(Some("running"), true, false)?;
        self.phase = "safety backup".into();
        self.set_phase("destination cleanup");
        self.report("Destructive cleanup begins", None, None).await;
        self.destructive = true;
        self.delete_destination_state().await?;
        self.throw_if_critical()?;

        self.set_phase("roles");
        self.update(None, true, false)?;
        let total_roles = snapshot.roles.len() as i64;
        self.report("Roles", Some(0), Some(total_roles)).await;
        for (index, saved) in snapshot.roles.iter().enumerate() {
            let name = js_string(&saved["name"]);
            let id = js_string(&saved["id"]);
            let colors = role_colors_from_snapshot(saved);
            let requested_cosmetics =
                colors.is_some() || saved.get("unicodeEmoji").and_then(str_of).is_some() || saved.get("iconUrl").and_then(str_of).is_some();
            if (saved.get("colors").is_some() || saved.get("color").is_some()) && colors.is_none() {
                self.warnings.push(format!(
                    "Role {name} ({id}) has malformed or absent color metadata; its color was omitted rather than sending an invalid Discord payload."
                ));
            }
            let created = match self.svc.create_role(self.guild_id, role_create_options(saved, true)).await {
                Ok(role) => Ok(role),
                Err(error) => {
                    self.restore_error(
                        "role.create",
                        &error,
                        ctx! {"roleId" => id, "roleName" => name, "attemptedCosmetics" => requested_cosmetics},
                    );
                    if requested_cosmetics {
                        self.warnings.push(format!(
                            "Role {name} ({id}) rejected one or more cosmetic properties; retrying its required permissions and flags without cosmetics."
                        ));
                        self.svc.create_role(self.guild_id, role_create_options(saved, false)).await
                    } else {
                        Err(error)
                    }
                }
            };
            match created {
                Ok(role_id) => {
                    self.role_map.set(&id, role_id);
                    self.counts.roles += 1;
                    self.update(None, false, false)?;
                }
                Err(error) => {
                    self.restore_error("role.create", &error, ctx! {"roleId" => id, "roleName" => name});
                    self.critical.push(format!("Role {name} ({id}) could not be created — {}", error_text(&error)));
                }
            }
            if index % 10 == 0 || index + 1 == snapshot.roles.len() {
                self.report("Roles", Some(index as i64 + 1), Some(total_roles)).await;
            }
        }
        self.apply_role_hierarchy(snapshot).await?;
        let everyone_permissions = js_string(&snapshot.guild["everyonePermissions"]);
        let everyone = self
            .http()
            .edit_role(self.guild_id, RoleId::new(self.guild_id.get()), &json!({ "permissions": normalized_bitfield(&json!(everyone_permissions)) }), Some(REASON))
            .await;
        if let Err(error) = everyone {
            let error = anyhow::Error::from(error);
            self.restore_error("role.setPermissions", &error, ctx! {"roleId" => self.key.clone()});
            self.critical.push(format!("@everyone permissions could not be restored — {}", error_text(&error)));
        }
        self.throw_if_critical()?;

        let categories: Vec<&Value> = snapshot.channels.iter().filter(|c| c["type"].as_u64() == Some(CATEGORY)).collect();
        self.set_phase("categories");
        self.update(None, true, false)?;
        self.create_channels(&categories, "Categories").await;
        let members = self.svc.acquire_members(self.guild_id).await?;
        self.member_ids = members.members.iter().map(|m| m.id.clone()).collect();
        self.apply_overwrites(&categories).await?;
        self.throw_if_critical()?;
        // Community requires mapped Rules/Updates channels. Create ordinary channels first, then turn it on before announcement channels.
        self.set_phase("community prerequisites");
        self.update(None, true, false)?;
        self.report("Community prerequisites", None, None).await;
        let ordinary: Vec<&Value> =
            snapshot.channels.iter().filter(|c| !matches!(c["type"].as_u64(), Some(CATEGORY | ANNOUNCEMENT))).collect();
        self.create_channels(&ordinary, "Channels").await;
        self.throw_if_critical()?;
        self.restore_community(snapshot).await?;
        self.throw_if_critical()?;
        self.set_phase("announcement channels");
        self.update(None, true, false)?;
        let announcements: Vec<&Value> = snapshot.channels.iter().filter(|c| c["type"].as_u64() == Some(ANNOUNCEMENT)).collect();
        self.create_channels(&announcements, "Announcement channels").await;
        self.apply_channel_positions(&snapshot.channels).await?;
        self.throw_if_critical()?;

        self.set_phase("permission overwrites");
        self.update(None, true, false)?;
        let non_categories: Vec<&Value> = snapshot.channels.iter().filter(|c| c["type"].as_u64() != Some(CATEGORY)).collect();
        self.apply_overwrites(&non_categories).await?;
        self.throw_if_critical()?;
        self.set_phase("member roles");
        self.update(None, true, false)?;
        self.replace_member_roles(snapshot).await?;
        self.set_phase("guild settings");
        self.update(None, true, false)?;
        self.report("Guild settings", None, None).await;
        self.restore_guild_settings(snapshot, assets).await?;
        self.throw_if_critical()?;
        self.set_phase("stale channel cleanup");
        self.update(None, true, false)?;
        self.report("Stale channel cleanup", None, None).await;
        self.cleanup_stale_channels().await?;
        self.throw_if_critical()?;
        self.set_phase("Gaia configuration");
        self.report("Gaia Configuration", None, None).await;
        self.restore_gaia_config(&snapshot.gaia)?;
        self.set_phase("final validation");
        self.update(None, true, false)?;
        self.report("Final Validation", None, None).await;
        self.final_validate(snapshot).await?;
        self.close_phase();
        let status = if !self.critical.is_empty() {
            "failed_partial"
        } else if !self.warnings.is_empty() {
            "completed_with_warnings"
        } else {
            "completed"
        };
        self.phase = if status == "failed_partial" { "partial failure" } else { "complete" }.into();
        self.update(Some(status), true, true)?;
        self.phase = "final validation".into();
        let result = self.result(status, self.critical.clone());
        self.log_completed(status);
        self.svc.record(AuditEvent {
            title: if status == "completed" { "Restore Completed" } else { "Restore Completed With Warnings" }.into(),
            category: "Backups".into(),
            color: if status == "completed" { "success" } else { "warning" }.into(),
            priority: "critical".into(),
            guild: Some(AuditSubject::named(self.key.clone(), self.dest_name.clone())),
            actor: Some(AuditSubject::id(self.user_id.clone())),
            details: vec![
                ("Restore".into(), json!(self.restore_id)),
                ("Source Guild".into(), json!(self.backup.source_guild_name)),
                ("Roles".into(), json!(self.counts.roles)),
                ("Channels".into(), json!(self.counts.channels)),
                ("Overwrites".into(), json!(self.counts.overwrites)),
                ("Member Roles".into(), json!(self.counts.members)),
                ("Warnings".into(), json!(self.warnings.len())),
                ("Duration".into(), json!(human_duration(now_ms() - self.started_at_ms))),
            ],
            ..Default::default()
        });
        Ok(result)
    }

    async fn state(&self) -> anyhow::Result<GuildState> {
        self.svc.fetch_state(self.guild_id).await
    }

    async fn preflight(&self) -> anyhow::Result<()> {
        let state = self.state().await?;
        if state.me.is_none() {
            return Ok(()); // partial state; Discord still authoritatively checks every call.
        }
        let needed = MANAGE_CHANNELS | MANAGE_ROLES | MANAGE_GUILD;
        if state.base_permissions() & needed != needed {
            anyhow::bail!("Gaia needs Manage Channels, Manage Roles, and Manage Server before a full restore can begin.");
        }
        Ok(())
    }

    async fn delete_destination_state(&mut self) -> anyhow::Result<()> {
        let active: HashSet<String> =
            self.svc.repo.active_rooms(Some(&self.key)).unwrap_or_default().into_iter().map(|r| r.channel_id).collect();
        let state = self.state().await?;
        let mut channels: Vec<Value> =
            state.channels.iter().filter(|c| is_supported(c) && !active.contains(&js_string(&c["id"]))).cloned().collect();
        // Children are removed before their categories. Managed/system objects and undeletable objects are preserved and reported.
        categories_last(&mut channels);
        for (index, channel) in channels.iter().enumerate() {
            let id = js_string(&channel["id"]);
            let name = js_string(&channel["name"]);
            if !state.channel_deletable(channel) {
                let references = state.guild_channel_references(&id);
                logger::warn(
                    "[restore] destination channel deferred",
                    ctx! {
                        "guildId" => self.key,
                        "channelId" => id,
                        "channelName" => name,
                        "references" => if references.is_empty() { "none".to_string() } else { references.join(",") },
                        "reason" => if references.is_empty() {
                            "discord.js reports channel.deletable=false before reconciliation"
                        } else {
                            "Referenced by active guild settings before reconciliation"
                        },
                    },
                );
                continue;
            }
            let deleted = self
                .http()
                .delete_channel(ChannelId::new(snowflake(&id)?), Some("Gaia full backup restore: authoritative destination cleanup"))
                .await;
            if let Err(error) = deleted {
                let error = anyhow::Error::from(error);
                self.restore_error("channel.delete", &error, ctx! {"channelId" => id});
                self.critical.push(format!("Destination channel {name} could not be deleted — {}", error_text(&error)));
            }
            if index % 10 == 0 || index + 1 == channels.len() {
                self.report("Deleting channels", Some(index as i64 + 1), Some(channels.len() as i64)).await;
            }
        }
        let me_id = state.me.as_ref().map(|(id, _)| id.clone());
        for role in state.roles() {
            let is_bot_role = me_id.as_deref().is_some_and(|me| role["tags"]["bot_id"] == me);
            if role["id"] == self.key.as_str() || role["managed"] == true || is_bot_role {
                continue;
            }
            let name = js_string(&role["name"]);
            if !state.role_editable(role) {
                self.warnings.push(format!("Destination role {name} is above Gaia's hierarchy and was preserved."));
                continue;
            }
            let deleted = self
                .http()
                .delete_role(self.guild_id, RoleId::new(snowflake(&js_string(&role["id"]))?), Some("Gaia full backup restore: authoritative destination cleanup"))
                .await;
            if let Err(error) = deleted {
                self.critical.push(format!("Destination role {name} could not be deleted — {}", error_text(&error.into())));
            }
        }
        Ok(())
    }

    /// Removes leftovers only after Community and channel-backed guild settings point at restored channels.
    async fn cleanup_stale_channels(&mut self) -> anyhow::Result<()> {
        let state = self.state().await?;
        let expected = self.channel_map.values();
        let mut stale: Vec<Value> =
            state.channels.iter().filter(|c| is_supported(c) && !expected.contains(&js_string(&c["id"]))).cloned().collect();
        categories_last(&mut stale);
        for channel in &stale {
            let id = js_string(&channel["id"]);
            let name = js_string(&channel["name"]);
            let references = state.guild_channel_references(&id);
            let references_text = if references.is_empty() { "none".to_string() } else { references.join(",") };
            if !state.channel_deletable(channel) {
                let reason = if references.is_empty() {
                    "discord.js reports channel.deletable=false after settings reconciliation".to_string()
                } else {
                    format!("Discord still references it as {}", references.join(", "))
                };
                self.unremovable.push((id.clone(), reason.clone()));
                logger::warn(
                    "[restore] stale channel cannot be deleted",
                    ctx! {"guildId" => self.key, "channelId" => id, "channelName" => name, "references" => references_text, "reason" => reason},
                );
                continue;
            }
            logger::info(
                "[restore] deleting stale channel after reconciliation",
                ctx! {"guildId" => self.key, "channelId" => id, "channelName" => name, "references" => references_text},
            );
            let deleted = self
                .http()
                .delete_channel(ChannelId::new(snowflake(&id)?), Some("Gaia full backup restore: post-reconciliation stale channel cleanup"))
                .await;
            if let Err(error) = deleted {
                let error = anyhow::Error::from(error);
                self.restore_error("channel.delete.stale", &error, ctx! {"channelId" => id});
                self.critical.push(format!(
                    "Stale channel {name} could not be deleted after settings reconciliation — {}",
                    error_text(&error)
                ));
            }
        }
        Ok(())
    }

    async fn apply_role_hierarchy(&mut self, snapshot: &Snapshot) -> anyhow::Result<()> {
        let state = self.state().await?;
        let ceiling = state.highest().filter(|_| state.me.is_some()).and_then(|r| r["position"].as_i64()).unwrap_or((1 << 53) - 1);
        let mut roles: Vec<&Value> = snapshot.roles.iter().collect();
        roles.sort_by_key(|r| r["position"].as_i64().unwrap_or(0));
        for saved in roles {
            let name = js_string(&saved["name"]);
            let id = js_string(&saved["id"]);
            let Some(mapped) = self.role_map.get(&id).cloned() else {
                // The creation failure above is the authoritative error; do not obscure it
                // with a second, downstream "unavailable" hierarchy failure.
                continue;
            };
            let Some(role) = state.role(&mapped) else {
                self.critical.push(format!(
                    "Role hierarchy for {name} ({id}) cannot be restored because its mapped destination role is unavailable."
                ));
                continue;
            };
            if !state.role_editable(role) {
                self.critical.push(format!("Role hierarchy for {name} cannot be restored because Gaia cannot manage it."));
                continue;
            }
            let position = saved["position"].as_i64().unwrap_or(0);
            if position >= ceiling {
                self.warnings.push(format!("Role {name} was placed below Gaia's highest role instead of its source position."));
            }
            let target = position.min((ceiling - 1).max(1)).clamp(0, i64::from(u16::MAX)) as u16;
            let moved = self.http().edit_role_position(self.guild_id, RoleId::new(snowflake(&mapped)?), target, Some(REASON)).await;
            if let Err(error) = moved {
                self.critical.push(format!("Role position for {name} could not be restored — {}", error_text(&error.into())));
            }
        }
        Ok(())
    }

    async fn create_channels(&mut self, saved_channels: &[&Value], label: &str) {
        let total = saved_channels.len() as i64;
        self.report(label, Some(0), Some(total)).await;
        for (index, saved) in saved_channels.iter().enumerate() {
            let name = js_string(&saved["name"]);
            let parent_id = &saved["parentId"];
            let parent = if truthy(parent_id) { self.channel_map.get(&js_string(parent_id)).cloned() } else { None };
            if truthy(parent_id) && parent.is_none() {
                self.critical.push(format!("Channel {name} cannot be created because its restored category is unavailable."));
                continue;
            }
            let body = channel_options(saved, parent.as_deref());
            match self.http().create_channel(self.guild_id, &body, Some(REASON)).await {
                Ok(channel) => {
                    self.channel_map.set(&js_string(&saved["id"]), channel.id.to_string());
                    self.counts.channels += 1;
                }
                Err(error) => {
                    let error = anyhow::Error::from(error);
                    self.restore_error("channel.create", &error, ctx! {"channelId" => js_string(&saved["id"])});
                    self.critical.push(format!("Channel {name} could not be created — {}", error_text(&error)));
                }
            }
            if index % 10 == 0 || index + 1 == saved_channels.len() {
                self.report(label, Some(index as i64 + 1), Some(total)).await;
            }
        }
    }

    async fn restore_community(&mut self, snapshot: &Snapshot) -> anyhow::Result<()> {
        let guild = &snapshot.guild;
        let Some(source_community) = guild["communityEnabled"].as_bool() else {
            self.warnings.push("Community state was not captured by this older backup and was preserved.".into());
            return Ok(());
        };
        let mapped = |key: &str| if truthy(&guild[key]) { self.channel_map.get(&js_string(&guild[key])).cloned() } else { None };
        let (rules, updates) = (mapped("rulesChannelId"), mapped("publicUpdatesChannelId"));
        if source_community && (rules.is_none() || updates.is_none()) {
            self.critical.push(
                "Community could not be enabled because the backed-up Rules or Community Updates channel was not reconstructed.".into(),
            );
            return Ok(());
        }
        let state = self.state().await?;
        let mut features: Vec<String> = Vec::new();
        for feature in state.features() {
            if feature != "COMMUNITY" && !features.contains(&feature) {
                features.push(feature);
            }
        }
        if source_community {
            features.push("COMMUNITY".into());
        }
        let body = json!({ "features": features, "rules_channel_id": rules, "public_updates_channel_id": updates });
        if let Err(error) = self.http().edit_guild(self.guild_id, &body, Some("Gaia full backup restore: Community state")).await {
            let error = anyhow::Error::from(error);
            self.restore_error("guild.edit", &error, ctx! {"field" => "community"});
            let message = format!(
                "Community source {} could not be applied — {}",
                if source_community { "enabled" } else { "disabled" },
                error_text(&error)
            );
            if source_community {
                self.critical.push(message);
            } else {
                self.warnings.push(message);
            }
        }
        Ok(())
    }

    async fn apply_channel_positions(&mut self, channels: &[Value]) -> anyhow::Result<()> {
        let state = self.state().await?;
        for saved in channels {
            let name = js_string(&saved["name"]);
            let channel = self.channel_map.get(&js_string(&saved["id"])).filter(|id| state.channel(id).is_some()).cloned();
            let Some(channel) = channel else {
                self.critical.push(format!("Channel position for {name} cannot be restored because its channel is unavailable."));
                continue;
            };
            let body = json!([{ "id": channel, "position": saved["position"] }]);
            if let Err(error) = self.http().edit_guild_channel_positions(self.guild_id, &body).await {
                self.warnings.push(format!("Channel position for {name} could not be restored — {}", error_text(&error.into())));
            }
        }
        Ok(())
    }

    async fn apply_overwrites(&mut self, saved_channels: &[&Value]) -> anyhow::Result<()> {
        let state = self.state().await?;
        let total: usize = saved_channels.iter().map(|c| overwrite_count(c)).sum();
        let mut complete = 0;
        self.report("Permissions", Some(0), Some(total as i64)).await;
        for saved in saved_channels {
            let name = js_string(&saved["name"]);
            let destination = self.channel_map.get(&js_string(&saved["id"])).and_then(|id| state.channel(id));
            let Some(destination) = destination else {
                self.critical.push(format!(
                    "Permission overwrites for {name} cannot be restored because its destination channel is unavailable."
                ));
                continue;
            };
            let channel_id = js_string(&destination["id"]);
            let (mapped, skipped) = map_permission_overwrites(&saved_overwrites(saved), &self.role_map.map, &self.member_ids);
            if skipped > 0 {
                self.warnings.push(format!(
                    "{skipped} member-specific permission target{} for {name} is no longer in the destination and was skipped.",
                    plural(skipped, "", "s")
                ));
            }
            let lock = saved["permissionSync"] == true && truthy(&saved["parentId"]);
            let body = if lock {
                // discord.js `lockPermissions`: copy the destination parent's current overwrites.
                let parent = destination["parent_id"].as_str().and_then(|id| state.channel(id));
                let overwrites: Vec<Value> = parent
                    .map(GuildState::channel_overwrites)
                    .unwrap_or_default()
                    .iter()
                    .map(|o| json!({ "id": o["id"], "type": o["type"], "allow": js_string(&o["allow"]), "deny": js_string(&o["deny"]) }))
                    .collect();
                json!({ "permission_overwrites": overwrites })
            } else {
                let overwrites: Vec<Value> = mapped
                    .iter()
                    .map(|o| json!({ "id": o.id, "type": o.kind, "allow": o.allow.to_string(), "deny": o.deny.to_string() }))
                    .collect();
                json!({ "permission_overwrites": overwrites })
            };
            let reason = if lock {
                "Gaia full backup restore: sync category permissions"
            } else {
                "Gaia full backup restore: authoritative overwrite replacement"
            };
            match self.http().edit_channel(ChannelId::new(snowflake(&channel_id)?), &body, Some(reason)).await {
                Ok(_) => self.counts.overwrites += mapped.len() as i64,
                Err(error) => {
                    let error = anyhow::Error::from(error);
                    let operation = if lock { "channel.lockPermissions" } else { "channel.permissionOverwrites.set" };
                    self.restore_error(operation, &error, ctx! {"channelId" => channel_id});
                    self.critical.push(format!("Permission overwrite for {name} could not be restored — {}", error_text(&error)));
                }
            }
            complete += overwrite_count(saved);
            if complete % 20 == 0 || complete == total {
                self.report("Permissions", Some(complete as i64), Some(total as i64)).await;
            }
        }
        Ok(())
    }

    async fn replace_member_roles(&mut self, snapshot: &Snapshot) -> anyhow::Result<()> {
        enum Outcome {
            Unavailable,
            Replaced,
            Failed(String),
        }
        let total = snapshot.members.len() as i64;
        self.report("Member Roles", Some(0), Some(total)).await;
        let state = self.state().await?;
        let removable_roles: Arc<HashSet<String>> = Arc::new(
            state
                .roles()
                .iter()
                .filter(|r| r["id"] != self.key.as_str() && r["managed"] != true && state.role_editable(r))
                .map(|r| js_string(&r["id"]))
                .collect(),
        );
        let mut tasks = tokio::task::JoinSet::new();
        let mut queue = snapshot.members.iter();
        let mut done = 0i64;
        loop {
            while tasks.len() < 5 {
                let Some(saved) = queue.next() else { break };
                let http = Arc::clone(&self.svc.discord.http);
                let guild_id = self.guild_id;
                let removable_roles = Arc::clone(&removable_roles);
                let desired: Vec<String> = saved.role_ids.iter().filter_map(|id| self.role_map.get(id).cloned()).collect();
                let user = snowflake(&saved.user_id).ok().map(UserId::new);
                tasks.spawn(async move {
                    let member = match user {
                        Some(user) => http.get_member(guild_id, user).await.ok(),
                        None => None,
                    };
                    let Some(member) = member else { return Outcome::Unavailable };
                    let mut desired_unique: Vec<String> = Vec::new();
                    for id in desired {
                        if !desired_unique.contains(&id) {
                            desired_unique.push(id);
                        }
                    }
                    let current: Vec<String> = member.roles.iter().map(|r| r.to_string()).collect();
                    let removable: Vec<&String> = current.iter().filter(|r| removable_roles.contains(*r)).collect();
                    let kept: Vec<String> = current.iter().filter(|r| !removable_roles.contains(*r)).cloned().collect();
                    let work = async {
                        if !removable.is_empty() {
                            http.edit_member(guild_id, member.user.id, &json!({ "roles": kept }), Some("Gaia full backup restore: replace saved roles"))
                                .await?;
                        }
                        if !desired_unique.is_empty() {
                            let mut roles = kept.clone();
                            for id in desired_unique {
                                if !roles.contains(&id) {
                                    roles.push(id);
                                }
                            }
                            http.edit_member(guild_id, member.user.id, &json!({ "roles": roles }), Some("Gaia full backup restore: apply saved roles"))
                                .await?;
                        }
                        Ok::<(), serenity::Error>(())
                    };
                    match work.await {
                        Ok(()) => Outcome::Replaced,
                        Err(error) => Outcome::Failed(error_text(&error.into())),
                    }
                });
            }
            let Some(joined) = tasks.join_next().await else { break };
            done += 1;
            match joined {
                Ok(Outcome::Unavailable) => {
                    self.warnings.push("Roles for one unavailable member were skipped.".into());
                    continue;
                }
                Ok(Outcome::Replaced) => self.counts.members += 1,
                Ok(Outcome::Failed(text)) => self.warnings.push(format!("Roles for one member could not be replaced — {text}")),
                Err(error) => self.warnings.push(format!("Roles for one member could not be replaced — {}", error_text_str(Some(&error.to_string())))),
            }
            if done % 25 == 0 || done == total {
                self.report("Member Roles", Some(done), Some(total)).await;
            }
        }
        Ok(())
    }

    async fn apply_setting(&mut self, field: &str, body: Value, optional: bool) {
        let reason = format!("Gaia full backup restore: guild setting {field}");
        if let Err(error) = self.http().edit_guild(self.guild_id, &body, Some(&reason)).await {
            let error = anyhow::Error::from(error);
            let detail = self.restore_error("guild.edit", &error, ctx! {"field" => field});
            let message = format!("Guild setting {field} could not be restored: {detail}");
            if optional {
                self.warnings.push(message);
            } else {
                self.critical.push(message);
            }
        }
    }

    fn mapped_channel(&self, id: &Value) -> Value {
        if truthy(id) { self.channel_map.get(&js_string(id)).map_or(Value::Null, |c| json!(c)) } else { Value::Null }
    }

    async fn restore_guild_settings(&mut self, snapshot: &Snapshot, assets: &[GuildBackupAsset]) -> anyhow::Result<()> {
        let setting = &snapshot.guild;
        let has = |key: &str| setting.get(key).is_some();
        self.apply_setting("name", json!({ "name": setting["name"] }), false).await;
        if has("description") {
            self.apply_setting("description", json!({ "description": setting["description"] }), false).await;
        }
        for (field, api) in [
            ("verificationLevel", "verification_level"),
            ("explicitContentFilter", "explicit_content_filter"),
            ("defaultMessageNotifications", "default_message_notifications"),
            ("afkTimeout", "afk_timeout"),
        ] {
            if has(field) {
                self.apply_setting(field, json!({ api: setting[field] }), false).await;
            }
        }
        if has("afkChannelId") {
            let body = json!({ "afk_channel_id": self.mapped_channel(&setting["afkChannelId"]) });
            self.apply_setting("afkChannel", body, false).await;
        }
        if has("systemChannelId") {
            let body = json!({ "system_channel_id": self.mapped_channel(&setting["systemChannelId"]) });
            self.apply_setting("systemChannel", body, false).await;
        }
        if has("systemChannelFlags") {
            let flags = system_channel_flags_from_snapshot(&setting["systemChannelFlags"])?;
            self.apply_setting("systemChannelFlags", json!({ "system_channel_flags": flags }), false).await;
        }
        if has("safetyAlertsChannelId") {
            let body = json!({ "safety_alerts_channel_id": self.mapped_channel(&setting["safetyAlertsChannelId"]) });
            self.apply_setting("safetyAlertsChannel", body, true).await;
        }
        if has("preferredLocale") {
            self.apply_setting("preferredLocale", json!({ "preferred_locale": setting["preferredLocale"] }), false).await;
        }
        if has("premiumProgressBarEnabled") {
            let body = json!({ "premium_progress_bar_enabled": setting["premiumProgressBarEnabled"] });
            self.apply_setting("premiumProgressBarEnabled", body, true).await;
        }
        self.restore_guild_assets(setting, assets).await;
        Ok(())
    }

    async fn restore_guild_assets(&mut self, setting: &Value, assets: &[GuildBackupAsset]) {
        let Some(descriptors) = setting.get("assets").filter(|v| truthy(v)) else {
            self.warnings.push("Guild image assets were not captured by this older backup and were preserved.".into());
            return;
        };
        for kind in ASSET_KINDS {
            let descriptor = &descriptors[kind];
            let key = if kind == "discovery_splash" { "discoverySplash" } else { kind };
            let asset = assets.iter().rev().find(|a| a.kind == kind);
            let value = match (descriptor["state"].as_str(), asset) {
                (Some("captured"), Some(asset)) => {
                    json!(format!("data:{};base64,{}", asset.content_type, base64::engine::general_purpose::STANDARD.encode(&asset.data)))
                }
                (Some("absent"), _) => Value::Null,
                _ => continue,
            };
            let body = json!({ kind: value });
            if let Err(error) = self.http().edit_guild(self.guild_id, &body, Some("Gaia full backup restore: guild identity asset")).await {
                let error = anyhow::Error::from(error);
                self.restore_error("guild.edit", &error, ctx! {"field" => key});
                let detail = format!("{} could not be restored — {}", kind.replacen('_', " ", 1), error_text(&error));
                if kind == "icon" {
                    self.critical.push(detail);
                } else {
                    self.warnings.push(format!("{detail}. The destination may not be eligible for this boost-dependent asset."));
                }
            }
        }
    }

    fn restore_gaia_config(&mut self, config: &Value) -> anyhow::Result<()> {
        let repo = &self.svc.repo;
        let guild = self.key.as_str();
        let map_role = |id: &Value| if truthy(id) { self.role_map.get(&js_string(id)).cloned() } else { None };
        let user_role = map_role(&config["autoroles"]["user_role_id"]);
        let bot_role = map_role(&config["autoroles"]["bot_role_id"]);
        let verification_role = map_role(&config["verification"]["role_id"]);
        let settings = &config["settings"];
        for key in ["autorole_enabled", "voice_rooms_enabled", "moderation_enabled"] {
            repo.update_setting(guild, key, sql_value(&settings[key]))?;
        }
        let ticket = self.mapped_channel(&settings["ticket_category_id"]);
        repo.update_setting(guild, "ticket_category_id", sql_value(&ticket))?;
        repo.set_autorole(guild, "user_role_id", user_role.as_deref())?;
        repo.set_autorole(guild, "bot_role_id", bot_role.as_deref())?;
        let join = self.mapped_channel(&config["joinChannelId"]);
        repo.set_join_channel(guild, join.as_str())?;
        match config["staff"].as_array() {
            Some(staff) => {
                let staff: Vec<String> = staff.iter().map(js_string).collect();
                repo.replace_staff(guild, &staff, &self.user_id)?;
            }
            None => self.warnings.push("Gaia Staff was not captured by this older backup and was preserved.".into()),
        }
        let verification = &config["verification"];
        let enabled = truthy(&verification["enabled"])
            && !(truthy(&settings["autorole_enabled"]) && user_role.is_some())
            && verification_role.is_some();
        if truthy(&verification["enabled"]) && !enabled {
            self.warnings.push(
                "Verification was disabled because restored User AutoRole configuration conflicts with Verification or its role was unavailable."
                    .into(),
            );
        }
        repo.update_verification(
            guild,
            &VerificationUpdate {
                enabled: Some(i64::from(enabled)),
                role_id: Some(verification_role),
                title: verification["title"].as_str().map(str::to_string),
                description: verification["description"].as_str().map(str::to_string),
                color: verification["color"].as_i64(),
                button_text: verification["button_text"].as_str().map(str::to_string),
                channel_id: Some(None),
                message_id: Some(None),
            },
        )?;
        Ok(())
    }

    async fn final_validate(&mut self, snapshot: &Snapshot) -> anyhow::Result<()> {
        let live = self.state().await?;
        let raw = &live.raw;
        let setting = &snapshot.guild;
        let expected = |key: &str| setting.get(key).cloned();
        let mapped = |key: &str| Some(self.mapped_channel(&setting[key]));
        let actual = |key: &str| raw.get(key).cloned();
        let identity: Vec<(&str, Option<Value>, Option<Value>)> = vec![
            ("name", expected("name"), actual("name")),
            ("description", expected("description"), actual("description")),
            ("preferred locale", expected("preferredLocale"), actual("preferred_locale")),
            ("AFK timeout", expected("afkTimeout"), actual("afk_timeout")),
            ("AFK channel", mapped("afkChannelId"), actual("afk_channel_id")),
            ("system channel", mapped("systemChannelId"), actual("system_channel_id")),
            ("system channel flags", expected("systemChannelFlags"), raw.get("system_channel_flags").map(|v| json!(js_string(v)))),
            ("rules channel", mapped("rulesChannelId"), actual("rules_channel_id")),
            ("Community Updates channel", mapped("publicUpdatesChannelId"), actual("public_updates_channel_id")),
            ("safety alerts channel", mapped("safetyAlertsChannelId"), actual("safety_alerts_channel_id")),
            ("verification level", expected("verificationLevel"), actual("verification_level")),
            ("explicit content filter", expected("explicitContentFilter"), actual("explicit_content_filter")),
            ("default notification level", expected("defaultMessageNotifications"), actual("default_message_notifications")),
        ];
        for (label, expected, actual) in identity {
            if let (Some(expected), Some(actual)) = (expected, actual) {
                if expected != actual {
                    self.critical.push(format!("Final validation: guild {label} does not match its backup."));
                }
            }
        }
        let icon_state = setting["assets"]["icon"]["state"].as_str();
        if icon_state == Some("captured") && !truthy(&raw["icon"]) {
            self.critical.push("Final validation: guild icon is missing after restoration.".into());
        }
        if icon_state == Some("absent") && truthy(&raw["icon"]) {
            self.critical.push("Final validation: guild icon should have been removed.".into());
        }
        for saved in &snapshot.roles {
            let name = js_string(&saved["name"]);
            match self.role_map.get(&js_string(&saved["id"])).and_then(|id| live.role(id)) {
                None => self.critical.push(format!("Final validation: role {name} is unavailable.")),
                Some(role) => {
                    if js_string(&role["name"]) != name || normalized_bitfield(&role["permissions"]) != js_string(&saved["permissions"]) {
                        self.critical.push(format!("Final validation: role {name} does not match its backup."));
                    }
                }
            }
        }
        let expected_roles = self.role_map.values();
        let unexpected_roles: Vec<String> = live
            .roles()
            .iter()
            .filter(|r| r["id"] != self.key.as_str() && r["managed"] != true && !expected_roles.contains(&js_string(&r["id"])))
            .map(|r| js_string(&r["name"]))
            .collect();
        if !unexpected_roles.is_empty() {
            self.critical.push(format!(
                "Final validation: {} unexpected ordinary role{} remain after restore ({}).",
                unexpected_roles.len(),
                plural(unexpected_roles.len(), "", "s"),
                unexpected_roles.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        for saved in &snapshot.channels {
            let name = js_string(&saved["name"]);
            let Some(channel) = self.channel_map.get(&js_string(&saved["id"])).and_then(|id| live.channel(id)) else {
                self.critical.push(format!("Final validation: channel {name} is unavailable."));
                continue;
            };
            let parent = &saved["parentId"];
            let parent_mismatch =
                truthy(parent) && channel["parent_id"].as_str().map(str::to_string) != self.channel_map.get(&js_string(parent)).cloned();
            if channel["type"] != saved["type"] || js_string(&channel["name"]) != name || parent_mismatch {
                self.critical.push(format!("Final validation: channel {name} does not match its backup."));
            }
            let (expected, _) = map_permission_overwrites(&saved_overwrites(saved), &self.role_map.map, &self.member_ids);
            let actual = GuildState::channel_overwrites(channel);
            if actual.len() != expected.len() {
                self.critical.push(format!("Final validation: permission overwrite count for {name} does not match its backup."));
            }
            let locked = live.permissions_locked(channel) == Some(true);
            let sync = saved["permissionSync"] == true;
            if sync && truthy(parent) && !locked {
                self.critical.push(format!("Final validation: channel {name} should be permission-synced with its category."));
            }
            if !truthy(&saved["permissionSync"]) && truthy(parent) && locked {
                self.critical.push(format!("Final validation: channel {name} should retain custom permission overwrites."));
            }
            for overwrite in &expected {
                let matches = actual.iter().find(|o| o["id"] == overwrite.id.as_str()).is_some_and(|o| {
                    o["type"].as_u64() == Some(u64::from(overwrite.kind))
                        && js_string(&o["allow"]).parse::<u128>().ok() == Some(overwrite.allow)
                        && js_string(&o["deny"]).parse::<u128>().ok() == Some(overwrite.deny)
                });
                if !matches {
                    self.critical.push(format!("Final validation: permission overwrite for {name} does not match its backup."));
                }
            }
        }
        let expected_channels = self.channel_map.values();
        let unremovable_ids: HashSet<&String> = self.unremovable.iter().map(|(id, _)| id).collect();
        let unexpected_channels: Vec<String> = live
            .channels
            .iter()
            .filter(|c| {
                let id = js_string(&c["id"]);
                is_supported(c) && !expected_channels.contains(&id) && !unremovable_ids.contains(&id)
            })
            .map(|c| js_string(&c["name"]))
            .collect();
        for (channel_id, reason) in self.unremovable.clone() {
            if let Some(channel) = live.channel(&channel_id) {
                self.warnings.push(format!("Final validation: stale channel {} remains because {reason}.", js_string(&channel["name"])));
            }
        }
        if !unexpected_channels.is_empty() {
            self.critical.push(format!(
                "Final validation: {} unexpected channel or category{} remain after restore ({}).",
                unexpected_channels.len(),
                plural(unexpected_channels.len(), "", "ies"),
                unexpected_channels.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if let Some(community) = snapshot.guild["communityEnabled"].as_bool() {
            if live.features().iter().any(|f| f == "COMMUNITY") != community {
                self.critical.push(format!(
                    "Final validation: Community state should be {}.",
                    if community { "enabled" } else { "disabled" }
                ));
            }
        }
        let repo = &self.svc.repo;
        let settings = repo.settings(&self.key)?;
        let autoroles = repo.autoroles(&self.key)?;
        let join = repo.join_channel(&self.key)?;
        let verification = repo.verification(&self.key)?;
        for role_id in [autoroles.user_role_id, autoroles.bot_role_id, verification.role_id].into_iter().flatten() {
            if !role_id.is_empty() && live.role(&role_id).is_none() {
                self.warnings.push("Final validation: a Gaia role reference is unavailable.".into());
            }
        }
        for channel_id in [settings.ticket_category_id, join].into_iter().flatten() {
            if !channel_id.is_empty() && live.channel(&channel_id).is_none() {
                self.warnings.push("Final validation: a Gaia channel reference is unavailable.".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> GaiaRepository {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        GaiaRepository::new(conn)
    }

    fn backup(id: &str, guild_id: &str, created_at: &str) -> GuildBackup {
        let value = json!({
            "schemaVersion": BACKUP_SCHEMA_VERSION,
            "guild": { "id": guild_id, "name": "Source" },
            "roles": [], "channels": [], "members": [], "cookie": {},
        })
        .to_string();
        GuildBackup {
            backup_id: id.into(),
            source_guild_id: guild_id.into(),
            source_guild_name: "Source".into(),
            created_at: created_at.into(),
            created_by: Some("admin".into()),
            creation_type: "manual".into(),
            snapshot_schema_version: BACKUP_SCHEMA_VERSION,
            snapshot_size: value.len() as i64,
            snapshot_json: value,
            snapshot_hash: String::new(),
            ..Default::default()
        }
    }

    fn ids(r: &GaiaRepository) -> Vec<String> {
        r.backups("a").unwrap().into_iter().map(|b| b.backup_id).collect()
    }

    #[test]
    fn rotates_only_after_inserting_a_successful_third_backup() {
        let r = repo();
        for (id, at) in [("one", "2026-01-01T00:00:00.000Z"), ("two", "2026-01-02T00:00:00.000Z"), ("three", "2026-01-03T00:00:00.000Z")] {
            r.store_backup(&backup(id, "a", at), None, None).unwrap();
        }
        assert_eq!(ids(&r), ["three", "two"]);
    }

    #[test]
    fn retains_restore_referenced_history() {
        let r = repo();
        r.store_backup(&backup("one", "a", "2026-01-01T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("two", "a", "2026-01-02T00:00:00.000Z"), None, None).unwrap();
        r.create_restore_operation(&NewRestoreOperation {
            restore_id: "restore-one".into(),
            backup_id: "one".into(),
            destination_guild_id: "b".into(),
            requested_by: "admin".into(),
            status: "completed".into(),
            current_phase: "complete".into(),
            progress_json: "{}".into(),
            errors_json: "[]".into(),
            ..Default::default()
        })
        .unwrap();
        r.store_backup(&backup("three", "a", "2026-01-03T00:00:00.000Z"), None, None).unwrap();
        assert_eq!(ids(&r), ["three", "two", "one"]);
    }

    #[test]
    fn retains_a_selected_source_backup_during_a_same_guild_safety_backup() {
        let r = repo();
        r.store_backup(&backup("source", "a", "2026-01-01T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("other", "a", "2026-01-02T00:00:00.000Z"), None, None).unwrap();
        r.store_backup(&backup("safety", "a", "2026-01-03T00:00:00.000Z"), Some("source"), None).unwrap();
        assert_eq!(ids(&r), ["safety", "source"]);
    }

    #[test]
    fn rejects_an_integrity_mismatched_snapshot() {
        let mut bad = backup("bad", "a", "2026-01-01T00:00:00.000Z");
        bad.snapshot_hash = "not-a-hash".into();
        assert!(load(&bad).unwrap_err().to_string().contains("integrity"));
        bad.snapshot_hash = sha256_hex(bad.snapshot_json.as_bytes());
        assert!(load(&bad).is_ok());
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn creates_one_missed_ljubljana_daily_backup_without_duplicate_retries() {
        let mut settings = BackupSettings { automatic_enabled: 1, ..Default::default() };
        assert!(automatic_backup_due(&settings, at("2026-08-26T03:30:00.000Z")));
        settings.last_success_at = Some("2026-08-26T01:00:00.000Z".into());
        assert!(!automatic_backup_due(&settings, at("2026-08-26T04:00:00.000Z")));
        assert!(automatic_backup_due(&settings, at("2026-08-27T01:30:00.000Z")));
        settings.automatic_enabled = 0;
        assert!(!automatic_backup_due(&settings, at("2026-08-27T01:30:00.000Z")));
    }

    #[test]
    fn does_not_run_a_daily_backup_before_03_00_ljubljana() {
        let settings = BackupSettings { automatic_enabled: 1, ..Default::default() };
        assert!(!automatic_backup_due(&settings, at("2026-08-26T00:59:00.000Z")));
        assert!(automatic_backup_due(&settings, at("2026-08-26T01:00:00.000Z")));
    }

    #[test]
    fn maps_every_overwrite_through_destination_ids_and_preserves_precision() {
        let large = "900719925474099312345";
        let ow = |id: &str, kind: u8, allow: &str, deny: &str| SavedOverwrite { id: id.into(), kind, allow: allow.into(), deny: deny.into() };
        let role_map: HashMap<String, String> =
            [("source-everyone", "destination-everyone"), ("source-role", "destination-role")].map(|(a, b)| (a.into(), b.into())).into();
        let members: HashSet<String> = ["present-member".to_string()].into();
        let (mapped, skipped) = map_permission_overwrites(
            &[ow("source-everyone", 0, large, "7"), ow("source-role", 0, "3", large), ow("present-member", 1, "5", "6"), ow("missing-member", 1, "1", "2")],
            &role_map,
            &members,
        );
        let big: u128 = large.parse().unwrap();
        let m = |id: &str, kind: u8, allow: u128, deny: u128| MappedOverwrite { id: id.into(), kind, allow, deny };
        assert_eq!(skipped, 1);
        assert_eq!(mapped, [m("destination-everyone", 0, big, 7), m("destination-role", 0, 3, big), m("present-member", 1, 5, 6)]);
        assert!(!mapped.iter().any(|e| e.id.starts_with("source-")));
    }

    #[test]
    fn groups_restore_warnings_into_compact_categories() {
        let warnings: Vec<String> = [
            "Managed integration roles were excluded.",
            "Protected channel could not be deleted.",
            "Role hierarchy exceeds Gaia's highest role.",
            "Community feature is controlled by Discord.",
            "An unsupported property was not captured.",
            "A miscellaneous warning.",
        ]
        .map(String::from)
        .into();
        let groups: Vec<(&str, usize)> = group_restore_warnings(&warnings).iter().map(|g| (g.category, g.warnings.len())).collect();
        assert_eq!(
            groups,
            [
                ("Managed/integration roles", 1),
                ("Protected channels", 1),
                ("Hierarchy limitations", 1),
                ("Discord-controlled guild features", 1),
                ("Unsupported properties", 1),
                ("Other", 1)
            ]
        );
    }

    #[test]
    fn system_channel_flags_round_trip_and_reject_unsupported() {
        assert_eq!(system_channel_flags_from_snapshot(&json!("0")).unwrap(), 0);
        assert_eq!(system_channel_flags_from_snapshot(&json!("1")).unwrap(), 1);
        assert_eq!(system_channel_flags_from_snapshot(&json!("13")).unwrap(), 13);
        assert_eq!(system_channel_flags_from_snapshot(&json!("63")).unwrap(), 63);
        assert_eq!(system_channel_flags_from_snapshot(&json!(0)).unwrap(), 0);
        assert_eq!(system_channel_flags_from_snapshot(&json!("10")).unwrap().to_string(), "10");
        for bad in [json!("not-a-bitfield"), json!("-1"), json!(-1), json!(null)] {
            assert!(system_channel_flags_from_snapshot(&bad).unwrap_err().to_string().contains("systemChannelFlags"));
        }
        assert!(system_channel_flags_from_snapshot(&json!("64")).unwrap_err().to_string().contains("unsupported"));
    }

    #[test]
    fn never_sends_undefined_or_legacy_numeric_role_colors() {
        let role = |extra: Value| {
            let mut base = json!({ "id": "source-role", "name": "Staff", "permissions": "900719925474099312345", "hoist": false, "mentionable": true });
            base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            base
        };
        let no_color = role_create_options(&role(json!({})), true);
        assert!(no_color.get("colors").is_none());
        let standard = role_create_options(&role(json!({ "color": 0x5865f2 })), true);
        assert_eq!(standard["colors"], json!({ "primary_color": 0x5865f2 }));
        assert_eq!(standard["permissions"], "900719925474099312345");
        assert_eq!(role_create_options(&role(json!({ "color": 0x5865f2 })), false).get("colors"), None);
    }

    #[test]
    fn preserves_enhanced_role_colors_and_omits_malformed_data() {
        assert_eq!(
            role_colors_from_snapshot(&json!({ "colors": { "primaryColor": 11127295, "secondaryColor": 16759788, "tertiaryColor": 16761760 } })),
            Some(SavedRoleColors { primary_color: 11127295, secondary_color: Some(Some(16759788)), tertiary_color: Some(Some(16761760)) })
        );
        assert_eq!(role_colors_from_snapshot(&json!({})), None);
        assert_eq!(role_colors_from_snapshot(&json!({ "colors": 0x5865f2 })), None);
        assert_eq!(
            role_colors_from_snapshot(&json!({ "colors": { "primaryColor": 1, "secondaryColor": null } })),
            Some(SavedRoleColors { primary_color: 1, secondary_color: Some(None), tertiary_color: None })
        );
    }

    #[test]
    fn rejects_malformed_snapshots_before_restore() {
        let valid = json!({
            "schemaVersion": BACKUP_SCHEMA_VERSION,
            "guild": { "id": "source", "everyonePermissions": "0" },
            "roles": [{ "id": "role", "name": "Staff", "permissions": "0" }],
            "channels": [{ "id": "category", "type": 4, "name": "Private", "parentId": null, "overwrites": [] }],
            "members": [{ "userId": "member", "roleIds": ["role"] }],
            "cookie": {},
        });
        assert!(validate(&valid).is_ok());
        let fails = |patch: &dyn Fn(&mut Value), text: &str| {
            let mut v = valid.clone();
            patch(&mut v);
            let error = validate(&v).unwrap_err().to_string();
            assert!(error.contains(text), "{error} should contain {text}");
        };
        fails(&|v| v["roles"][0]["permissions"] = json!("not-a-bitfield"), "role");
        fails(&|v| v["channels"][0]["overwrites"] = json!([{ "id": "x", "type": 0, "allow": "1.5", "deny": "0" }]), "overwrite");
        fails(&|v| v["channels"][0]["parentId"] = json!("missing"), "channel");
        fails(&|v| v["guild"]["systemChannelFlags"] = json!("invalid"), "system channel flags");
        fails(&|v| v["members"][0]["roleIds"] = json!(["ghost"]), "member assignments");
        fails(&|v| v["schemaVersion"] = json!(3), "schema");
    }

    #[test]
    fn uses_utf8_byte_boundaries_for_the_size_limit() {
        let under = "€".repeat((MAX_BACKUP_BYTES - 3) / 3);
        let over = format!("{under}€€");
        assert!(under.len() <= MAX_BACKUP_BYTES);
        assert!(over.len() > MAX_BACKUP_BYTES);
    }

    #[test]
    fn aborts_an_older_incomplete_announcement_channel_backup() {
        let snapshot = Snapshot {
            schema_version: 1,
            guild: json!({}),
            roles: vec![],
            channels: vec![json!({ "type": 5 })],
            members: vec![],
            gaia: Value::Null,
            unsupported: None,
        };
        assert!(assert_reconstructable(&snapshot).unwrap_err().to_string().contains("did not capture Community state"));
    }

    #[test]
    fn rejects_a_corrupt_stored_icon() {
        let snapshot = Snapshot {
            schema_version: 2,
            guild: json!({ "assets": { "icon": { "state": "captured", "sha256": "bad", "sizeBytes": 3 } } }),
            roles: vec![],
            channels: vec![],
            members: vec![],
            gaia: Value::Null,
            unsupported: None,
        };
        let asset = GuildBackupAsset { kind: "icon".into(), sha256: "bad".into(), size_bytes: 3, data: b"bad".to_vec(), ..Default::default() };
        assert!(validate_assets(&snapshot, std::slice::from_ref(&asset)).unwrap_err().to_string().contains("icon asset integrity"));
        let good = GuildBackupAsset { sha256: sha256_hex(b"bad"), ..asset };
        let mut snapshot = snapshot;
        snapshot.guild["assets"]["icon"]["sha256"] = json!(good.sha256);
        assert!(validate_assets(&snapshot, &[good]).is_ok());
    }

    #[test]
    fn validates_a_large_synthetic_snapshot_without_numeric_conversion() {
        let snapshot = json!({
            "schemaVersion": BACKUP_SCHEMA_VERSION,
            "guild": { "id": "source", "everyonePermissions": "0" },
            "roles": (0..100).map(|i| json!({ "id": format!("r{i}"), "name": format!("Staff {}", i % 3), "permissions": "900719925474099312345" })).collect::<Vec<_>>(),
            "channels": (0..500).map(|i| json!({ "id": format!("c{i}"), "type": 0, "name": format!("general-{}", i % 3), "parentId": null,
                "overwrites": [{ "id": "source", "type": 0, "allow": "900719925474099312345", "deny": "0" }] })).collect::<Vec<_>>(),
            "members": (0..5_000).map(|i| json!({ "userId": format!("u{i}"), "roleIds": if i % 2 == 1 { vec!["r1"] } else { vec![] } })).collect::<Vec<_>>(),
            "cookie": {},
        });
        let started = Instant::now();
        assert!(validate(&snapshot).is_ok());
        let text = snapshot.to_string();
        assert_eq!(sha256_hex(text.as_bytes()).len(), 64);
        assert!(text.len() < MAX_BACKUP_BYTES);
        assert!(parse_snapshot(snapshot).is_ok());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn error_text_is_sanitized_and_bounded() {
        assert_eq!(error_text(&anyhow::anyhow!("a\nb")), "Discord error: a b");
        assert_eq!(error_text_str(None), "Discord error: unknown Discord error");
        assert_eq!(human_duration(61_000), "1m 1s");
    }

    #[test]
    fn builds_discord_channel_and_guild_bodies() {
        let field = "afk_timeout";
        assert_eq!(json!({ field: 300 }), json!({ "afk_timeout": 300 }));
        let forum = json!({ "type": 15, "name": "f", "topic": null, "nsfw": false, "rateLimitPerUser": 0, "availableTags": [{ "name": "t", "moderated": true, "emojiId": null, "emojiName": "x" }] });
        assert_eq!(
            channel_options(&forum, Some("9")),
            json!({ "name": "f", "type": 15, "parent_id": "9", "nsfw": false, "rate_limit_per_user": 0,
                "available_tags": [{ "name": "t", "moderated": true, "emoji_id": null, "emoji_name": "x" }] })
        );
    }
}
