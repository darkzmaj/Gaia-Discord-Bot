use crate::ctx;
use crate::db::repositories::{ActiveMute, GaiaRepository};
use crate::discord::Discord;
use crate::logger;
use crate::services::audit_log_service::{AuditEvent, AuditLogService, AuditSubject};
use crate::services::duration::{human_duration, MAX_MUTE_MS};
use crate::services::locks::KeyedLock;
use chrono::{DateTime, Utc};
use serenity::builder::{CreateEmbed, CreateMessage};
use serenity::futures::future::join_all;
use serenity::model::prelude::*;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Default)]
pub struct MuteResult {
    pub error: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub dm_delivered: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct UnmuteResult {
    pub changed: bool,
    pub dm_delivered: Option<bool>,
    pub error: Option<String>,
}

fn muted_permissions() -> Permissions {
    Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::CREATE_PRIVATE_THREADS
        | Permissions::ADD_REACTIONS
        | Permissions::CONNECT
        | Permissions::SPEAK
        | Permissions::USE_VAD
        | Permissions::USE_APPLICATION_COMMANDS
        | Permissions::USE_EMBEDDED_ACTIVITIES
        | Permissions::USE_SOUNDBOARD
        | Permissions::USE_EXTERNAL_SOUNDS
}

pub(crate) fn subject(id: impl ToString, name: Option<String>) -> Option<AuditSubject> {
    Some(AuditSubject { id: Some(id.to_string()), name })
}

/// A member's highest role, falling back to @everyone (discord.js `roles.highest`).
pub(crate) fn highest_role(guild: &Guild, member: &Member) -> Option<Role> {
    guild
        .member_highest_role(member)
        .or_else(|| guild.roles.get(&guild.id.everyone_role()))
        .cloned()
}

/// discord.js `permissionOverwrites.edit` semantics: merges `bits` into the existing overwrite for
/// `target` (true = allow, false = deny, None = neutral) and writes the result.
pub(crate) async fn edit_overwrite(
    discord: &Discord,
    channel: &GuildChannel,
    target: u64,
    is_role: bool,
    bits: Permissions,
    value: Option<bool>,
    reason: Option<&str>,
) -> serenity::Result<()> {
    let existing = channel.permission_overwrites.iter().find(|o| overwrite_target(o).0 == target);
    let (mut allow, mut deny) = existing.map_or((Permissions::empty(), Permissions::empty()), |o| (o.allow, o.deny));
    let is_role = existing.map_or(is_role, |o| overwrite_target(o).1);
    match value {
        Some(true) => {
            allow |= bits;
            deny -= bits;
        }
        Some(false) => {
            allow -= bits;
            deny |= bits;
        }
        None => {
            allow -= bits;
            deny -= bits;
        }
    }
    put_overwrite(discord, channel.id, target, is_role, allow, deny, reason).await
}

pub(crate) async fn put_overwrite(
    discord: &Discord,
    channel: ChannelId,
    target: u64,
    is_role: bool,
    allow: Permissions,
    deny: Permissions,
    reason: Option<&str>,
) -> serenity::Result<()> {
    let body = serde_json::json!({ "allow": allow.bits().to_string(), "deny": deny.bits().to_string(), "type": if is_role { 0 } else { 1 } });
    discord.http.create_permission(channel, TargetId::new(target), &body, reason).await
}

/// (target id, is role)
pub(crate) fn overwrite_target(overwrite: &PermissionOverwrite) -> (u64, bool) {
    match overwrite.kind {
        PermissionOverwriteType::Role(id) => (id.get(), true),
        PermissionOverwriteType::Member(id) => (id.get(), false),
        _ => (0, false),
    }
}

pub fn muted_dm_embed(guild_name: &str, duration_ms: i64, expires: DateTime<Utc>, reason: Option<&str>) -> CreateEmbed {
    let at = expires.timestamp();
    CreateEmbed::new()
        .color(0xed4245)
        .title("🔇 You have been muted")
        .field("Server", guild_name, false)
        .field("Duration", human_duration(duration_ms), true)
        .field("Unmute", format!("<t:{at}:F>\n<t:{at}:R>"), true)
        .field("Reason", reason.unwrap_or("No reason provided."), false)
}

pub fn unmuted_dm_embed(guild_name: &str, reason: &str, automatic: bool) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .color(0x57f287)
        .title(if automatic { "🔊 Your mute has expired" } else { "🔊 You have been unmuted" });
    if automatic {
        embed = embed.description("You can communicate normally again.");
    }
    embed = embed.field("Server", guild_name, false);
    if !automatic {
        embed = embed.field("Reason", reason, false);
    }
    embed
}

