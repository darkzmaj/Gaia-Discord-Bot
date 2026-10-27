//! Best-effort remote audit delivery through a tokened Discord webhook.
use crate::logger;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub type AuditLogConfig = crate::config::AuditLogging;

const MAX_QUEUE: usize = 100;
const RETRIES: u32 = 3;
pub(crate) static SNOWFLAKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{17,20}$").unwrap());
pub(crate) static WEBHOOK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^https://(?:canary\.|ptb\.)?discord(?:app)?\.com/api/webhooks/[0-9]{17,20}/[A-Za-z0-9_\-.]+/?(?:\?.*)?$").unwrap()
});

/// A subject (guild, actor, target, channel) of an audit event.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct AuditSubject {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl AuditSubject {
    pub fn id(id: impl Into<String>) -> Self {
        Self { id: Some(id.into()), name: None }
    }
    pub fn named(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self { id: Some(id.into()), name: Some(name.into()) }
    }
}

/// Mirrors the TS `AuditEvent`. `color`: "success" | "warning" | "danger" | "info" (empty = info).
/// `priority`: "low" | "normal" | "critical" (empty = normal). `details` keep insertion order;
/// a `Value::Null` detail is the TS `undefined` and is omitted from the embed.
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

impl AuditEvent {
    fn is_low(&self) -> bool {
        self.priority == "low"
    }
}

/// Removes credentials and control characters before anything leaves Gaia. `limit` counts chars.
pub fn sanitize_audit_text(value: &str, limit: usize) -> String {
    static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\r\n]+").unwrap());
    NEWLINES.replace_all(&logger::redact(value), " ").chars().take(limit).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidAuditLogConfig {
    pub webhook_url: String,
    pub guild_id: String,
    pub channel_id: String,
}

fn trimmed(value: &Option<String>) -> Option<String> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(String::from)
}

/// `Err(reason)` means disabled: "not configured", "incomplete configuration",
/// "invalid guild or channel ID" or "invalid webhook URL".
pub fn validate_audit_log_config(config: &AuditLogConfig) -> Result<ValidAuditLogConfig, &'static str> {
    let (webhook_url, guild_id, channel_id) = (trimmed(&config.webhook_url), trimmed(&config.guild_id), trimmed(&config.channel_id));
    if webhook_url.is_none() && guild_id.is_none() && channel_id.is_none() {
        return Err("not configured");
    }
    let (Some(webhook_url), Some(guild_id), Some(channel_id)) = (webhook_url, guild_id, channel_id) else {
        return Err("incomplete configuration");
    };
    if !SNOWFLAKE.is_match(&guild_id) || !SNOWFLAKE.is_match(&channel_id) {
        return Err("invalid guild or channel ID");
    }
    if !WEBHOOK.is_match(&webhook_url) {
        return Err("invalid webhook URL");
    }
    Ok(ValidAuditLogConfig { webhook_url, guild_id, channel_id })
}

