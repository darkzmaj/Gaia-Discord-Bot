//! Behavioural test harness: a fake Discord REST server, a seeded serenity cache, an in-memory
//! repository, a capturing audit log and interaction/message builders.
//!
//! ```ignore
//! let h = Harness::start().await;
//! h.seed(GuildSeed::new(1, "Guild").role(20, "Gaia", 10, Permissions::MANAGE_ROLES).bot(&[20]).member(5, &[]));
//! let service = VerificationService::new(h.repo.clone(), h.discord.clone(), Some(h.audit.clone()));
//! service.button(&h.button(1, 5, "verify:click")).await.unwrap();
//! assert_eq!(h.calls_to("PUT", "/guilds/1/members/5/roles/*").len(), 1);
//! assert_eq!(h.replies()[0]["data"]["content"], "🟢 You have been verified.");
//! ```
//!
//! Routes are matched on the path after `/api/v10`; `*` matches one segment. Rules added with
//! [`Harness::on`] win over the defaults (latest rule first). Member GET/PUT-role/DELETE-role/kick
//! routes are stateful against the seeded members, like real REST; nothing updates the cache,
//! because Discord does that through gateway events, which do not exist here.
use crate::db::repositories::GaiaRepository;
use crate::discord::Discord;
use crate::services::audit_log_service::AuditLogService;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use serde_json::{json, Value};
use serenity::all::{
    Cache, ChannelType, CommandInteraction, ComponentInteraction, Context, GuildCreateEvent, GuildId, HttpBuilder,
    Interaction, Message, ModalInteraction, Permissions, ReadyEvent, ShardId, ShardMessenger, UserId,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const BOT_ID: u64 = 900;
pub const APP_ID: u64 = 901;
const TIMESTAMP: &str = "2026-01-01T00:00:00.000Z";
const AUDIT_GUILD: &str = "123456789012345678";
const AUDIT_CHANNEL: &str = "223456789012345678";

/// One request the bot sent. `body` is `Null` for empty or non-JSON (multipart) bodies.
#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub body: Value,
    /// Decoded `X-Audit-Log-Reason`.
    pub reason: Option<String>,
}

struct Rule {
    method: String,
    pattern: String,
    status: u16,
    body: Value,
}

#[derive(Default)]
struct Fake {
    calls: Vec<Call>,
    rules: Vec<Rule>,
    members: HashMap<(String, String), Value>,
    channel_guilds: HashMap<String, String>,
    dm_channels: Vec<String>,
    next_id: u64,
}

fn matches(pattern: &str, path: &str) -> bool {
    let (p, s): (Vec<_>, Vec<_>) = (pattern.trim_matches('/').split('/').collect(), path.trim_matches('/').split('/').collect());
    p.len() == s.len() && p.iter().zip(&s).all(|(p, s)| *p == "*" || p == s)
}

pub fn user_json(id: u64) -> Value {
    json!({"id": id.to_string(), "username": format!("user{id}"), "discriminator": "0", "global_name": null, "avatar": null, "bot": id == BOT_ID})
}

fn unknown(code: u64, message: &str) -> (u16, Value) {
    (404, json!({"code": code, "message": message}))
}

impl Fake {
    fn id(&mut self) -> String {
        self.next_id += 1;
        (10_000_000 + self.next_id).to_string()
    }

    fn message(&mut self, channel: &str, body: &Value) -> Value {
        let mut message = json!({
            "id": self.id(), "channel_id": channel, "author": user_json(BOT_ID), "content": "", "timestamp": TIMESTAMP,
            "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0,
        });
        // Components are not echoed: builder output is not always valid as a received component.
        for key in ["content", "embeds", "flags"] {
            if let Some(value) = body.get(key) {
                message[key] = value.clone();
            }
        }
        message
    }