pub struct MuteService {
    repo: Arc<GaiaRepository>,
    discord: Discord,
    audit: Option<Arc<AuditLogService>>,
    locks: KeyedLock,
    timer: Mutex<Option<JoinHandle<()>>>,
}

impl MuteService {
    /// TS `guilds: () => Iterable<Guild>` is the Discord cache here.
    pub fn new(repo: Arc<GaiaRepository>, discord: Discord, audit: Option<Arc<AuditLogService>>) -> Self {
        Self { repo, discord, audit, locks: KeyedLock::default(), timer: Mutex::new(None) }
    }

    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        let handle = tokio::spawn(async move {
            let mut first = true;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(e) = this.expire_due().await {
                    let message = if first { "initial mute expiry sweep failed" } else { "mute expiry sweep failed" };
                    logger::error(message, ctx! {"error" => e.to_string()});
                }
                first = false;
            }
        });
        if let Some(old) = self.timer.lock().unwrap().replace(handle) {
            old.abort();
        }
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.recover_active().await {
                logger::error("mute recovery failed", ctx! {"error" => e.to_string()});
            }
        });
    }

    pub fn stop(&self) {
        if let Some(timer) = self.timer.lock().unwrap().take() {
            timer.abort();
        }
    }

    fn guild_name(&self, guild: GuildId) -> String {
        self.discord.cache.guild(guild).map(|g| g.name.clone()).unwrap_or_default()
    }

    fn record(&self, event: AuditEvent) {
        if let Some(audit) = &self.audit {
            audit.record(event);
        }
    }

    pub async fn mute(
        &self,
        guild: GuildId,
        member: &Member,
        moderator: &Member,
        duration_ms: i64,
        reason: Option<&str>,
    ) -> anyhow::Result<MuteResult> {
        let key = format!("{guild}:{}", member.user.id);
        self.locks.run(&key, self.mute_locked(guild, member, moderator, duration_ms, reason)).await
    }

    async fn mute_locked(
        &self,
        guild: GuildId,
        member: &Member,
        moderator: &Member,
        duration_ms: i64,
        reason: Option<&str>,
    ) -> anyhow::Result<MuteResult> {
        let error = |m: &str| Ok(MuteResult { error: Some(m.into()), ..Default::default() });
        if duration_ms <= 0 || duration_ms > MAX_MUTE_MS {
            return error("Maximum mute duration is 30 days.");
        }
        let me_id = self.discord.cache.current_user().id;
        let (check, guild_name) = {
            let Some(g) = self.discord.cache.guild(guild) else { return error("Gaia cannot mute that member because of role hierarchy.") };
            let check = if member.user.id == moderator.user.id || member.user.id == me_id || member.user.id == g.owner_id {
                Some("You cannot mute that member.")
            } else if g.member_permissions(member).administrator() {
                Some("🔴 Cannot mute this member because Discord Administrator bypasses channel permission overwrites.")
            } else {
                match g.members.get(&me_id) {
                    Some(me) if highest_role(&g, member) < highest_role(&g, me) => None,
                    _ => Some("Gaia cannot mute that member because of role hierarchy."),
                }
            };
            (check, g.name.clone())
        };
        if let Some(message) = check {
            return error(message);
        }
        if let Some(message) = self.moderator_check(guild, member.user.id, moderator, "mute")? {
            return error(&message);
        }
        let guild_id = guild.to_string();
        if self.repo.settings(&guild_id)?.moderation_enabled == 0 {
            return error("Moderation is disabled on this server.");
        }
        let start = Utc::now();
        let expires_at = start + chrono::Duration::milliseconds(duration_ms);
        let iso = |d: DateTime<Utc>| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let added = self.repo.add_mute(&ActiveMute {
            guild_id: guild_id.clone(),
            user_id: member.user.id.to_string(),
            moderator_id: moderator.user.id.to_string(),
            reason: reason.map(str::to_string),
            started_at: iso(start),
            expires_at: iso(expires_at),
        })?;
        if !added {
            return error("That member is already muted.");
        }
        let target = subject(member.user.id, Some(member.user.tag()));
        let actor = subject(moderator.user.id, Some(moderator.user.tag()));
        if self.apply_guild(guild, member.user.id).await.is_err() {
            let _ = self.restore(guild, member.user.id).await;
            self.repo.remove_mute(&guild_id, &member.user.id.to_string())?;
            self.record(AuditEvent {
                title: "Mute Failed".into(),
                category: "Moderation".into(),
                color: "danger".into(),
                priority: "critical".into(),
                guild: subject(guild, Some(guild_name)),
                target,
                actor,
                reason: Some("Gaia could not apply mute channel restrictions.".into()),
                ..Default::default()
            });
            return error("Gaia could not apply mute channel restrictions. Check Manage Channels permission.");
        }
        let dm = muted_dm_embed(&guild_name, duration_ms, expires_at, reason);
        let dm_delivered = self.dm(member.user.id, dm).await;
        if !dm_delivered {
            logger::warn("mute DM failed", ctx! {"guildId" => guild_id, "userId" => member.user.id.to_string()});
        }
        logger::info(
            "member muted",
            ctx! {"guildId" => guild_id, "userId" => member.user.id.to_string(), "moderatorId" => moderator.user.id.to_string()},
        );
        self.record(AuditEvent {
            title: "Member Muted".into(),
            category: "Moderation".into(),
            color: "danger".into(),
            priority: "critical".into(),
            guild: subject(guild, Some(guild_name)),
            target,
            actor,
            reason: reason.map(str::to_string),
            details: vec![("Duration".into(), human_duration(duration_ms).into())],
            ..Default::default()
        });
        Ok(MuteResult { error: None, expires_at: Some(expires_at), dm_delivered: Some(dm_delivered) })
    }

    /// Unless the moderator owns the guild or is a Discord Administrator, the target must sit strictly
    /// below the moderator's highest role and must not be Gaia staff.
    fn moderator_check(&self, guild: GuildId, target: UserId, moderator: &Member, verb: &str) -> anyhow::Result<Option<String>> {
        let below = {
            let Some(g) = self.discord.cache.guild(guild) else { return Ok(None) };
            if moderator.user.id == g.owner_id || g.member_permissions(moderator).administrator() {
                return Ok(None);
            }
            // A target who already left has no roles to compare; the staff check below still applies.
            g.members.get(&target).is_none_or(|t| highest_role(&g, t) < highest_role(&g, moderator))
        };
        let staff = self.repo.is_staff(&guild.to_string(), &target.to_string())?;
        Ok(if !below {
            Some(format!("🔴 You cannot {verb} a member whose highest role is equal to or above yours."))
        } else if staff {
            Some(format!("🔴 You cannot {verb} Gaia staff."))
        } else {
            None
        })
    }

    async fn dm(&self, user: UserId, embed: CreateEmbed) -> bool {
        user.direct_message(&self.discord.http, CreateMessage::new().embed(embed)).await.is_ok()
    }

    /// Manual unmute (TS `automaticOrModerator` = the moderator). Expiry uses `unmute_with(.., true, None)`.
    pub async fn unmute(&self, guild: GuildId, user_id: &str, reason: &str, moderator: Option<&Member>) -> anyhow::Result<UnmuteResult> {
        self.unmute_with(guild, user_id.parse()?, reason, false, moderator).await
    }

    async fn unmute_with(
        &self,
        guild: GuildId,
        user_id: UserId,
        reason: &str,
        automatic: bool,
        moderator: Option<&Member>,
    ) -> anyhow::Result<UnmuteResult> {
        let key = format!("{guild}:{user_id}");
        self.locks.run(&key, self.unmute_locked(guild, user_id, reason, automatic, moderator)).await
    }

    async fn unmute_locked(
        &self,
        guild: GuildId,
        user_id: UserId,
        reason: &str,
        automatic: bool,
        moderator: Option<&Member>,
    ) -> anyhow::Result<UnmuteResult> {
        let (gid, uid) = (guild.to_string(), user_id.to_string());
        if self.repo.mute(&gid, &uid)?.is_none() {
            return Ok(UnmuteResult::default());
        }
        if let Some(message) = match moderator {
            Some(m) if !automatic => self.moderator_check(guild, user_id, m, "unmute")?,
            _ => None,
        } {
            return Ok(UnmuteResult { error: Some(message), ..Default::default() });
        }
        let guild_name = self.guild_name(guild);
        if self.restore(guild, user_id).await.is_err() {
            self.record(AuditEvent {
                title: "Unmute Failed".into(),
                category: "Moderation".into(),
                color: "danger".into(),
                priority: "critical".into(),
                guild: subject(guild, Some(guild_name)),
                target: subject(user_id, None),
                reason: Some("Gaia could not restore channel permissions.".into()),
                ..Default::default()
            });
            return Ok(UnmuteResult { changed: false, error: Some("Gaia could not restore this member’s channel permissions.".into()), ..Default::default() });
        }
        self.repo.remove_mute(&gid, &uid)?;
        let member = guild.member(&self.discord.http, user_id).await.ok();
        let dm_delivered = match &member {
            Some(m) => self.dm(m.user.id, unmuted_dm_embed(&guild_name, reason, automatic)).await,
            None => false,
        };
        logger::info("member unmuted", ctx! {"guildId" => gid, "userId" => uid, "automatic" => automatic});
        self.record(AuditEvent {
            title: if automatic { "Mute Automatically Expired" } else { "Member Unmuted" }.into(),
            category: "Moderation".into(),
            color: if automatic { "info" } else { "success" }.into(),
            priority: if automatic { "normal" } else { "critical" }.into(),
            guild: subject(guild, Some(guild_name)),
            target: subject(user_id, member.as_ref().map(|m| m.user.tag())),
            actor: if automatic { None } else { moderator.and_then(|m| subject(m.user.id, Some(m.user.tag()))) },
            reason: if automatic { None } else { Some(reason.to_string()) },
            ..Default::default()
        });
        Ok(UnmuteResult { changed: true, dm_delivered: Some(dm_delivered), error: None })
    }

    pub async fn apply_mute_on_join(&self, member: &Member) -> anyhow::Result<()> {
        if self.repo.mute(&member.guild_id.to_string(), &member.user.id.to_string())?.is_some() {
            let key = format!("{}:{}", member.guild_id, member.user.id);
            self.locks.run(&key, self.apply_guild(member.guild_id, member.user.id)).await?;
        }
        Ok(())
    }

    pub async fn configure_ticket_category(&self, guild: GuildId, category_id: Option<String>) -> anyhow::Result<()> {
        let gid = guild.to_string();
        self.repo.update_setting(&gid, "ticket_category_id", category_id.into())?;
        for mute in self.repo.active_mutes(&gid)? {
            let user: UserId = mute.user_id.parse()?;
            self.locks
                .run(&format!("{gid}:{}", mute.user_id), async {
                    self.restore(guild, user).await?;
                    self.apply_guild(guild, user).await
                })
                .await?;
        }
        Ok(())
    }

    pub async fn sync_new_channel(&self, channel: &GuildChannel) -> anyhow::Result<()> {
        if channel.kind == ChannelType::Category || channel.thread_metadata.is_some() {
            return Ok(());
        }
        let gid = channel.guild_id.to_string();
        let settings = self.repo.settings(&gid)?;
        if channel.parent_id.map(|p| p.to_string()) == settings.ticket_category_id {
            return Ok(());
        }
        for mute in self.repo.active_mutes(&gid)? {
            let user: UserId = mute.user_id.parse()?;
            self.locks.run(&format!("{gid}:{}", mute.user_id), self.apply_channel(channel, user)).await?;
        }
        Ok(())
    }

    async fn expire_due(&self) -> anyhow::Result<()> {
        for mute in self.repo.due_mutes(None)? {
            let guild: GuildId = mute.guild_id.parse()?;
            if self.discord.cache.guild(guild).is_some() {
                self.unmute_with(guild, mute.user_id.parse()?, "", true, None).await?;
            } else {
                self.repo.clear_mute_restores(&mute.guild_id, &mute.user_id)?;
                self.repo.remove_mute(&mute.guild_id, &mute.user_id)?;
            }
        }
        Ok(())
    }

    async fn recover_active(&self) -> anyhow::Result<()> {
        let now = Utc::now();
        for guild in self.discord.cache.guilds() {
            for mute in self.repo.active_mutes(&guild.to_string())? {
                let user: UserId = mute.user_id.parse()?;
                let expired = DateTime::parse_from_rfc3339(&mute.expires_at).map(|d| d <= now).unwrap_or(false);
                if expired {
                    self.unmute_with(guild, user, "", true, None).await?;
                } else if self.locks.run(&format!("{guild}:{user}"), self.apply_guild(guild, user)).await.is_err() {
                    logger::warn("mute recovery failed", ctx! {"guildId" => guild.to_string(), "userId" => mute.user_id});
                }
            }
        }
        Ok(())
    }

    async fn apply_guild(&self, guild: GuildId, user: UserId) -> anyhow::Result<()> {
        let ticket = self.repo.settings(&guild.to_string())?.ticket_category_id;
        let list: Vec<GuildChannel> = match self.discord.cache.guild(guild) {
            Some(g) => g
                .channels
                .values()
                .filter(|c| c.kind != ChannelType::Category && c.parent_id.map(|p| p.to_string()) != ticket)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        for chunk in list.chunks(10) {
            for result in join_all(chunk.iter().map(|c| self.apply_channel(c, user))).await {
                result?;
            }
        }
        Ok(())
    }

    async fn apply_channel(&self, channel: &GuildChannel, user: UserId) -> anyhow::Result<()> {
        let existing = channel.permission_overwrites.iter().find(|o| o.kind == PermissionOverwriteType::Member(user));
        self.repo.save_mute_restore(
            &channel.guild_id.to_string(),
            &user.to_string(),
            &channel.id.to_string(),
            existing.is_some(),
            &existing.map_or(0, |o| o.allow.bits()).to_string(),
            &existing.map_or(0, |o| o.deny.bits()).to_string(),
        )?;
        edit_overwrite(&self.discord, channel, user.get(), false, muted_permissions(), Some(false), Some("Gaia member mute")).await?;
        Ok(())
    }

    async fn restore(&self, guild: GuildId, user: UserId) -> anyhow::Result<()> {
        let (gid, uid) = (guild.to_string(), user.to_string());
        for saved in self.repo.mute_restores(&gid, &uid)? {
            let Ok(channel) = saved.channel_id.parse::<ChannelId>() else { continue };
            let exists = self.discord.cache.guild(guild).is_some_and(|g| g.channels.contains_key(&channel));
            if !exists {
                continue;
            }
            let reason = Some("Gaia mute restoration");
            if saved.existed != 0 {
                let allow = Permissions::from_bits_truncate(saved.allow_bits.parse().unwrap_or(0));
                let deny = Permissions::from_bits_truncate(saved.deny_bits.parse().unwrap_or(0));
                put_overwrite(&self.discord, channel, user.get(), false, allow, deny, reason).await?;
            } else {
                let _ = self.discord.http.delete_permission(channel, TargetId::new(user.get()), reason).await;
            }
        }
        self.repo.clear_mute_restores(&gid, &uid)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_and_unmute_dms_have_no_moderator_attribution() {
        let expires = DateTime::parse_from_rfc3339("2026-01-01T00:01:00Z").unwrap().with_timezone(&Utc);
        let sent = serde_json::to_string(&[
            muted_dm_embed("Guild", 60_000, expires, Some("Reason")),
            unmuted_dm_embed("Guild", "Reason", false),
        ])
        .unwrap();
        assert!(!sent.contains("Moderator"));
        assert!(!sent.contains("moderator"));
        assert!(sent.contains("<t:1767225660:F>"));
    }

    use crate::test_support::{GuildSeed, Harness};
    use serde_json::json;

    const G: GuildId = GuildId::new(1);
    const TARGET: u64 = 4;
    const MODERATOR: u64 = 3;
    const PEER: u64 = 5;
    const STAFF: u64 = 6;
    const OWNER: u64 = 2;

    /// Category 100 holds text 200 (target already has an Attach Files overwrite) and voice 201;
    /// ticket category 101 holds 202, which a mute must never touch.
    async fn setup() -> (Harness, MuteService) {
        let h = Harness::start().await;
        h.seed(
            GuildSeed::new(1, "Guild")
                .role(10, "Mod", 5, Permissions::empty())
                .role(20, "Gaia", 10, Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES)
                .channel(100, ChannelType::Category, None)
                .channel(101, ChannelType::Category, None)
                .channel(200, ChannelType::Text, Some(100))
                .channel(201, ChannelType::Voice, Some(100))
                .channel(202, ChannelType::Text, Some(101))
                .overwrite(200, TARGET, false, Permissions::ATTACH_FILES, Permissions::empty())
                .bot(&[20])
                .member(MODERATOR, &[10])
                .member(TARGET, &[])
                .member(PEER, &[10])
                .member(STAFF, &[])
                .member(OWNER, &[]),
        );
        h.repo.update_setting("1", "ticket_category_id", rusqlite::types::Value::Text("101".into())).unwrap();
        let service = MuteService::new(h.repo.clone(), h.discord.clone(), Some(h.audit.clone()));
        (h, service)
    }

    fn overwrite(allow: Permissions, deny: Permissions) -> serde_json::Value {
        json!({"allow": allow.bits().to_string(), "deny": deny.bits().to_string(), "type": 1})
    }

    #[tokio::test]
    async fn mute_then_unmute_applies_and_restores_member_overwrites_outside_ticket_channels() {
        let (h, service) = setup().await;
        let (target, moderator) = (h.member(1, TARGET), h.member(1, MODERATOR));

        let muted = service.mute(G, &target, &moderator, 60_000, Some("spam")).await.unwrap();
        assert_eq!((muted.error, muted.dm_delivered), (None, Some(true)));
        let puts = h.calls_to("PUT", "/channels/*/permissions/4");
        let mut touched: Vec<&str> = puts.iter().map(|c| c.path.as_str()).collect();
        touched.sort();
        assert_eq!(touched, ["/channels/200/permissions/4", "/channels/201/permissions/4"], "categories and ticket channels are skipped");
        for call in &puts {
            let allow = if call.path.contains("/200/") { Permissions::ATTACH_FILES } else { Permissions::empty() };
            assert_eq!(call.body, overwrite(allow, muted_permissions()), "existing allow bits are kept");
            assert_eq!(call.reason.as_deref(), Some("Gaia member mute"));
        }
        assert!(h.calls().iter().all(|c| !c.path.contains("/channels/202/")));
        assert_eq!(service.repo.mute_restores("1", "4").unwrap().len(), 2);

        h.clear_calls();
        let unmuted = service.unmute(G, "4", "appeal", Some(&moderator)).await.unwrap();
        assert!(unmuted.changed);
        assert_eq!(unmuted.dm_delivered, Some(true));
        let restored = h.calls_to("PUT", "/channels/200/permissions/4");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].body, overwrite(Permissions::ATTACH_FILES, Permissions::empty()));
        assert_eq!(restored[0].reason.as_deref(), Some("Gaia mute restoration"));
        let deleted = h.calls_to("DELETE", "/channels/201/permissions/4");
        assert_eq!(deleted.len(), 1, "an overwrite Gaia created is removed again");
        assert_eq!(deleted[0].reason.as_deref(), Some("Gaia mute restoration"));
        assert!(h.calls().iter().all(|c| !c.path.contains("/channels/202/")));
        assert!(service.repo.mute("1", "4").unwrap().is_none());
        assert!(service.repo.mute_restores("1", "4").unwrap().is_empty());

        let titles: Vec<_> = h.audits().await.iter().map(|a| a["title"].clone()).collect();
        assert_eq!(titles, ["Member Muted", "Member Unmuted"]);
    }

    #[tokio::test]
    async fn mute_and_unmute_dms_reach_the_member_without_naming_the_moderator() {
        let (h, service) = setup().await;
        let (target, moderator) = (h.member(1, TARGET), h.member(1, MODERATOR));
        service.mute(G, &target, &moderator, 60_000, Some("spam")).await.unwrap();
        service.unmute(G, "4", "appeal", Some(&moderator)).await.unwrap();
        let opened = h.calls_to("POST", "/users/@me/channels");
        assert!(opened.iter().all(|c| c.body["recipient_id"] == "4"));
        let dms = h.dms();
        assert_eq!(dms.len(), 2);
        assert_eq!(dms[0]["embeds"][0]["title"], "🔇 You have been muted");
        assert_eq!(dms[1]["embeds"][0]["title"], "🔊 You have been unmuted");
        let sent = serde_json::to_string(&dms).unwrap();
        assert!(!sent.to_lowercase().contains("moderator"));
        assert!(!sent.contains("user3"), "moderator's name is not disclosed");
    }

    #[tokio::test]
    async fn a_failed_overwrite_rolls_the_mute_back() {
        let (h, service) = setup().await;
        h.on("PUT", "/channels/201/permissions/4", 403, json!({"code": 50013, "message": "Missing Permissions"}));
        let result = service.mute(G, &h.member(1, TARGET), &h.member(1, MODERATOR), 60_000, None).await.unwrap();
        assert!(result.error.unwrap().contains("could not apply mute channel restrictions"));
        assert!(service.repo.mute("1", "4").unwrap().is_none());
        assert_eq!(h.calls_to("PUT", "/channels/200/permissions/4").last().unwrap().body, overwrite(Permissions::ATTACH_FILES, Permissions::empty()));
        assert!(h.dms().is_empty());
    }

    #[tokio::test]
    async fn moderators_cannot_mute_or_unmute_equal_or_higher_members_or_gaia_staff() {
        let (h, service) = setup().await;
        h.repo.replace_staff("1", &[STAFF.to_string()], "2").unwrap();
        let moderator = h.member(1, MODERATOR);
        let refused = service.mute(G, &h.member(1, PEER), &moderator, 60_000, None).await.unwrap();
        assert_eq!(refused.error.as_deref(), Some("🔴 You cannot mute a member whose highest role is equal to or above yours."));
        let refused = service.mute(G, &h.member(1, STAFF), &moderator, 60_000, None).await.unwrap();
        assert_eq!(refused.error.as_deref(), Some("🔴 You cannot mute Gaia staff."));
        assert!(h.calls().is_empty() && service.repo.mute("1", "5").unwrap().is_none());

        // The owner bypasses the moderator hierarchy; the moderator then cannot lift it.
        let owner = h.member(1, OWNER);
        assert_eq!(service.mute(G, &h.member(1, PEER), &owner, 60_000, None).await.unwrap().error, None);
        let refused = service.unmute(G, "5", "appeal", Some(&moderator)).await.unwrap();
        assert!(!refused.changed);
        assert_eq!(refused.error.as_deref(), Some("🔴 You cannot unmute a member whose highest role is equal to or above yours."));
        assert!(service.repo.mute("1", "5").unwrap().is_some());
        assert!(service.unmute(G, "5", "appeal", Some(&owner)).await.unwrap().changed);
    }
}
