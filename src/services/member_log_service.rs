//! Member join/leave log: delivered, like the audit log, to the channel chosen in `/admin` → **Logs**
//! (this server's, or the log server's in global mode).
use super::audit_log_service::{sanitize_audit_text, AuditLogService};
use super::log_routing::MEMBERS;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use serenity::model::guild::Member;
use serenity::model::id::GuildId;
use serenity::model::user::User;
use std::sync::Arc;

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

struct MemberLogEvent {
    kind: Kind,
    subject: MemberLogSubject,
    occurred_at: DateTime<Utc>,
}

/// The message for one event; `None` for bots, which are never logged.
fn payload(event: &MemberLogEvent) -> Option<Value> {
    (!event.subject.bot).then(|| json!({"embeds": [member_embed(event)], "allowed_mentions": {"parse": []}}))
}

#[derive(Clone)]
pub struct MemberLogService {
    audit: Arc<AuditLogService>,
}

impl MemberLogService {
    pub fn new(audit: Arc<AuditLogService>) -> Self {
        Self { audit }
    }

    /// `occurred_at` defaults to now.
    pub fn record_join(&self, member: &Member, guild_name: &str, occurred_at: Option<DateTime<Utc>>) {
        self.record(Kind::Joined, MemberLogSubject::from_user(&member.user, member.guild_id, guild_name), occurred_at);
    }

    /// For `guild_member_removal` (user + guild id; member data may be unavailable).
    pub fn record_leave(&self, user: &User, guild_id: GuildId, guild_name: &str, occurred_at: Option<DateTime<Utc>>) {
        self.record(Kind::Left, MemberLogSubject::from_user(user, guild_id, guild_name), occurred_at);
    }

    fn record(&self, kind: Kind, subject: MemberLogSubject, occurred_at: Option<DateTime<Utc>>) {
        let source = subject.guild_id.parse::<u64>().ok().filter(|g| *g != 0).map(GuildId::new);
        let event = MemberLogEvent { kind, subject, occurred_at: occurred_at.unwrap_or_else(Utc::now) };
        if let (Some(source), Some(body)) = (source, payload(&event)) {
            self.audit.deliver(Some(source), MEMBERS, body);
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
    use super::*;

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
    fn payload_for(join: bool) -> Value {
        payload(&MemberLogEvent { kind: if join { Kind::Joined } else { Kind::Left }, subject: member(false), occurred_at: time() }).unwrap()
    }

    #[test]
    fn sends_green_no_ping_join_embed() {
        let payload = payload_for(true);
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
        assert_eq!(payload["allowed_mentions"], json!({"parse": []}));
    }

    #[test]
    fn sends_red_leave_embed() {
        let payload = payload_for(false);
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "Member Left");
        assert_eq!(embed["color"], 0xed4245);
        assert_eq!(embed["fields"][2], json!({"name": "Left", "value": format!("<t:{}:F>", time().timestamp()), "inline": false}));
    }

    #[test]
    fn ignores_bot_joins_and_leaves() {
        assert!(payload(&MemberLogEvent { kind: Kind::Joined, subject: member(true), occurred_at: time() }).is_none());
    }
}
