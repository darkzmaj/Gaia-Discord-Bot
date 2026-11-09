//! Where a server's logs go. Shared design with Gaia: every server chooses, once, between
//! **global** (logs from every server go to one log server, the "hub") and **per server**
//! (logs stay in the server they came from). A public bot is always per server.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serenity::all::{ChannelId, GuildId, UserId};

use crate::db::repositories::GaiaRepository;

/// Log kinds = their `guild_settings` column (the server's own channel, or the hub's default channel).
pub const ACTIVITY: &str = "log_channel_id";
pub const MEMBERS: &str = "member_log_channel_id";

pub const GLOBAL: &str = "global";
pub const PER_SERVER: &str = "server";

pub struct LogRouter {
    repo: Arc<GaiaRepository>,
    public: AtomicBool,
}

fn channel_id(value: Option<String>) -> Option<ChannelId> {
    value.and_then(|v| v.parse::<u64>().ok()).filter(|v| *v != 0).map(ChannelId::new)
}

impl LogRouter {
    pub fn new(repo: Arc<GaiaRepository>) -> Self {
        // Public until Discord says otherwise: global logging is never on by mistake.
        LogRouter { repo, public: AtomicBool::new(true) }
    }

    /// Set from the Discord application's "Public Bot" switch at startup.
    pub fn set_public(&self, public: bool) {
        self.public.store(public, Ordering::SeqCst);
    }

    pub fn is_public(&self) -> bool {
        self.public.load(Ordering::SeqCst)
    }

    /// The server's effective mode: `None` until its owner chooses. A public bot treats global as per server.
    pub fn mode(&self, guild_id: GuildId) -> Option<&'static str> {
        match self.repo.settings(&guild_id.to_string()).ok()?.log_mode.as_deref() {
            Some(GLOBAL) if !self.is_public() => Some(GLOBAL),
            Some(GLOBAL | PER_SERVER) => Some(PER_SERVER),
            _ => None,
        }
    }

    /// The log server of global mode (never for a public bot).
    pub fn hub(&self) -> Option<GuildId> {
        if self.is_public() {
            return None;
        }
        self.repo.log_hub().ok().flatten().and_then(|g| g.parse::<u64>().ok()).filter(|g| *g != 0).map(GuildId::new)
    }

    fn setting(&self, guild_id: GuildId, kind: &str) -> Option<ChannelId> {
        let s = self.repo.settings(&guild_id.to_string()).ok()?;
        channel_id(if kind == MEMBERS { s.member_log_channel_id } else { s.log_channel_id })
    }

    /// Where `kind` logs from `source` go; `None` source = bot health (the hub only).
    pub fn channel(&self, source: Option<GuildId>, kind: &str) -> Option<ChannelId> {
        let Some(source) = source else { return self.hub().and_then(|hub| self.setting(hub, kind)) };
        match self.mode(source)? {
            PER_SERVER => self.setting(source, kind),
            _ => {
                let hub = self.hub()?;
                channel_id(self.repo.log_route(&source.to_string(), kind).ok().flatten()).or_else(|| self.setting(hub, kind))
            }
        }
    }

    /// Only the server owner may choose the mode or move the hub.
    pub fn is_owner(cache: &serenity::cache::Cache, guild_id: GuildId, user: UserId) -> bool {
        cache.guild(guild_id).is_some_and(|g| g.owner_id == user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::types::Value;

    #[test]
    fn routes_per_server_global_and_public() {
        let repo = crate::test_support::repo();
        let router = LogRouter::new(repo.clone());
        router.set_public(false);
        let (a, b, hub) = (GuildId::new(1), GuildId::new(2), GuildId::new(3));
        let set = |g: &str, key: &str, v: &str| repo.update_setting(g, key, Value::Text(v.into())).unwrap();

        assert_eq!(router.channel(Some(a), ACTIVITY), None, "nothing is logged before the owner chooses");
        set("1", "log_mode", PER_SERVER);
        set("1", ACTIVITY, "11");
        assert_eq!(router.channel(Some(a), ACTIVITY), Some(ChannelId::new(11)));
        assert_eq!(router.channel(None, ACTIVITY), None, "no hub, no bot health");

        set("2", "log_mode", GLOBAL);
        set("3", "log_mode", GLOBAL);
        repo.set_log_hub(Some("3")).unwrap();
        set("3", ACTIVITY, "31");
        set("3", MEMBERS, "32");
        assert_eq!(router.channel(Some(b), ACTIVITY), Some(ChannelId::new(31)), "global servers use the hub's channels");
        assert_eq!(router.channel(Some(hub), MEMBERS), Some(ChannelId::new(32)));
        assert_eq!(router.channel(None, ACTIVITY), Some(ChannelId::new(31)), "bot health goes to the hub");
        repo.set_log_route("2", MEMBERS, Some("33")).unwrap();
        assert_eq!(router.channel(Some(b), MEMBERS), Some(ChannelId::new(33)), "a server-specific channel in the hub");
        assert_eq!(router.channel(Some(a), ACTIVITY), Some(ChannelId::new(11)), "per-server servers are untouched");

        router.set_public(true);
        assert_eq!(router.mode(b), Some(PER_SERVER), "a public bot is always per server");
        assert_eq!(router.channel(Some(b), ACTIVITY), None, "and never sends to the hub");
        assert_eq!(router.channel(None, ACTIVITY), None);
    }
}