    fn route(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        if let Some(rule) = self.rules.iter().rev().find(|r| r.method == method && matches(&r.pattern, path)) {
            return (rule.status, rule.body.clone());
        }
        let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
        let key = |g: &str, u: &str| (g.to_string(), u.to_string());
        match (method, seg.as_slice()) {
            ("GET", ["guilds", g, "members", u]) => match self.members.get(&key(g, u)) {
                Some(member) => (200, member.clone()),
                None => unknown(10007, "Unknown Member"),
            },
            ("PUT" | "DELETE", ["guilds", g, "members", u, "roles", r]) => {
                let Some(member) = self.members.get_mut(&key(g, u)) else { return unknown(10007, "Unknown Member") };
                let roles = member["roles"].as_array_mut().unwrap();
                roles.retain(|x| x != r);
                if method == "PUT" {
                    roles.push(json!(r));
                }
                (204, Value::Null)
            }
            ("DELETE", ["guilds", g, "members", u]) => match self.members.remove(&key(g, u)) {
                Some(_) => (204, Value::Null),
                None => unknown(10007, "Unknown Member"),
            },
            ("POST", ["users", "@me", "channels"]) => {
                let recipient = body["recipient_id"].as_str().and_then(|r| r.parse().ok()).unwrap_or(0);
                let id = self.id();
                self.dm_channels.push(id.clone());
                (200, json!({"id": id, "type": 1, "last_message_id": null, "recipients": [user_json(recipient)]}))
            }
            ("POST", ["interactions", ..]) => (204, Value::Null),
            ("POST" | "PATCH", ["channels", c, "messages", ..]) => (200, self.message(c, body)),
            ("POST" | "PATCH", ["webhooks", ..]) => (200, self.message("1", body)),
            ("POST" | "PATCH", ["guilds", g, "roles", ..]) => {
                let id = seg.get(3).map_or_else(|| self.id(), |r| r.to_string());
                let mut role = role_json(id.parse().unwrap(), "role", 1, Permissions::empty());
                role["guild_id"] = json!(g);
                merge(&mut role, body);
                (200, role)
            }
            ("POST", ["guilds", g, "channels"]) => {
                let id = self.id();
                self.channel_guilds.insert(id.clone(), g.to_string());
                let mut channel = json!({"id": id, "type": 0, "guild_id": g, "name": "channel", "position": 0, "permission_overwrites": [], "nsfw": false});
                merge(&mut channel, body);
                (200, channel)
            }
            ("PATCH", ["channels", c]) => {
                let guild = self.channel_guilds.get(*c).cloned().unwrap_or_else(|| "1".into());
                let mut channel = json!({"id": c, "type": 0, "guild_id": guild, "name": "channel", "position": 0, "permission_overwrites": [], "nsfw": false});
                merge(&mut channel, body);
                (200, channel)
            }
            ("DELETE" | "PUT", _) => (204, Value::Null),
            ("POST" | "PATCH", _) => {
                let mut echo = json!({"id": self.id()});
                merge(&mut echo, body);
                (200, echo)
            }
            _ => unknown(0, "Unknown route in fake Discord"),
        }
    }
}

fn merge(target: &mut Value, body: &Value) {
    if let (Some(target), Some(body)) = (target.as_object_mut(), body.as_object()) {
        for (k, v) in body {
            if !v.is_null() {
                target.insert(k.clone(), v.clone());
            }
        }
    }
}

async fn handle(State(fake): State<Arc<Mutex<Fake>>>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().trim_start_matches("/api/v10").to_string();
    let reason = req
        .headers()
        .get("x-audit-log-reason")
        .and_then(|v| v.to_str().ok())
        .map(|v| percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned());
    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap_or_default();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let (status, response) = {
        let mut fake = fake.lock().unwrap();
        let routed = fake.route(&method, &path, &body);
        fake.calls.push(Call { method, path, body, reason });
        routed
    };
    let builder = Response::builder().status(status);
    if status == 204 {
        return builder.body(Body::empty()).unwrap();
    }
    builder.header("content-type", "application/json").body(Body::from(response.to_string())).unwrap()
}

pub fn role_json(id: u64, name: &str, position: u16, permissions: Permissions) -> Value {
    json!({
        "id": id.to_string(), "name": name, "color": 0, "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "managed": false, "mentionable": false, "permissions": permissions.bits().to_string(), "position": position,
        "icon": null, "unicode_emoji": null, "flags": 0,
    })
}