// ---- webhook transport (the TS `fetch` injection point) ----

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[derive(Clone, Debug, Default)]
pub struct WebhookResponse {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

impl WebhookResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// `(url, None)` = GET (validation), `(url, Some(json))` = POST JSON (delivery).
pub type WebhookTransport = Arc<dyn Fn(String, Option<String>) -> BoxFut<anyhow::Result<WebhookResponse>> + Send + Sync>;

pub fn reqwest_transport(client: reqwest::Client) -> WebhookTransport {
    Arc::new(move |url, body| {
        let client = client.clone();
        Box::pin(async move {
            let request = match body {
                None => client.get(&url),
                Some(body) => client.post(&url).header("content-type", "application/json").body(body),
            };
            let response = request.timeout(Duration::from_secs(5)).send().await?;
            let status = response.status().as_u16();
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(String::from);
            let body = response.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            Ok(WebhookResponse { status, retry_after, body })
        })
    })
}

/// TS `Number(header)`: absent/empty -> 0, non-numeric -> None (exponential backoff).
pub(crate) fn retry_delay_ms(retry_after: Option<&str>, attempt: u32) -> u64 {
    let parsed = match retry_after.map(str::trim) {
        None | Some("") => Some(0.0),
        Some(text) => text.parse::<f64>().ok().filter(|v| v.is_finite()),
    };
    match parsed {
        Some(seconds) => (seconds * 1000.0).clamp(0.0, 5000.0) as u64,
        None => 150 * 2u64.pow(attempt),
    }
}

pub(crate) fn error_name(error: &anyhow::Error) -> String {
    match error.downcast_ref::<reqwest::Error>() {
        Some(e) if e.is_timeout() => "TimeoutError".into(),
        _ => "Error".into(),
    }
}

// ---- service ----

struct State {
    config: Option<ValidAuditLogConfig>,
    queue: VecDeque<AuditEvent>,
    stopping: bool,
    destination_validated: bool,
    validation_timer: Option<JoinHandle<()>>,
    validation_attempts: u32,
}

impl State {
    fn enabled(&self) -> bool {
        self.config.is_some() && self.destination_validated && !self.stopping
    }
}

struct Inner {
    state: Mutex<State>,
    request: WebhookTransport,
    busy: watch::Sender<bool>,
}

/// `record` is deliberately synchronous and never fails: an unavailable audit
/// destination cannot affect Discord work. Cheap to clone (shared state).
#[derive(Clone)]
pub struct AuditLogService {
    inner: Arc<Inner>,
}

impl AuditLogService {
    #[cfg(test)]
    pub fn enabled(&self) -> bool {
        self.inner.state.lock().unwrap().enabled()
    }
    pub fn new(configured: AuditLogConfig, client: reqwest::Client) -> Self {
        Self::with_transport(configured, reqwest_transport(client))
    }

    pub fn with_transport(configured: AuditLogConfig, request: WebhookTransport) -> Self {
        let config = match validate_audit_log_config(&configured) {
            Ok(config) => Some(config),
            Err(reason) => {
                if reason != "not configured" {
                    logger::warn("[logging] remote audit logging disabled", crate::ctx! {"reason" => reason});
                }
                None
            }
        };
        let state = State { config, queue: VecDeque::new(), stopping: false, destination_validated: false, validation_timer: None, validation_attempts: 0 };
        Self { inner: Arc::new(Inner { state: Mutex::new(state), request, busy: watch::channel(false).0 }) }
    }

    /// Called after ready. A tokened webhook is self-contained transport/authentication.
    pub async fn initialize(&self) {
        self.inner.clone().initialize().await
    }

    pub fn record(&self, event: AuditEvent) {
        let safe = safe_event(event);
        {
            let mut state = self.inner.state.lock().unwrap();
            if state.config.is_none() || state.stopping {
                return;
            }
            if state.queue.len() >= MAX_QUEUE {
                if let Some(low) = state.queue.iter().position(AuditEvent::is_low) {
                    state.queue.remove(low);
                } else if safe.is_low() {
                    return;
                } else {
                    logger::warn("[logging] audit queue full; dropped event", crate::ctx! {"category" => safe.category});
                    return;
                }
            }
            state.queue.push_back(safe);
        }
        self.inner.start_worker();
    }

    /// Resolves once the delivery worker is idle (the TS tests' `await service.working`).
    pub async fn idle(&self) {
        let _ = self.inner.busy.subscribe().wait_for(|busy| !*busy).await;
    }

    pub async fn shutdown(&self, timeout: Duration) {
        {
            let mut state = self.inner.state.lock().unwrap();
            state.stopping = true;
            if let Some(timer) = state.validation_timer.take() {
                timer.abort();
            }
        }
        if !*self.inner.busy.borrow() {
            return;
        }
        let _ = tokio::time::timeout(timeout, self.idle()).await;
        let mut state = self.inner.state.lock().unwrap();
        if !state.queue.is_empty() {
            logger::warn("[logging] audit queue flush timed out", crate::ctx! {"dropped" => state.queue.len()});
        }
        state.queue.clear();
    }

