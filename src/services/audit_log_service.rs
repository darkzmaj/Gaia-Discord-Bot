//! Best-effort audit delivery into Discord channels chosen in `/admin` → **Logs** (see `log_routing`).
//! `record` never fails or blocks: a missing or unwritable log channel cannot affect Gaia's work.
use crate::logger;
use crate::services::log_routing::{LogRouter, ACTIVITY};
use regex::Regex;
use serde_json::{json, Value};
use serenity::all::{ChannelId, GuildId};
use serenity::http::Http;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::mpsc;

/// A subject (guild, actor, target, channel) of an audit event.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct AuditSubject {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl AuditSubject {
    pub fn named(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self { id: Some(id.into()), name: Some(name.into()) }
    }
    pub fn id(id: impl Into<String>) -> Self {
        Self { id: Some(id.into()), name: None }
    }
}

/// `color`: "success" | "warning" | "danger" | "info" (empty = info). `priority`: "low" | "normal" | "critical".
/// `details` keep insertion order; a `Value::Null` detail is omitted from the embed.
#[derive(Default, Clone, Debug)]
pub struct AuditEvent {
    pub title: String,
    pub category: String,
    pub color: String,
    pub priority: String,
    pub guild: Option<AuditSubject>,
    pub actor: Option<AuditSubject>,
    pub target: Option<AuditSubject>,
    pub channel: Option<AuditSubject>,
    pub reason: Option<String>,
    pub details: Vec<(String, Value)>,
}

/// Removes credentials and control characters before anything leaves Gaia. `limit` counts chars.
pub fn sanitize_audit_text(value: &str, limit: usize) -> String {
    static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\r\n]+").unwrap());
    NEWLINES.replace_all(&logger::redact(value), " ").chars().take(limit).collect()
}

/// The server an event belongs to; bot health ("Bot" category) belongs to none and goes to the log server.
fn source(event: &AuditEvent) -> Option<GuildId> {
    if event.category == "Bot" {
        return None;
    }
    event.guild.as_ref()?.id.as_deref()?.parse::<u64>().ok().filter(|g| *g != 0).map(GuildId::new)
}

/// Delivers events in order through one worker. Cheap to clone.
#[derive(Clone)]
pub struct AuditLogService {
    router: Arc<LogRouter>,
    queue: mpsc::UnboundedSender<(ChannelId, Value)>,
    pending: Arc<AtomicUsize>,
}

impl AuditLogService {
    pub fn new(router: Arc<LogRouter>, http: Arc<Http>) -> Self {
        let (queue, mut rx) = mpsc::unbounded_channel::<(ChannelId, Value)>();
        let pending = Arc::new(AtomicUsize::new(0));
        let done = pending.clone();
        tokio::spawn(async move {
            while let Some((channel, body)) = rx.recv().await {
                if let Err(error) = http.send_message(channel, vec![], &body).await {
                    logger::warn("[logging] audit delivery failed", crate::ctx! {"channelId" => channel.to_string(), "status" => logger::discord_code(&error)});
                }
                done.fetch_sub(1, Ordering::SeqCst);
            }
        });
        Self { router, queue, pending }
    }

    pub fn router(&self) -> &Arc<LogRouter> {
        &self.router
    }

    pub fn record(&self, event: AuditEvent) {
        let source = source(&event);
        self.deliver(source, ACTIVITY, json!({"embeds": [render_event(event)], "allowed_mentions": {"parse": []}}));
    }