/// A `GUILD_CREATE` payload. `json` is public so tests can tweak anything the helpers do not cover.
pub struct GuildSeed {
    pub json: Value,
}

impl GuildSeed {
    /// Owner defaults to user 2; the @everyone role (id = guild id) has View Channel + Send Messages.
    pub fn new(id: u64, name: &str) -> Self {
        let everyone = role_json(id, "@everyone", 0, Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES);
        Self {
            json: json!({
                "id": id.to_string(), "name": name, "icon": null, "icon_hash": null, "splash": null, "discovery_splash": null,
                "owner_id": "2", "afk_channel_id": null, "afk_timeout": 300, "verification_level": 0,
                "default_message_notifications": 0, "explicit_content_filter": 0, "roles": [everyone], "emojis": [],
                "features": [], "mfa_level": 0, "application_id": null, "system_channel_id": null, "system_channel_flags": 0,
                "rules_channel_id": null, "vanity_url_code": null, "description": null, "banner": null, "premium_tier": 0,
                "preferred_locale": "en-US", "public_updates_channel_id": null, "nsfw_level": 0, "stickers": [],
                "premium_progress_bar_enabled": false, "joined_at": TIMESTAMP, "large": false, "member_count": 0,
                "voice_states": [], "members": [], "channels": [], "threads": [], "presences": [], "stage_instances": [],
                "guild_scheduled_events": [], "safety_alerts_channel_id": null, "incidents_data": null,
            }),
        }
    }

    fn id(&self) -> &str {
        self.json["id"].as_str().unwrap()
    }

    fn push(mut self, key: &str, value: Value) -> Self {
        self.json[key].as_array_mut().unwrap().push(value);
        self
    }

    pub fn owner(mut self, user: u64) -> Self {
        self.json["owner_id"] = json!(user.to_string());
        self
    }

    pub fn role(self, id: u64, name: &str, position: u16, permissions: Permissions) -> Self {
        self.push("roles", role_json(id, name, position, permissions))
    }

    pub fn channel(self, id: u64, kind: ChannelType, parent: Option<u64>) -> Self {
        let guild = self.id().to_string();
        let channel = json!({
            "id": id.to_string(), "type": u8::from(kind), "guild_id": guild, "name": format!("channel{id}"), "position": 0,
            "permission_overwrites": [], "parent_id": parent.map(|p| p.to_string()), "nsfw": false,
        });
        self.push("channels", channel)
    }

    /// Adds a permission overwrite to an already added channel.
    pub fn overwrite(mut self, channel: u64, target: u64, is_role: bool, allow: Permissions, deny: Permissions) -> Self {
        let overwrite = json!({"id": target.to_string(), "type": if is_role { 0 } else { 1 }, "allow": allow.bits().to_string(), "deny": deny.bits().to_string()});
        let channels = self.json["channels"].as_array_mut().unwrap();
        let entry = channels.iter_mut().find(|c| c["id"].as_str() == Some(&*channel.to_string())).expect("seed the channel before its overwrite");
        entry["permission_overwrites"].as_array_mut().unwrap().push(overwrite);
        self
    }

    pub fn member(self, user: u64, roles: &[u64]) -> Self {
        let guild = self.id().to_string();
        let roles: Vec<String> = roles.iter().map(u64::to_string).collect();
        let member = json!({
            "user": user_json(user), "nick": null, "avatar": null, "roles": roles, "joined_at": TIMESTAMP, "premium_since": null,
            "deaf": false, "mute": false, "flags": 0, "pending": false, "communication_disabled_until": null, "guild_id": guild,
        });
        self.push("members", member)
    }

    /// Gaia's own member (user [`BOT_ID`]).
    pub fn bot(self, roles: &[u64]) -> Self {
        self.member(BOT_ID, roles)
    }

    pub fn voice_state(self, user: u64, channel: u64) -> Self {
        let guild = self.id().to_string();
        let state = json!({
            "guild_id": guild, "channel_id": channel.to_string(), "user_id": user.to_string(), "session_id": format!("session{user}"),
            "deaf": false, "mute": false, "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false,
            "request_to_speak_timestamp": null,
        });
        self.push("voice_states", state)
    }
}

