//! Best-effort centralized member join/leave delivery. Uses only webhook
//! credentials, so Gaia never needs access to the logging guild.
use super::audit_log_service::{error_name, reqwest_transport, retry_delay_ms, sanitize_audit_text, BoxFut, WebhookTransport, SNOWFLAKE, WEBHOOK};
use crate::logger;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use serenity::model::guild::Member;
use serenity::model::id::GuildId;
use serenity::model::user::User;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub type MemberLogConfig = crate::config::MemberLogging;

const MAX_QUEUE: usize = 100;
const RETRIES: u32 = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct ValidMemberLogConfig {
    pub webhook_url: String,
    pub channel_id: String,
}

/// `Err(reason)`: "not configured", "incomplete configuration", "invalid channel ID", "invalid webhook URL".
pub fn validate_member_log_config(config: &MemberLogConfig) -> Result<ValidMemberLogConfig, &'static str> {
    let trim = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    match (trim(&config.webhook_url), trim(&config.channel_id)) {
        (None, None) => Err("not configured"),
        (Some(webhook_url), Some(channel_id)) => {
            if !SNOWFLAKE.is_match(&channel_id) {
                Err("invalid channel ID")
            } else if !WEBHOOK.is_match(&webhook_url) {
                Err("invalid webhook URL")
            } else {
                Ok(ValidMemberLogConfig { webhook_url, channel_id })
            }
        }
        _ => Err("incomplete configuration"),
    }
}

/// What the embed needs from a member event (decoupled from serenity for tests).
#[derive(Clone, Debug, Default)]
pub struct MemberLogSubject {
    pub user_id: String,
    pub username: String,
    pub bot: bool,
    pub avatar_url: String,
    pub guild_id: String,
    pub guild_name: String,
}

impl MemberLogSubject {
    pub fn from_user(user: &User, guild_id: GuildId, guild_name: &str) -> Self {
        Self {
            user_id: user.id.to_string(),
            username: user.name.clone(),
            bot: user.bot,
            avatar_url: display_avatar_url(user),
            guild_id: guild_id.to_string(),
            guild_name: guild_name.to_string(),
        }
    }
}

/// discord.js `user.displayAvatarURL({ extension: "png", size: 4096, forceStatic: false })`.
pub fn display_avatar_url(user: &User) -> String {
    match &user.avatar {
        Some(hash) => {
            let ext = if hash.is_animated() { "gif" } else { "png" };
            format!("https://cdn.discordapp.com/avatars/{}/{}.{}?size=4096", user.id, hash, ext)
        }
        None => user.default_avatar_url(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Joined,
    Left,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Joined => "joined",
            Kind::Left => "left",
        }
    }
}

struct MemberLogEvent {
    kind: Kind,
    subject: MemberLogSubject,
    occurred_at: DateTime<Utc>,
}

struct State {
    config: Option<ValidMemberLogConfig>,
    queue: VecDeque<MemberLogEvent>,
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

#[derive(Clone)]
pub struct MemberLogService {
    inner: Arc<Inner>,
}

impl MemberLogService {
    #[cfg(test)]
    pub fn enabled(&self) -> bool {
        self.inner.state.lock().unwrap().enabled()
    }
    pub fn new(configured: MemberLogConfig, client: reqwest::Client) -> Self {
        Self::with_transport(configured, reqwest_transport(client))
    }

    pub fn with_transport(configured: MemberLogConfig, request: WebhookTransport) -> Self {
        let config = match validate_member_log_config(&configured) {
            Ok(config) => Some(config),
            Err(reason) => {
                if reason != "not configured" {
                    logger::warn("[member-logging] disabled", crate::ctx! {"reason" => reason});
                }
                None
            }
        };
        let state = State { config, queue: VecDeque::new(), stopping: false, destination_validated: false, validation_timer: None, validation_attempts: 0 };
        Self { inner: Arc::new(Inner { state: Mutex::new(state), request, busy: watch::channel(false).0 }) }
    }

    /// Validates the configured target through the webhook itself, never Discord guild membership.
    pub async fn initialize(&self) {
        self.inner.clone().initialize().await
    }

    /// `occurred_at` defaults to now.
    pub fn record_join(&self, member: &Member, guild_name: &str, occurred_at: Option<DateTime<Utc>>) {
        self.record_subject_join(MemberLogSubject::from_user(&member.user, member.guild_id, guild_name), occurred_at);
    }

    /// For `guild_member_removal` (user + guild id; member data may be unavailable).
    pub fn record_leave(&self, user: &User, guild_id: GuildId, guild_name: &str, occurred_at: Option<DateTime<Utc>>) {
        self.record_subject_leave(MemberLogSubject::from_user(user, guild_id, guild_name), occurred_at);
    }