    #[cfg(test)]
    fn queue_len(&self) -> usize {
        self.inner.state.lock().unwrap().queue.len()
    }
    #[cfg(test)]
    fn push_raw(&self, event: AuditEvent) {
        self.inner.state.lock().unwrap().queue.push_back(event);
    }
}

impl Inner {
    fn initialize(self: Arc<Self>) -> BoxFut<()> {
        Box::pin(async move {
            let configured = {
                let state = self.state.lock().unwrap();
                match &state.config {
                    Some(config) if !state.stopping && !state.destination_validated => config.clone(),
                    _ => return,
                }
            };
            logger::info("[logging] validating webhook destination", crate::ctx! {"configuredGuild" => configured.guild_id, "configuredChannel" => configured.channel_id});
            let response = match (self.request)(configured.webhook_url.clone(), None).await {
                Ok(response) => response,
                Err(_) => return self.validation_failed("temporary webhook API failure", true),
            };
            if !response.ok() {
                if response.status == 429 || response.status >= 500 {
                    self.validation_failed("temporary webhook API failure", true);
                } else {
                    self.permanently_disable("webhook unavailable");
                }
                return;
            }
            let webhook: Value = match serde_json::from_slice(&response.body) {
                Ok(value) => value,
                Err(_) => return self.validation_failed("temporary webhook API failure", true),
            };
            let (Some(guild_id), Some(channel_id)) = (webhook["guild_id"].as_str(), webhook["channel_id"].as_str()) else {
                return self.permanently_disable("webhook unavailable");
            };
            logger::info("[logging] webhook resolved", crate::ctx! {});
            if guild_id != configured.guild_id {
                return self.destination_mismatch("[logging] webhook guild mismatch", crate::ctx! {"configuredGuild" => configured.guild_id, "webhookGuild" => guild_id});
            }
            if channel_id != configured.channel_id {
                return self.destination_mismatch("[logging] webhook channel mismatch", crate::ctx! {"configuredChannel" => configured.channel_id, "webhookChannel" => channel_id});
            }
            logger::info("[logging] destination verified", crate::ctx! {"guildId" => configured.guild_id, "channelId" => configured.channel_id});
            {
                let mut state = self.state.lock().unwrap();
                state.destination_validated = true;
                state.validation_attempts = 0;
            }
            logger::info("[logging] remote audit logging enabled", crate::ctx! {"guildId" => configured.guild_id, "channelId" => configured.channel_id});
            self.start_worker();
        })
    }

    fn start_worker(self: &Arc<Self>) {
        {
            let state = self.state.lock().unwrap();
            if !state.enabled() || *self.busy.borrow() || state.queue.is_empty() {
                return;
            }
            self.busy.send_replace(true);
        }
        let inner = self.clone();
        tokio::spawn(async move {
            loop {
                let event = {
                    let mut state = inner.state.lock().unwrap();
                    match state.queue.pop_front() {
                        Some(event) => event,
                        None => {
                            inner.busy.send_replace(false);
                            return;
                        }
                    }
                };
                inner.send(event).await;
            }
        });
    }

    fn permanently_disable(&self, reason: &str) {
        let mut state = self.state.lock().unwrap();
        state.config = None;
        state.destination_validated = false;
        state.queue.clear();
        logger::warn("[logging] remote audit logging disabled", crate::ctx! {"reason" => sanitize_audit_text(reason, 160)});
    }

    fn destination_mismatch(&self, message: &str, context: logger::Ctx) {
        let mut state = self.state.lock().unwrap();
        state.config = None;
        state.destination_validated = false;
        state.queue.clear();
        logger::warn(message, context);
    }