pub struct Harness {
    fake: Arc<Mutex<Fake>>,
    pub discord: Discord,
    pub repo: Arc<GaiaRepository>,
    /// Audit log whose webhook deliveries are captured; read them with [`Harness::audits`].
    pub audit: Arc<AuditLogService>,
}

pub fn repo() -> Arc<GaiaRepository> {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::db::migrate(&mut conn).unwrap();
    Arc::new(GaiaRepository::new(conn))
}

impl Harness {
    pub async fn start() -> Self {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let app = axum::Router::new().fallback(handle).with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // `proxy` rewrites the https://discord.com prefix every serenity route starts with.
        let http = HttpBuilder::new("Bot test-token")
            .proxy(base)
            .ratelimiter_disabled(true)
            .application_id(APP_ID.into())
            .build();
        let cache = Arc::new(Cache::new());
        let ready = json!({
            "v": 10, "user": user_json(BOT_ID), "guilds": [], "session_id": "session", "resume_gateway_url": "wss://gateway.invalid",
            "shard": [0, 1], "application": {"id": APP_ID.to_string(), "flags": 0},
        });
        cache.update(&mut serde_json::from_value::<ReadyEvent>(json!(ready)).unwrap());

        // Audit events reach a fake log server: every seeded server is in global mode (see `seed`).
        let repo = repo();
        repo.set_log_hub(Some(AUDIT_GUILD)).unwrap();
        repo.update_setting(AUDIT_GUILD, "log_channel_id", rusqlite::types::Value::Text(AUDIT_CHANNEL.into())).unwrap();
        let http = Arc::new(http);
        let audit = Arc::new(AuditLogService::new(Arc::new(crate::services::log_routing::LogRouter::new(repo.clone())), http.clone()));
        audit.router().set_public(false);

        Self { fake, discord: Discord { http, cache }, repo, audit }
    }

    /// Applies the guild to the cache (as GUILD_CREATE) and registers its members with the fake REST API.
    pub fn seed(&self, guild: GuildSeed) {
        {
            let mut fake = self.fake.lock().unwrap();
            let id = guild.id().to_string();
            for member in guild.json["members"].as_array().unwrap() {
                fake.members.insert((id.clone(), member["user"]["id"].as_str().unwrap().to_string()), member.clone());
            }
            for channel in guild.json["channels"].as_array().unwrap() {
                fake.channel_guilds.insert(channel["id"].as_str().unwrap().to_string(), id.clone());
            }
        }
        let id = guild.id().to_string();
        let mut event: GuildCreateEvent = serde_json::from_value(guild.json).expect("invalid guild seed");
        self.discord.cache.update(&mut event);
        self.repo.update_setting(&id, "log_mode", rusqlite::types::Value::Text("global".into())).unwrap();
    }

    /// Overrides the response for `method` + `pattern` (`*` = one path segment), e.g.
    /// `h.on("POST", "/channels/*/messages", 403, json!({"code": 50007, "message": "Cannot send messages to this user"}))`.
    pub fn on(&self, method: &str, pattern: &str, status: u16, body: Value) {
        self.fake.lock().unwrap().rules.push(Rule { method: method.into(), pattern: pattern.into(), status, body });
    }

    /// Every call except audit deliveries to the fake log server (read those with [`Harness::audits`]).
    pub fn calls(&self) -> Vec<Call> {
        let audit = format!("/channels/{AUDIT_CHANNEL}/messages");
        self.fake.lock().unwrap().calls.iter().filter(|c| c.path != audit).cloned().collect()
    }

