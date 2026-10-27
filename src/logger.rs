//! Structured JSON logs on stdout; secrets (webhook URLs, bot tokens) are redacted.
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

static WEBHOOK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://[^\s/]+)?/api/(?:v\d+/)?webhooks/\d{17,20}/[A-Za-z0-9_\-.]+").unwrap()
});
static TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z\d_-]{20,}\.[A-Za-z\d_-]{6,}\.[A-Za-z\d_-]{20,}").unwrap());

pub fn redact(value: &str) -> String {
    let value = WEBHOOK.replace_all(value, "[redacted webhook]");
    TOKEN.replace_all(&value, "[redacted token]").into_owned()
}

/// Log context: `ctx!{"guildId" => id, "count" => 3}`. `None` values are dropped.
pub type Ctx = Vec<(&'static str, Value)>;

#[macro_export]
macro_rules! ctx {
    () => { Vec::<(&'static str, serde_json::Value)>::new() };
    ($($key:expr => $value:expr),+ $(,)?) => {
        vec![$(($key, serde_json::json!($value))),+]
    };
}

fn write(level: &str, message: &str, context: Ctx) {
    let mut out = Map::new();
    out.insert("timestamp".into(), chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true).into());
    out.insert("level".into(), level.into());
    out.insert("message".into(), message.into());
    for (key, value) in context {
        match value {
            Value::Null => {}
            Value::String(text) => {
                out.insert(key.into(), Value::String(redact(&text)));
            }
            other => {
                out.insert(key.into(), other);
            }
        }
    }
    println!("{}", Value::Object(out));
}

pub fn info(message: &str, context: Ctx) { write("info", message, context) }
pub fn warn(message: &str, context: Ctx) { write("warn", message, context) }
pub fn error(message: &str, context: Ctx) { write("error", message, context) }

/// Discord JSON error code of a serenity error, e.g. 10007 Unknown Member.
pub fn discord_code(error: &serenity::Error) -> Option<isize> {
    match error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => Some(response.error.code),
        _ => None,
    }
}

pub fn discord_status(error: &serenity::Error) -> Option<u16> {
    match error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => Some(response.status_code.as_u16()),
        _ => None,
    }
}

/// Keeps Discord REST diagnostics structured without printing embed content or tokens.
/// Accepts any error; Discord code/status are extracted when it wraps a serenity HTTP error.
pub fn log_discord_error(operation: &str, error: &anyhow::Error, context: Ctx) {
    let serenity = error.downcast_ref::<serenity::Error>();
    let message: String = WEBHOOK
        .replace_all(&error.to_string().replace(['\r', '\n'], " "), "[redacted webhook]")
        .chars()
        .take(300)
        .collect();
    let mut all: Ctx = vec![
        ("operation", operation.into()),
        ("code", serenity.and_then(discord_code).map(|c| Value::String(c.to_string())).unwrap_or(Value::Null)),
        ("status", serenity.and_then(discord_status).map(Value::from).unwrap_or(Value::Null)),
        ("discordMessage", message.into()),
    ];
    all.extend(context);
    write("error", "[discord] operation failed", all);
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redacts_webhooks_and_tokens() {
        let out = redact("https://discord.com/api/webhooks/123456789012345678/abc-DEF_1 and MTIzNDU2Nzg5MDEyMzQ1Njc4OTA.GhIjKl.abcdefghijklmnopqrstuvwxyz12");
        assert_eq!(out, "[redacted webhook] and [redacted token]");
    }

    #[test]
    fn redacts_versioned_webhook_urls() {
        let out = redact("POST https://discord.com/api/v10/webhooks/123456789012345678/secret-Token_9 failed");
        assert_eq!(out, "POST [redacted webhook] failed");
    }

    #[test]
    fn config_debug_never_prints_secrets() {
        let config = crate::config::Config {
            token: "MTIzNDU2Nzg5MDEyMzQ1Njc4OTA.GhIjKl.abcdefghijklmnopqrstuvwxyz12".into(),
            database_path: "./data/gaia.sqlite".into(),
            application_id: Some("1".into()),
            audit_logging: crate::config::AuditLogging { webhook_url: Some("https://x/api/webhooks/1/secret-hook".into()), ..Default::default() },
            member_logging: crate::config::MemberLogging { webhook_url: Some("https://x/api/webhooks/2/secret-hook".into()), ..Default::default() },
        };
        let printed = format!("{config:?}");
        assert!(!printed.contains("GhIjKl") && !printed.contains("secret-hook"), "{printed}");
        assert!(printed.contains("./data/gaia.sqlite"));
    }
}