    pub fn record_subject_join(&self, subject: MemberLogSubject, occurred_at: Option<DateTime<Utc>>) {
        self.record(Kind::Joined, subject, occurred_at.unwrap_or_else(Utc::now));
    }

    pub fn record_subject_leave(&self, subject: MemberLogSubject, occurred_at: Option<DateTime<Utc>>) {
        self.record(Kind::Left, subject, occurred_at.unwrap_or_else(Utc::now));
    }

    fn record(&self, kind: Kind, subject: MemberLogSubject, occurred_at: DateTime<Utc>) {
        {
            let mut state = self.inner.state.lock().unwrap();
            if state.config.is_none() || state.stopping || subject.bot {
                return;
            }
            if state.queue.len() >= MAX_QUEUE {
                logger::warn("[member-logging] queue full; dropped event", crate::ctx! {"event" => kind.as_str()});
                return;
            }
            state.queue.push_back(MemberLogEvent { kind, subject, occurred_at });
        }
        self.inner.start_worker();
    }

    /// Resolves once the delivery worker is idle.
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
        if *self.inner.busy.borrow() {
            let _ = tokio::time::timeout(timeout, self.idle()).await;
        }
        self.inner.state.lock().unwrap().queue.clear();
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
            let response = match (self.request)(configured.webhook_url.clone(), None).await {
                Ok(response) => response,
                Err(_) => return self.validation_failed(true),
            };
            if !response.ok() {
                if response.status == 429 || response.status >= 500 {
                    self.validation_failed(true);
                } else {
                    self.permanently_disable("webhook unavailable");
                }
                return;
            }
            let Ok(webhook) = serde_json::from_slice::<Value>(&response.body) else {
                return self.validation_failed(true);
            };
            if webhook["channel_id"].as_str() != Some(configured.channel_id.as_str()) {
                return self.permanently_disable("webhook channel mismatch");
            }
            {
                let mut state = self.state.lock().unwrap();
                state.destination_validated = true;
                state.validation_attempts = 0;
            }
            logger::info("[member-logging] destination verified", crate::ctx! {"channelId" => configured.channel_id});
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
        logger::warn("[member-logging] disabled", crate::ctx! {"reason" => reason});
    }

    fn validation_failed(self: &Arc<Self>, retry: bool) {
        let mut state = self.state.lock().unwrap();
        let will_retry = retry && !state.stopping && state.validation_attempts < RETRIES;
        logger::warn("[member-logging] webhook unavailable", crate::ctx! {"retrying" => will_retry});
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

    async fn send(&self, event: MemberLogEvent) {
        let Some(url) = self.state.lock().unwrap().config.as_ref().map(|c| c.webhook_url.clone()) else { return };
        let payload = json!({
            "embeds": [member_embed(&event)],
            "allowed_mentions": {"parse": [], "users": [], "roles": [], "replied_user": false},
        })
        .to_string();
        let kind = event.kind.as_str();
        for attempt in 0..=RETRIES {
            match (self.request)(url.clone(), Some(payload.clone())).await {
                Ok(response) => {
                    if response.ok() {
                        return;
                    }
                    if response.status != 429 && response.status < 500 {
                        logger::warn("[member-logging] delivery dropped", crate::ctx! {"status" => response.status, "event" => kind});
                        return;
                    }
                    if attempt == RETRIES {
                        logger::warn("[member-logging] delivery retries exhausted", crate::ctx! {"status" => response.status, "event" => kind});
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms(response.retry_after.as_deref(), attempt))).await;
                }
                Err(error) => {
                    if attempt == RETRIES {
                        logger::warn("[member-logging] delivery retries exhausted", crate::ctx! {"event" => kind, "error" => sanitize_audit_text(&error_name(&error), 80)});
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(150 * 2u64.pow(attempt))).await;
                }
            }
        }
    }
}

fn member_embed(event: &MemberLogEvent) -> Value {
    let s = &event.subject;
    let joined = event.kind == Kind::Joined;
    let timestamp = event.occurred_at.timestamp();
    json!({
        "color": if joined { 0x57f287 } else { 0xed4245 },
        "title": if joined { "Member Joined" } else { "Member Left" },
        "fields": [
            {"name": "User", "value": format!("<@{}> ({})", s.user_id, sanitize_audit_text(&s.username, 160)), "inline": false},
            {"name": "Guild", "value": sanitize_audit_text(&s.guild_name, 160), "inline": false},
            {"name": if joined { "Joined" } else { "Left" }, "value": format!("<t:{timestamp}:F>"), "inline": false},
            {"name": "User ID", "value": s.user_id, "inline": true},
            {"name": "Guild ID", "value": s.guild_id, "inline": true},
        ],
        "thumbnail": {"url": s.avatar_url},
        "timestamp": event.occurred_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "footer": {"text": "Gaia Member Log"},
    })
}