    /// Queues a message for wherever `kind` logs from `source` go (nowhere when none is set).
    pub fn deliver(&self, source: Option<GuildId>, kind: &str, body: Value) {
        let Some(channel) = self.router.channel(source, kind) else { return };
        self.pending.fetch_add(1, Ordering::SeqCst);
        if self.queue.send((channel, body)).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Resolves once every recorded event has been sent (or has failed).
    pub async fn idle(&self) {
        while self.pending.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn shutdown(&self, timeout: Duration) {
        if tokio::time::timeout(timeout, self.idle()).await.is_err() {
            logger::warn("[logging] audit queue flush timed out", crate::ctx! {"dropped" => self.pending.load(Ordering::SeqCst)});
        }
    }
}

/// The sanitized embed for one event.
pub fn render_event(event: AuditEvent) -> Value {
    audit_embed(&safe_event(event))
}

fn safe_event(event: AuditEvent) -> AuditEvent {
    let subject = |value: Option<AuditSubject>| {
        value.map(|s| AuditSubject {
            id: s.id.filter(|v| !v.is_empty()).map(|v| sanitize_audit_text(&v, 40)),
            name: s.name.filter(|v| !v.is_empty()).map(|v| sanitize_audit_text(&v, 160)),
        })
    };
    AuditEvent {
        title: sanitize_audit_text(&event.title, 256),
        category: sanitize_audit_text(&event.category, 80),
        color: event.color,
        priority: event.priority,
        guild: subject(event.guild),
        actor: subject(event.actor),
        target: subject(event.target),
        channel: subject(event.channel),
        reason: event.reason.filter(|r| !r.is_empty()).map(|r| sanitize_audit_text(&r, 900)),
        details: event
            .details
            .into_iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::String(text) => Value::String(sanitize_audit_text(&text, 500)),
                    other => other,
                };
                (sanitize_audit_text(&key, 80), value)
            })
            .collect(),
    }
}

fn color_value(color: &str) -> u32 {
    match color {
        "success" => 0x57f287,
        "warning" => 0xfee75c,
        "danger" => 0xed4245,
        _ => crate::discord::GAIA_PURPLE,
    }
}

fn audit_embed(event: &AuditEvent) -> Value {
    let subject = |item: &Option<AuditSubject>| {
        item.as_ref().map(|s| {
            [s.name.clone(), s.id.as_ref().map(|id| format!("ID: {id}"))]
                .into_iter()
                .flatten()
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
    };
    let mut entries: Vec<(String, Option<String>)> = vec![
        ("Server".into(), subject(&event.guild)),
        ("Member".into(), subject(&event.target)),
        ("Executor".into(), subject(&event.actor)),
        ("Channel".into(), subject(&event.channel)),
        ("Reason".into(), event.reason.clone()),
    ];
    for (name, value) in &event.details {
        let text = match value {
            Value::Null => continue,
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        entries.push((name.clone(), Some(text)));
    }
    let fields: Vec<Value> = entries
        .into_iter()
        .filter_map(|(name, value)| value.filter(|v| !v.is_empty()).map(|v| (name, v)))
        .take(25)
        .map(|(name, value)| {
            json!({"name": name.chars().take(256).collect::<String>(), "value": value.chars().take(1024).collect::<String>(), "inline": name != "Reason"})
        })
        .collect();
    json!({
        "color": color_value(&event.color),
        "title": event.title,
        "fields": fields,
        "timestamp": crate::db::now(),
        "footer": {"text": format!("Gaia Audit Log • {}", event.category)},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_are_sanitized_and_show_the_server() {
        let embed = render_event(AuditEvent {
            title: "Member Muted".into(),
            category: "Moderation".into(),
            guild: Some(AuditSubject::named("123456789012345678", "Other Server")),
            reason: Some("https://discord.com/api/webhooks/123456789012345678/secret\nline".into()),
            ..Default::default()
        });
        let text = embed.to_string();
        assert!(!text.contains("/secret"), "{text}");
        assert!(text.contains("Other Server"), "global logs say which server they came from");
        assert_eq!(embed["footer"]["text"], "Gaia Audit Log • Moderation");
    }

    #[test]
    fn bot_health_has_no_source_server() {
        let event = |category: &str| AuditEvent { category: category.into(), guild: Some(AuditSubject::named("5", "S")), ..Default::default() };
        assert_eq!(source(&event("Bot")), None);
        assert_eq!(source(&event("Moderation")), Some(GuildId::new(5)));
    }
}
