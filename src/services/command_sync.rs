use crate::db::repositories::GaiaRepository;
use crate::discord::Discord;
use crate::logger;
use anyhow::{anyhow, bail};
use serde::Serialize;
use serde_json::{json, Map, Value};
use serenity::http::{LightMethod, Request, Route};
use serenity::model::id::{ApplicationId, GuildId};
use sha2::{Digest, Sha256};
use std::sync::{Arc, LazyLock};

pub const COMMANDS_JSON: &str = include_str!("../../shared/commands.json");

/// The command schema (`shared/commands.json`).
pub static COMMANDS: LazyLock<Vec<Value>> = LazyLock::new(|| serde_json::from_str(COMMANDS_JSON).expect("shared/commands.json is valid JSON"));

/// Recursively sorts object keys (byte order). serde_json maps are BTreeMaps, so a
/// round-trip through `Value` already sorts; this is explicit for clarity.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            Value::Object(entries.into_iter().map(|(k, v)| (k.clone(), canonicalize(v))).collect::<Map<_, _>>())
        }
        other => other.clone(),
    }
}

pub fn command_schema_fingerprint_of(definitions: &[Value]) -> String {
    let canonical = serde_json::to_string(&canonicalize(&Value::Array(definitions.to_vec()))).unwrap();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

pub fn command_schema_fingerprint() -> String {
    command_schema_fingerprint_of(&COMMANDS)
}

pub fn command_names() -> Vec<String> {
    COMMANDS.iter().filter_map(|c| c["name"].as_str().map(String::from)).collect()
}

pub fn assert_command_schema() -> anyhow::Result<()> {
    if command_names() != ["admin", "embed", "mute", "unmute", "unverify"] {
        bail!("Gaia command schema must contain exactly admin, embed, mute, unmute, and unverify.");
    }
    if let Some(admin) = COMMANDS.iter().find(|c| c["name"] == "admin") {
        if admin["options"].as_array().is_some_and(|o| !o.is_empty()) {
            bail!("admin must not have subcommands or options.");
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveCommand {
    pub name: String,
    pub id: Option<String>,
    pub application_id: Option<String>,
    pub guild_id: Option<String>,
    pub default_member_permissions: Option<String>,
    pub dm_permission: Option<bool>,
    pub contexts: Option<Value>,
    pub integration_types: Option<Value>,
    pub nsfw: bool,
    pub options: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncOutcome {
    pub application_id: String,
    pub fingerprint: String,
    pub cleaned: u32,
    pub failed: u32,
}

fn opt_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Raw Discord command JSON -> `LiveCommand`.
pub fn live_commands(values: &[Value], guild_id: Option<&str>) -> Vec<LiveCommand> {
    values
        .iter()
        .map(|c| LiveCommand {
            name: c["name"].as_str().unwrap_or_default().to_string(),
            id: opt_string(&c["id"]),
            application_id: opt_string(&c["application_id"]),
            guild_id: opt_string(&c["guild_id"]).or_else(|| guild_id.map(String::from)),
            default_member_permissions: opt_string(&c["default_member_permissions"]),
            dm_permission: c["dm_permission"].as_bool(),
            contexts: Some(c["contexts"].clone()).filter(|v| !v.is_null()),
            integration_types: Some(c["integration_types"].clone()).filter(|v| !v.is_null()),
            nsfw: c["nsfw"].as_bool().unwrap_or(false),
            options: if c["options"].is_null() { json!([]) } else { c["options"].clone() },
        })
        .collect()
}

/// Strips fields Discord adds or omits for defaults (nulls, empty arrays, `false`
/// flags such as `required`/`autocomplete`, localizations) so raw API options compare
/// semantically with `shared/commands.json`.
fn normalize_options(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(normalize_options).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, v)| {
                    !(v.is_null() || *v == &Value::Bool(false) || v.as_array().is_some_and(Vec::is_empty) || key.contains("localization") || key.ends_with("_localized"))
                })
                .map(|(k, v)| (k.clone(), normalize_options(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn verify_live_global_commands(live: &[LiveCommand]) -> anyhow::Result<()> {
    let expected_all = &*COMMANDS;
    if live.len() != expected_all.len() || live.iter().any(|c| !expected_all.iter().any(|e| e["name"] == c.name.as_str())) {
        bail!("Live global command set does not match Gaia command schema.");
    }
    for expected in expected_all {
        let name = expected["name"].as_str().unwrap_or_default();
        let received = live.iter().find(|c| c.name == name).expect("checked above");
        let expected_options = if expected["options"].is_null() { json!([]) } else { expected["options"].clone() };
        let null_or = |v: &Value| Some(v.clone()).filter(|v| !v.is_null());
        if received.default_member_permissions != opt_string(&expected["default_member_permissions"])
            || canonicalize(&normalize_options(&received.options)) != canonicalize(&normalize_options(&expected_options))
            || received.contexts != null_or(&expected["contexts"])
            || received.integration_types != null_or(&expected["integration_types"])
        {
            bail!("Live global command metadata does not match Gaia schema for /{name}.");
        }
    }
    Ok(())
}

pub struct CommandSynchronizer {
    repo: Arc<GaiaRepository>,
    configured_application_id: Option<String>,
}

impl CommandSynchronizer {
    pub fn new(repo: Arc<GaiaRepository>, configured_application_id: Option<String>) -> Self {
        Self { repo, configured_application_id }
    }

    pub async fn synchronize(&self, discord: &Discord, force_cleanup: bool) -> anyhow::Result<SyncOutcome> {
        assert_command_schema()?;
        let application_id = self.application_id(discord)?;
        let fingerprint = command_schema_fingerprint();
        logger::info("[commands] command schema loaded", crate::ctx! {"applicationId" => application_id, "schema" => &fingerprint[..12], "scope" => "global"});
        logger::info("[commands] synchronizing global application commands", crate::ctx! {"count" => COMMANDS.len()});
        discord.http.set_application_id(parse_id(&application_id)?);
        discord.http.create_global_commands(&*COMMANDS).await?;
        let live = live_commands(&self.raw_global_commands(discord, &application_id).await?, None);
        verify_live_global_commands(&live)?;
        logger::info("[commands] global commands synchronized", crate::ctx! {"count" => COMMANDS.len()});
        let state = self.repo.command_sync_state(&application_id)?;
        let cleanup_required = force_cleanup || state.as_ref().is_none_or(|s| s.schema_fingerprint != fingerprint || s.guild_cleanup_complete == 0);
        if !cleanup_required {
            return Ok(SyncOutcome { application_id, fingerprint, cleaned: 0, failed: 0 });
        }
        let (cleaned, failed) = self.clear_legacy_guild_commands(discord, &discord.cache.guilds()).await;
        self.repo.save_command_sync_state(&application_id, &fingerprint, failed == 0)?;
        Ok(SyncOutcome { application_id, fingerprint, cleaned, failed })
    }

    pub async fn clear_guild_on_join(&self, discord: &Discord, guild_id: GuildId) {
        let (_, failed) = self.clear_legacy_guild_commands(discord, &[guild_id]).await;
        if failed > 0 {
            logger::warn("[commands] joined guild legacy command cleanup failed", crate::ctx! {"guildId" => guild_id.to_string()});
        }
    }

    fn application_id(&self, discord: &Discord) -> anyhow::Result<String> {
        let application_id = discord
            .http
            .application_id()
            .map(|id| id.to_string())
            .or_else(|| Some(discord.cache.current_user().id).filter(|id| id.get() > 1).map(|id| id.to_string()))
            .ok_or_else(|| anyhow!("Discord application identity is unavailable after ready."))?;
        if self.configured_application_id.as_ref().is_some_and(|configured| *configured != application_id) {
            bail!("Configured application ID does not match authenticated Gaia application.");
        }
        Ok(application_id)
    }

    async fn raw_global_commands(&self, discord: &Discord, application_id: &str) -> anyhow::Result<Vec<Value>> {
        let application_id = parse_id::<ApplicationId>(application_id)?;
        Ok(discord.http.fire(Request::new(Route::Commands { application_id }, LightMethod::Get)).await?)
    }

    async fn clear_legacy_guild_commands(&self, discord: &Discord, guilds: &[GuildId]) -> (u32, u32) {
        logger::info("[commands] clearing legacy guild commands", crate::ctx! {"guilds" => guilds.len()});
        let (mut cleaned, mut failed) = (0, 0);
        for &guild_id in guilds {
            let result: anyhow::Result<bool> = async {
                let existing = discord.http.get_guild_commands(guild_id).await?;
                if existing.is_empty() {
                    return Ok(false);
                }
                discord.http.create_guild_commands(guild_id, &json!([])).await?;
                Ok(true)
            }
            .await;
            match result {
                Ok(true) => cleaned += 1,
                Ok(false) => {}
                Err(_) => {
                    failed += 1;
                    logger::warn("[commands] guild command cleanup failed", crate::ctx! {"guildId" => guild_id.to_string()});
                }
            }
        }
        logger::info("[commands] guild command cleanup complete", crate::ctx! {"cleaned" => cleaned, "failed" => failed});
        (cleaned, failed)
    }
}

fn parse_id<T: From<u64>>(id: &str) -> anyhow::Result<T> {
    id.parse::<u64>().ok().filter(|&n| n != 0).map(T::from).ok_or_else(|| anyhow!("Invalid Discord ID: {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::model::Permissions;
    const ADMIN_LABEL: &str = "𝗔𝗱𝗺𝗶𝗻";
    const STAFF_LABEL: &str = "𝗦𝘁𝗮𝗳𝗳";

    fn command(name: &str) -> &'static Value {
        COMMANDS.iter().find(|c| c["name"] == name).unwrap()
    }

    #[test]
    fn fingerprint_is_deterministic_and_matches_ruby_tooling() {
        assert_eq!(command_schema_fingerprint(), command_schema_fingerprint());
        assert_eq!(command_schema_fingerprint(), "1e98fe5bf84ba2fb43cdc8d2beeb2d3840053d0c263b5295e09d922242a89a32");
        assert!(assert_command_schema().is_ok());
    }

    #[test]
    fn uses_exact_labels_and_no_longer_registers_role() {
        assert!(!command_names().contains(&"role".to_string()));
        assert!(command("admin")["description"].as_str().unwrap().starts_with(ADMIN_LABEL));
        assert!(command("mute")["description"].as_str().unwrap().starts_with(STAFF_LABEL));
        assert_eq!(command("unverify")["description"], format!("{ADMIN_LABEL} Unverify a member."));
    }

    #[test]
    fn keeps_admin_and_embed_zero_option() {
        assert_eq!(command("admin")["options"], json!([]));
        assert_eq!(command("embed")["options"], json!([]));
        assert_eq!(command("unverify")["options"].as_array().unwrap().len(), 1);
        assert_eq!(command_names(), ["admin", "embed", "mute", "unmute", "unverify"]);
    }

    #[test]
    fn uses_default_permissions_to_hide_restricted_commands() {
        for name in ["admin", "embed", "unverify"] {
            assert_eq!(command(name)["default_member_permissions"], Permissions::ADMINISTRATOR.bits().to_string());
        }
        for name in ["mute", "unmute"] {
            assert_eq!(command(name)["default_member_permissions"], Permissions::MODERATE_MEMBERS.bits().to_string());
        }
        for c in COMMANDS.iter() {
            assert_eq!(c["integration_types"], json!([0]));
            assert_eq!(c["contexts"], json!([0]));
        }
    }

    fn live_set() -> Vec<Value> {
        COMMANDS
            .iter()
            .map(|c| {
                let mut c = c.clone();
                c["id"] = json!(format!("id-{}", c["name"].as_str().unwrap()));
                c["application_id"] = json!("app");
                c
            })
            .collect()
    }

    #[test]
    fn verifies_the_live_global_result() {
        let live = live_commands(&live_set(), None);
        verify_live_global_commands(&live).unwrap();
    }

    #[test]
    fn tolerates_discord_default_option_fields() {
        let mut raw = live_set();
        for c in &mut raw {
            for option in c["options"].as_array_mut().unwrap() {
                let obj = option.as_object_mut().unwrap();
                if obj.get("required") == Some(&json!(false)) {
                    obj.remove("required");
                }
                obj.insert("description_localizations".into(), Value::Null);
            }
        }
        verify_live_global_commands(&live_commands(&raw, None)).unwrap();
    }

    #[test]
    fn fails_on_mismatched_live_visibility_metadata() {
        let mut raw = live_set();
        raw[0]["integration_types"] = json!([1]);
        let error = verify_live_global_commands(&live_commands(&raw, None)).unwrap_err();
        assert!(error.to_string().contains("/admin"));
    }

}