    pub fn calls_to(&self, method: &str, pattern: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|c| c.method == method && matches(pattern, &c.path)).collect()
    }

    /// `METHOD /path` for every call, for asserting order.
    pub fn trace(&self) -> Vec<String> {
        self.calls().iter().map(|c| format!("{} {}", c.method, c.path)).collect()
    }

    pub fn clear_calls(&self) {
        self.fake.lock().unwrap().calls.clear();
    }

    /// Bodies of interaction callbacks (`{"type": 4, "data": {...}}`), in order.
    pub fn replies(&self) -> Vec<Value> {
        self.calls_to("POST", "/interactions/*/*/callback").into_iter().map(|c| c.body).collect()
    }

    /// Content of every message the bot sent in a DM channel it opened.
    pub fn dms(&self) -> Vec<Value> {
        let fake = self.fake.lock().unwrap();
        let dm = |c: &&Call| c.path.split('/').nth(2).is_some_and(|id| fake.dm_channels.iter().any(|d| d == id));
        fake.calls.iter().filter(|c| c.method == "POST" && matches("/channels/*/messages", &c.path)).filter(dm).map(|c| c.body.clone()).collect()
    }

    /// Embeds delivered to the audit log so far (waits for the delivery worker).
    pub async fn audits(&self) -> Vec<Value> {
        self.audit.idle().await;
        let path = format!("/channels/{AUDIT_CHANNEL}/messages");
        self.fake.lock().unwrap().calls.iter().filter(|c| c.method == "POST" && c.path == path).map(|c| c.body["embeds"][0].clone()).collect()
    }

    pub fn member(&self, guild: u64, user: u64) -> serenity::all::Member {
        self.discord.cache.guild(GuildId::new(guild)).and_then(|g| g.members.get(&UserId::new(user)).cloned()).expect("member not seeded")
    }

    /// A serenity `Context` over the harness's Http/Cache (its shard messenger goes nowhere).
    pub fn ctx(&self) -> Context {
        Context {
            data: Arc::new(tokio::sync::RwLock::new(serenity::prelude::TypeMap::new())),
            shard: ShardMessenger::detached(),
            shard_id: ShardId(0),
            http: self.discord.http.clone(),
            cache: self.discord.cache.clone(),
        }
    }

    /// The full bot over the harness's Discord and repository, for driving `on_interaction(&h.ctx(), ..)`.
    /// Its own audit log is unconfigured; services built by hand can use [`Harness::audit`] instead.
    /// A bot whose Discord application is private (global logging allowed), as `on_ready` would find it.
    pub fn bot(&self) -> crate::bot::GaiaBot {
        let bot = crate::bot::GaiaBot::new(self.repo.clone(), Some(APP_ID.to_string()), self.discord.clone());
        bot.audit.router().set_public(false);
        bot
    }

    fn next_id(&self) -> String {
        self.fake.lock().unwrap().id()
    }

    /// A guild message by `author` (from the seeded member if present).
    pub fn message(&self, guild: u64, channel: u64, author: u64, content: &str) -> Message {
        let mut message = self.fake.lock().unwrap().message(&channel.to_string(), &json!({"content": content}));
        message["author"] = user_json(author);
        message["guild_id"] = json!(guild.to_string());
        serde_json::from_value(message).unwrap()
    }

    /// Raw interaction: `kind` 2 = command, 3 = component, 4 = autocomplete, 5 = modal. The invoking member
    /// is the seeded one (with cache-computed permissions) or a role-less stand-in.
    pub fn interaction(&self, kind: u8, guild: u64, user: u64, data: Value) -> Interaction {
        let mut member = self.fake.lock().unwrap().members.get(&(guild.to_string(), user.to_string())).cloned().unwrap_or_else(|| {
            json!({"user": user_json(user), "roles": [], "joined_at": TIMESTAMP, "deaf": false, "mute": false, "flags": 0})
        });
        let permissions = self.discord.cache.guild(GuildId::new(guild)).map_or(Permissions::empty(), |g| {
            g.members.get(&UserId::new(user)).map_or(Permissions::empty(), |m| g.member_permissions(m))
        });
        member["permissions"] = json!(permissions.bits().to_string());
        let id = self.next_id();
        let mut value = json!({
            "id": id, "application_id": APP_ID.to_string(), "type": kind, "data": data, "guild_id": guild.to_string(),
            "channel_id": "1", "member": member, "token": format!("token{id}"), "version": 1, "locale": "en-US",
            "guild_locale": "en-US", "app_permissions": Permissions::all().bits().to_string(), "entitlements": [],
            "authorizing_integration_owners": {}, "context": 0, "attachment_size_limit": 10485760,
        });
        if kind == 3 {
            value["message"] = self.fake.lock().unwrap().message("1", &Value::Null);
        }
        serde_json::from_value(value).expect("invalid interaction")
    }

    pub fn button(&self, guild: u64, user: u64, custom_id: &str) -> ComponentInteraction {
        let data = json!({"custom_id": custom_id, "component_type": 2});
        self.interaction(3, guild, user, data).into_message_component().unwrap()
    }

    /// `component_type`: 3 string, 5 user, 6 role, 7 mentionable, 8 channel select.
    pub fn select(&self, guild: u64, user: u64, custom_id: &str, component_type: u8, values: &[&str]) -> ComponentInteraction {
        let data = json!({"custom_id": custom_id, "component_type": component_type, "values": values});
        self.interaction(3, guild, user, data).into_message_component().unwrap()
    }

    /// Text inputs as `(custom_id, value)`, one per action row.
    pub fn modal(&self, guild: u64, user: u64, custom_id: &str, fields: &[(&str, &str)]) -> ModalInteraction {
        let rows: Vec<Value> =
            fields.iter().map(|(id, v)| json!({"type": 1, "components": [{"type": 4, "custom_id": id, "value": v}]})).collect();
        self.interaction(5, guild, user, json!({"custom_id": custom_id, "components": rows})).into_modal_submit().unwrap()
    }

    /// `options` is Discord's raw `data.options` array.
    pub fn command(&self, guild: u64, user: u64, name: &str, options: Value) -> CommandInteraction {
        let data = json!({"id": self.next_id(), "name": name, "type": 1, "options": options});
        self.interaction(2, guild, user, data).into_command().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builders_parse_and_routes_reach_the_fake() {
        let h = Harness::start().await;
        h.seed(
            GuildSeed::new(1, "Guild")
                .role(20, "Gaia", 10, Permissions::ADMINISTRATOR)
                .channel(100, ChannelType::Category, None)
                .channel(200, ChannelType::Voice, Some(100))
                .overwrite(200, 5, false, Permissions::SPEAK, Permissions::empty())
                .bot(&[20])
                .member(5, &[])
                .voice_state(5, 200),
        );
        let guild = h.discord.cache.guild(GuildId::new(1)).unwrap().clone();
        assert_eq!(guild.channels.len(), 2);
        assert!(guild.voice_states.contains_key(&UserId::new(5)));
        assert_eq!(h.discord.cache.current_user().id, UserId::new(BOT_ID));
        assert!(guild.member_permissions(&h.member(1, BOT_ID)).administrator());

        let _ = h.command(1, 5, "admin", json!([]));
        let _ = h.select(1, 5, "x", 3, &["a"]);
        let _ = h.modal(1, 5, "m", &[("title", "Hello")]);
        let button = h.button(1, 5, "x");
        assert_eq!(button.member.as_ref().unwrap().user.id, UserId::new(5));

        let message = h.message(1, 200, 5, "hi");
        message.delete(&h.discord.http).await.unwrap();
        UserId::new(5).direct_message(&h.discord.http, serenity::all::CreateMessage::new().content("dm")).await.unwrap();
        GuildId::new(1).kick_with_reason(&h.discord.http, UserId::new(5), "bye ü").await.unwrap();
        button.defer(&h.discord.http).await.unwrap();
        assert!(GuildId::new(1).member(&h.discord.http, UserId::new(5)).await.is_err(), "kicked member is gone");
        assert_eq!(h.calls_to("DELETE", "/guilds/1/members/5")[0].reason.as_deref(), Some("bye ü"));
        assert_eq!(h.dms()[0]["content"], "dm");
        assert_eq!(h.replies()[0]["type"], 6, "component defer = deferred update");
        assert_eq!(h.trace().len(), 6);
    }

    #[tokio::test]
    async fn drives_the_bot_interaction_router() {
        let h = Harness::start().await;
        h.seed(GuildSeed::new(1, "Guild").owner(99).member(5, &[]));
        let bot = h.bot();
        let click = Interaction::Component(h.button(1, 5, "verify:click"));
        bot.on_interaction(&h.ctx(), &click).await;
        assert_eq!(h.replies()[0]["data"]["content"], "🔴 Verification is currently disabled on this server.");
    }
}