#[cfg(test)]
mod tests {
    use super::super::audit_log_service::tests::{ok, status, transport};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CHANNEL: &str = "1481921183464030358";
    fn logging() -> MemberLogConfig {
        MemberLogConfig { webhook_url: Some("https://discord.com/api/webhooks/123456789012345678/abcdefghijklmnopqrstuvwxyz".into()), channel_id: Some(CHANNEL.into()) }
    }
    fn member(bot: bool) -> MemberLogSubject {
        MemberLogSubject {
            user_id: "123456789012345678".into(),
            username: "dark".into(),
            bot,
            avatar_url: "https://cdn.discordapp.com/embed/avatars/0.png".into(),
            guild_id: "987654321098765432".into(),
            guild_name: "Example Server".into(),
        }
    }
    fn time() -> DateTime<Utc> {
        "2026-08-30T12:34:56.000Z".parse().unwrap()
    }
    async fn payload_for(join: bool) -> Value {
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = bodies.clone();
        let service = MemberLogService::with_transport(logging(), transport(move |_, body| match body {
            None => ok(json!({"channel_id": CHANNEL})),
            Some(body) => {
                sink.lock().unwrap().push(body);
                status(204)
            }
        }));
        service.initialize().await;
        if join { service.record_subject_join(member(false), Some(time())) } else { service.record_subject_leave(member(false), Some(time())) }
        service.idle().await;
        let body = bodies.lock().unwrap()[0].clone();
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn safely_disables_absent_invalid_and_mismatched_configuration() {
        assert_eq!(validate_member_log_config(&MemberLogConfig::default()), Err("not configured"));
        assert_eq!(validate_member_log_config(&MemberLogConfig { channel_id: Some(CHANNEL.into()), ..Default::default() }), Err("incomplete configuration"));
        assert_eq!(validate_member_log_config(&MemberLogConfig { webhook_url: Some("https://example.com/not-a-webhook".into()), ..logging() }), Err("invalid webhook URL"));
        let service = MemberLogService::with_transport(logging(), transport(|_, _| ok(json!({"channel_id": "999999999999999999"}))));
        service.initialize().await;
        assert!(!service.enabled());
    }

    #[tokio::test]
    async fn sends_green_no_ping_join_embed() {
        let payload = payload_for(true).await;
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "Member Joined");
        assert_eq!(embed["color"], 0x57f287);
        let ts = time().timestamp();
        assert_eq!(
            embed["fields"],
            json!([
                {"name": "User", "value": "<@123456789012345678> (dark)", "inline": false},
                {"name": "Guild", "value": "Example Server", "inline": false},
                {"name": "Joined", "value": format!("<t:{ts}:F>"), "inline": false},
                {"name": "User ID", "value": "123456789012345678", "inline": true},
                {"name": "Guild ID", "value": "987654321098765432", "inline": true},
            ])
        );
        assert!(embed["thumbnail"]["url"].as_str().unwrap().contains("embed/avatars/0.png"));
        assert_eq!(embed["timestamp"], "2026-08-30T12:34:56.000Z");
        assert_eq!(payload["allowed_mentions"], json!({"parse": [], "users": [], "roles": [], "replied_user": false}));
    }

    #[tokio::test]
    async fn sends_red_leave_embed() {
        let payload = payload_for(false).await;
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "Member Left");
        assert_eq!(embed["color"], 0xed4245);
        assert_eq!(embed["fields"][2], json!({"name": "Left", "value": format!("<t:{}:F>", time().timestamp()), "inline": false}));
    }

    #[tokio::test]
    async fn ignores_bot_joins_and_leaves() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let service = MemberLogService::with_transport(logging(), transport(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            ok(json!({"channel_id": CHANNEL}))
        }));
        service.initialize().await;
        service.record_subject_join(member(true), None);
        service.record_subject_leave(member(true), None);
        service.idle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn contains_transient_delivery_failures_and_retries() {
        let posts = Arc::new(AtomicUsize::new(0));
        let counter = posts.clone();
        let service = MemberLogService::with_transport(logging(), transport(move |_, body| {
            if body.is_none() {
                return ok(json!({"channel_id": CHANNEL}));
            }
            if counter.fetch_add(1, Ordering::SeqCst) == 0 { Err(anyhow::anyhow!("network unavailable")) } else { status(204) }
        }));
        service.initialize().await;
        service.record_subject_join(member(false), None);
        service.idle().await;
        assert_eq!(posts.load(Ordering::SeqCst), 2);
    }
}