    fn validation_failed(self: &Arc<Self>, reason: &str, retry: bool) {
        let mut state = self.state.lock().unwrap();
        let will_retry = retry && !state.stopping && state.validation_attempts < RETRIES;
        logger::warn("[logging] remote audit logging unavailable", crate::ctx! {"check" => reason, "retrying" => will_retry});
        if !will_retry {
            return;
        }
        state.validation_attempts += 1;
        let delay = Duration::from_millis(2_000 * state.validation_attempts as u64);
        let inner = self.clone();
        state.validation_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            inner.state.lock().unwrap().validation_timer = None;
            inner.initialize().await;
        }));
    }

    async fn send(&self, event: AuditEvent) {
        let Some(url) = self.state.lock().unwrap().config.as_ref().map(|c| c.webhook_url.clone()) else { return };
        let payload = json!({
            "embeds": [audit_embed(&event)],
            "allowed_mentions": {"parse": [], "users": [], "roles": [], "replied_user": false},
        })
        .to_string();
        for attempt in 0..=RETRIES {
            match (self.request)(url.clone(), Some(payload.clone())).await {
                Ok(response) => {
                    if response.ok() {
                        return;
                    }
                    if response.status != 429 && response.status < 500 {
                        logger::warn("[logging] audit delivery dropped", crate::ctx! {"status" => response.status, "category" => event.category});
                        return;
                    }
                    if attempt == RETRIES {
                        logger::warn("[logging] audit delivery retries exhausted", crate::ctx! {"status" => response.status, "category" => event.category});
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms(response.retry_after.as_deref(), attempt))).await;
                }
                Err(error) => {
                    if attempt == RETRIES {
                        logger::warn("[logging] audit delivery retries exhausted", crate::ctx! {"category" => event.category, "error" => sanitize_audit_text(&error_name(&error), 80)});
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(150 * 2u64.pow(attempt))).await;
                }
            }
        }
    }
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
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) fn transport<F>(f: F) -> WebhookTransport
    where
        F: Fn(String, Option<String>) -> anyhow::Result<WebhookResponse> + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        Arc::new(move |url, body| {
            let f = f.clone();
            Box::pin(async move { f(url, body) })
        })
    }
    pub(crate) fn ok(body: Value) -> anyhow::Result<WebhookResponse> {
        Ok(WebhookResponse { status: 200, retry_after: None, body: body.to_string().into_bytes() })
    }
    pub(crate) fn status(status: u16) -> anyhow::Result<WebhookResponse> {
        Ok(WebhookResponse { status, ..Default::default() })
    }

    const GUILD: &str = "123456789012345678";
    const CHANNEL: &str = "223456789012345678";
    fn logging() -> AuditLogConfig {
        AuditLogConfig {
            webhook_url: Some("https://discord.com/api/webhooks/123456789012345678/abcdefghijklmnopqrstuvwxyz".into()),
            guild_id: Some(GUILD.into()),
            channel_id: Some(CHANNEL.into()),
        }
    }
    fn valid_get() -> anyhow::Result<WebhookResponse> {
        ok(json!({"guild_id": GUILD, "channel_id": CHANNEL}))
    }
    fn event(title: &str, category: &str) -> AuditEvent {
        AuditEvent { title: title.into(), category: category.into(), ..Default::default() }
    }

    #[test]
    fn keeps_logging_disabled_for_absent_incomplete_and_invalid_configuration() {
        assert_eq!(validate_audit_log_config(&AuditLogConfig::default()), Err("not configured"));
        assert_eq!(validate_audit_log_config(&AuditLogConfig { guild_id: Some(GUILD.into()), ..Default::default() }), Err("incomplete configuration"));
        let bad = AuditLogConfig { webhook_url: Some("https://example.com/not-a-webhook".into()), ..logging() };
        assert_eq!(validate_audit_log_config(&bad), Err("invalid webhook URL"));
        assert!(!AuditLogService::with_transport(AuditLogConfig::default(), transport(|_, _| status(500))).enabled());
    }

    #[tokio::test]
    async fn rejects_mismatched_guild_channel_and_deleted_webhooks() {
        for response in [
            json!({"guild_id": "999999999999999999", "channel_id": CHANNEL}),
            json!({"guild_id": GUILD, "channel_id": "999999999999999999"}),
        ] {
            let service = AuditLogService::with_transport(logging(), transport(move |_, _| ok(response.clone())));
            service.initialize().await;
            assert!(!service.enabled());
        }
        let service = AuditLogService::with_transport(logging(), transport(|_, _| status(404)));
        service.initialize().await;
        assert!(!service.enabled());
    }

    #[tokio::test]
    async fn handles_transient_webhook_api_failure_safely() {
        let service = AuditLogService::with_transport(logging(), transport(|_, _| Err(anyhow::anyhow!("temporary network failure"))));
        service.initialize().await;
        assert!(!service.enabled());
        service.shutdown(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn validates_and_sends_through_cross_guild_webhook() {
        let sends = Arc::new(AtomicUsize::new(0));
        let counter = sends.clone();
        let service = AuditLogService::with_transport(logging(), transport(move |_, body| {
            if body.is_none() {
                return valid_get();
            }
            counter.fetch_add(1, Ordering::SeqCst);
            status(204)
        }));
        assert!(!service.enabled());
        service.initialize().await;
        service.record(event("Cross-guild Audit", "Bot"));
        service.idle().await;
        assert!(service.enabled());
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn delivers_compact_no_ping_embeds_and_sanitizes_secrets() {
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = bodies.clone();
        let service = AuditLogService::with_transport(logging(), transport(move |_, body| match body {
            None => valid_get(),
            Some(body) => {
                sink.lock().unwrap().push(body);
                status(204)
            }
        }));
        service.initialize().await;
        service.record(AuditEvent {
            guild: Some(AuditSubject::named("g", "@everyone")),
            reason: Some("https://discord.com/api/webhooks/123456789012345678/secret".into()),
            ..event("Member Muted", "Moderation")
        });
        service.idle().await;
        let body = bodies.lock().unwrap()[0].clone();
        let payload: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["allowed_mentions"], json!({"parse": [], "users": [], "roles": [], "replied_user": false}));
        assert!(!body.contains("/secret"));
        assert!(sanitize_audit_text("https://discord.com/api/webhooks/123456789012345678/secret", 900).contains("redacted"));
        assert_eq!(payload["embeds"][0]["footer"]["text"], "Gaia Audit Log • Moderation");
    }

    #[tokio::test]
    async fn retries_transient_failures_and_bounds_the_queue() {
        let posts = Arc::new(AtomicUsize::new(0));
        let counter = posts.clone();
        let service = AuditLogService::with_transport(logging(), transport(move |_, body| {
            if body.is_none() {
                return valid_get();
            }
            if counter.fetch_add(1, Ordering::SeqCst) + 1 < 3 { status(500) } else { status(204) }
        }));
        service.initialize().await;
        service.record(AuditEvent { priority: "critical".into(), ..event("Restore Failed", "Backups") });
        service.idle().await;
        assert_eq!(posts.load(Ordering::SeqCst), 3);
        // Fill the queue directly while the worker is idle (mirrors pushing onto the TS private queue).
        for _ in 0..100 {
            service.push_raw(AuditEvent { priority: "low".into(), ..event("noise", "Bot") });
        }
        service.record(AuditEvent { priority: "critical".into(), ..event("Mute Automatically Expired", "Moderation") });
        assert!(service.queue_len() <= 100);
    }

    #[tokio::test]
    async fn delivery_and_shutdown_failures_do_not_escape() {
        let service = AuditLogService::with_transport(logging(), transport(|_, body| {
            if body.is_none() { valid_get() } else { Err(anyhow::anyhow!("network unavailable")) }
        }));
        service.initialize().await;
        service.record(event("Verification Enabled", "Verification"));
        service.shutdown(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn does_not_retry_permanent_rejections() {
        let posts = Arc::new(AtomicUsize::new(0));
        let counter = posts.clone();
        let service = AuditLogService::with_transport(logging(), transport(move |_, body| {
            if body.is_none() {
                return valid_get();
            }
            counter.fetch_add(1, Ordering::SeqCst);
            status(400)
        }));
        service.initialize().await;
        service.record(event("Backup Created", "Backups"));
        service.idle().await;
        assert_eq!(posts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn accepts_important_event_categories() {
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        let service = AuditLogService::with_transport(logging(), transport(move |_, body| {
            if body.is_none() {
                return valid_get();
            }
            counter.fetch_add(1, Ordering::SeqCst);
            status(204)
        }));
        service.initialize().await;
        for (title, category) in [("Admin Configuration Changed", "Admin Configuration"), ("Embed Scheduled", "Embeds"), ("Manual Backup Created", "Backups"), ("Restore Completed With Warnings", "Backups"), ("Restore Failed", "Backups")] {
            service.record(AuditEvent { guild: Some(AuditSubject::named("g", "Guild")), ..event(title, category) });
        }
        service.idle().await;
        assert_eq!(sent.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn retry_after_matches_js_number_semantics() {
        assert_eq!(retry_delay_ms(None, 2), 0);
        assert_eq!(retry_delay_ms(Some("1.5"), 0), 1500);
        assert_eq!(retry_delay_ms(Some("60"), 0), 5000);
        assert_eq!(retry_delay_ms(Some("soon"), 2), 600);
    }
}
