use crate::db::repositories::{GaiaRepository, VerificationConfig, VerificationUpdate};
use crate::discord::Discord;
use crate::services::audit_log_service::{AuditEvent, AuditLogService};
use crate::services::locks::KeyedLock;
use crate::services::mute_service::{highest_role, subject};
use crate::services::voice_room_service::VoiceRoomService;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage,
};
use serenity::model::prelude::*;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub fn normalize_verification_color(input: &str) -> Option<i64> {
    let value = input.trim();
    let value = value.strip_prefix('#').unwrap_or(value);
    (value.len() == 6 && value.bytes().all(|b| b.is_ascii_hexdigit())).then(|| i64::from_str_radix(value, 16).ok()).flatten()
}

/// The DM for a member caught by the honeypot. `invite` is a single-use link back to the
/// same server; without one (no Create Invite permission) the member is pointed at its staff.
pub fn honeypot_dm(guild_name: &str, invite: Option<&str>) -> String {
    let back = match invite {
        Some(url) => format!("If this was a mistake, you can rejoin here: {url}"),
        None => "If this was a mistake, contact the server's moderators to get back in.".to_string(),
    };
    format!(
        "You were kicked from **{guild_name}** because you sent a message in the verification channel. That channel is a trap for spam bots.\n\n{back}"
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerificationPanel {
    pub embeds: Vec<CreateEmbed>,
    pub components: Vec<CreateActionRow>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangeResult {
    pub changed: bool,
    pub error: Option<String>,
}

fn changed(changed: bool) -> ChangeResult {
    ChangeResult { changed, error: None }
}

fn failed(error: &str) -> ChangeResult {
    ChangeResult { changed: false, error: Some(error.into()) }
}

fn ephemeral(content: &str) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true))
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// discord.js `member.kickable`: Kick Members plus a strictly higher top role (owner/self excluded).
fn kickable(guild: &Guild, me: &Member, member: &Member) -> bool {
    if member.user.id == guild.owner_id || member.user.id == me.user.id {
        return false;
    }
    guild.member_permissions(me).kick_members()
        && (me.user.id == guild.owner_id || highest_role(guild, me) > highest_role(guild, member))
}

/// @everyone's effective Send Messages in `channel` (base role permissions + its channel overwrite).
fn everyone_can_send(guild: &Guild, channel: &GuildChannel) -> bool {
    let everyone = guild.id.everyone_role();
    let mut perms = guild.roles.get(&everyone).map_or(Permissions::empty(), |r| r.permissions);
    if perms.administrator() {
        return true;
    }
    if let Some(o) = channel.permission_overwrites.iter().find(|o| o.kind == PermissionOverwriteType::Role(everyone)) {
        perms = (perms - o.deny) | o.allow;
    }
    perms.contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
}

pub struct VerificationService {
    repo: Arc<GaiaRepository>,
    audit: Option<Arc<AuditLogService>>,
    discord: Discord,
    locks: KeyedLock,
    honeypot_kicks: Arc<Mutex<HashSet<String>>>,
}

impl VerificationService {
    pub fn new(repo: Arc<GaiaRepository>, discord: Discord, audit: Option<Arc<AuditLogService>>) -> Self {
        Self { repo, audit, discord, locks: KeyedLock::default(), honeypot_kicks: Arc::default() }
    }

    fn record(&self, event: AuditEvent) {
        if let Some(audit) = &self.audit {
            audit.record(event);
        }
    }

    /// Spam bots post bursts; kick and DM once per burst (the key is released after 60 seconds).
    fn claim_honeypot_kick(&self, key: String) -> bool {
        if !self.honeypot_kicks.lock().unwrap().insert(key.clone()) {
            return false;
        }
        let kicks = self.honeypot_kicks.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            kicks.lock().unwrap().remove(&key);
        });
        true
    }

    /// The verification channel is a honeypot: humans only press the button, so any member message there is a spam bot.
    pub async fn honeypot(&self, message: &Message) -> anyhow::Result<()> {
        let system = !matches!(
            message.kind,
            MessageType::Regular | MessageType::InlineReply | MessageType::ChatInputCommand | MessageType::ContextMenuCommand
        );
        let Some(guild) = message.guild_id else { return Ok(()) };
        if message.author.bot || message.webhook_id.is_some() || system {
            return Ok(());
        }
        // Pure SELECT: this runs for every guild message, so no ensure_guild writes here.
        let Some(config) = self.repo.verification_lookup(&guild.to_string())? else { return Ok(()) };
        let deployed = config.message_id.is_some() && config.channel_id == Some(message.channel_id.to_string());
        if config.enabled == 0 || !deployed {
            return Ok(());
        }
        let http = &self.discord.http;
        let cached = self.discord.cache.guild(guild).and_then(|g| g.members.get(&message.author.id).cloned());
        let member = match cached {
            Some(m) => Some(m),
            None => guild.member(http, message.author.id).await.ok(),
        };
        let Some(member) = member else { return Ok(()) };
        let me_id = self.discord.cache.current_user().id;
        let (exempt, can_kick, guild_name) = {
            let Some(g) = self.discord.cache.guild(guild) else { return Ok(()) };
            let can_kick = g.members.get(&me_id).is_some_and(|me| kickable(&g, me, &member));
            let perms = g.member_permissions(&member);
            (perms.administrator() || perms.manage_messages() || perms.kick_members(), can_kick, g.name.clone())
        };
        if exempt || self.repo.is_staff(&guild.to_string(), &member.user.id.to_string())? {
            return Ok(());
        }
        let _ = message.delete(http).await;
        if !self.claim_honeypot_kick(format!("{guild}:{}", member.user.id)) {
            return Ok(());
        }
        let guild_subject = subject(guild, Some(guild_name.clone()));
        let target = subject(member.user.id, Some(member.user.tag()));
        if !can_kick {
            self.record(AuditEvent {
                title: "Honeypot Kick Failed".into(),
                category: "Verification".into(),
                color: "danger".into(),
                guild: guild_subject,
                target,
                reason: Some("Gaia cannot kick this member. Check Kick Members and role hierarchy.".into()),
                ..Default::default()
            });
            return Ok(());
        }
        // A one-use, one-day invite to this server's verification channel, so a mistaken human can come back.
        let invite = message
            .channel_id
            .create_invite(
                http,
                serenity::builder::CreateInvite::new()
                    .max_age(86_400)
                    .max_uses(1)
                    .unique(true)
                    .audit_log_reason("Gaia honeypot: rejoin link for the kicked member"),
            )
            .await
            .ok()
            .map(|i| i.url());
        // DM before the kick: once the member shares no server with Gaia, Discord refuses the DM.
        let dm_sent = member
            .user
            .id
            .direct_message(
                http,
                serenity::builder::CreateMessage::new().content(honeypot_dm(&guild_name, invite.as_deref())),
            )
            .await
            .is_ok();
        match guild.kick_with_reason(http, member.user.id, "Gaia honeypot: sent a message in the verification channel").await {
            Ok(()) => self.record(AuditEvent {
                title: "Honeypot Kick".into(),
                category: "Verification".into(),
                color: "warning".into(),
                guild: guild_subject,
                target,
                details: vec![("DM Sent".into(), dm_sent.into())],
                ..Default::default()
            }),
            Err(e) => self.record(AuditEvent {
                title: "Honeypot Kick Failed".into(),
                category: "Verification".into(),
                color: "danger".into(),
                guild: guild_subject,
                target,
                reason: Some(e.to_string()),
                ..Default::default()
            }),
        }
        Ok(())
    }

    pub async fn button(&self, interaction: &ComponentInteraction) -> anyhow::Result<()> {
        let Some(guild) = interaction.guild_id else { return Ok(()) };
        let key = format!("{guild}:{}", interaction.user.id);
        self.locks.run(&key, self.button_locked(interaction, guild)).await
    }

    async fn button_locked(&self, interaction: &ComponentInteraction, guild: GuildId) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let reply = |content: &'static str| interaction.create_response(http, ephemeral(content));
        let config = self.repo.verification(&guild.to_string())?;
        if config.enabled == 0 {
            reply("🔴 Verification is currently disabled on this server.").await?;
            return Ok(());
        }
        let role = config
            .role_id
            .as_deref()
            .and_then(|r| r.parse::<RoleId>().ok())
            .and_then(|r| self.discord.cache.guild(guild)?.roles.get(&r).cloned());
        let member = guild.member(http, interaction.user.id).await.ok();
        let (Some(role), Some(member)) = (role, member) else {
            reply("Verification is not configured correctly. Please contact an administrator.").await?;
            return Ok(());
        };
        if !self.assignable(guild, &role) {
            reply("Gaia cannot assign the configured verification role.").await?;
            return Ok(());
        }
        if member.roles.contains(&role.id) {
            reply("🟢 You are already verified.").await?;
            return Ok(());
        }
        let guild_subject = subject(guild, Some(self.discord.cache.guild(guild).map(|g| g.name.clone()).unwrap_or_default()));
        match http.add_member_role(guild, member.user.id, role.id, Some("Gaia verification")).await {
            Ok(()) => {
                self.record(AuditEvent {
                    title: "Member Verified".into(),
                    category: "Verification".into(),
                    color: "success".into(),
                    guild: guild_subject,
                    target: subject(member.user.id, Some(member.user.tag())),
                    details: vec![("Role".into(), role.name.clone().into())],
                    ..Default::default()
                });
                reply("🟢 You have been verified.").await?;
            }
            Err(_) => {
                self.record(AuditEvent {
                    title: "Verification Failed".into(),
                    category: "Verification".into(),
                    color: "danger".into(),
                    priority: "critical".into(),
                    guild: guild_subject,
                    target: subject(interaction.user.id, Some(interaction.user.tag())),
                    reason: Some("Gaia could not assign the verification role.".into()),
                    ..Default::default()
                });
                reply("Gaia could not assign the verification role.").await?;
            }
        }
        Ok(())
    }

    pub async fn unverify(&self, guild: GuildId, user_id: &str, executor: Option<&Member>) -> anyhow::Result<ChangeResult> {
        let user_id: UserId = user_id.parse()?;
        let config = self.repo.verification(&guild.to_string())?;
        let Some(role_id) = config.role_id.as_deref() else {
            return Ok(failed("🔴 No verification role is configured for this server."));
        };
        let role = role_id.parse::<RoleId>().ok().and_then(|r| self.discord.cache.guild(guild)?.roles.get(&r).cloned());
        let Some(role) = role else {
            return Ok(failed("🔴 The configured verification role no longer exists."));
        };
        if !self.assignable(guild, &role) {
            return Ok(failed("Gaia cannot manage the configured verification role."));
        }
        self.locks
            .run(&format!("{guild}:{user_id}"), async {
                let http = &self.discord.http;
                let Ok(member) = guild.member(http, user_id).await else {
                    return Ok(failed("That user is not a member of this server."));
                };
                if !self.manageable(guild, &member) {
                    return Ok(failed("Gaia cannot manage that member because of role hierarchy."));
                }
                if !member.roles.contains(&role.id) {
                    return Ok(changed(false));
                }
                if http.remove_member_role(guild, user_id, role.id, Some("Gaia unverify")).await.is_err() {
                    return Ok(failed("Gaia could not remove the verification role."));
                }
                self.record(AuditEvent {
                    title: "Member Unverified".into(),
                    category: "Verification".into(),
                    color: "warning".into(),
                    priority: "critical".into(),
                    guild: subject(guild, Some(self.discord.cache.guild(guild).map(|g| g.name.clone()).unwrap_or_default())),
                    target: subject(member.user.id, Some(member.user.tag())),
                    actor: executor.and_then(|m| subject(m.user.id, Some(m.user.tag()))),
                    details: vec![("Role".into(), role.name.clone().into())],
                    ..Default::default()
                });
                Ok(changed(true))
            })
            .await
    }

    /// discord.js `member.manageable`.
    fn manageable(&self, guild: GuildId, member: &Member) -> bool {
        let me_id = self.discord.cache.current_user().id;
        let Some(g) = self.discord.cache.guild(guild) else { return false };
        if member.user.id == g.owner_id || member.user.id == me_id {
            return false;
        }
        if me_id == g.owner_id {
            return true;
        }
        g.members.get(&me_id).is_some_and(|me| highest_role(&g, me) > highest_role(&g, member))
    }

    pub fn assignable(&self, guild: GuildId, role: &Role) -> bool {
        let me_id = self.discord.cache.current_user().id;
        let Some(g) = self.discord.cache.guild(guild) else { return false };
        let Some(me) = g.members.get(&me_id) else { return false };
        g.member_permissions(me).manage_roles() && !role.managed && highest_role(&g, me).is_some_and(|top| top > *role)
    }

    pub fn configuration_error(&self, guild: GuildId, config: &VerificationConfig, channel: Option<&GuildChannel>) -> Option<String> {
        let Some(role_id) = config.role_id.as_deref() else { return Some("Set a verification role first.".into()) };
        let role = role_id.parse::<RoleId>().ok().and_then(|r| self.discord.cache.guild(guild)?.roles.get(&r).cloned());
        let Some(role) = role else { return Some("The configured verification role no longer exists.".into()) };
        if !self.assignable(guild, &role) {
            return Some("Gaia cannot assign the configured verification role.".into());
        }
        if config.title.trim().is_empty()
            || utf16_len(&config.title) > 256
            || config.description.trim().is_empty()
            || utf16_len(&config.description) > 4096
            || config.button_text.trim().is_empty()
            || utf16_len(&config.button_text) > 80
            || !(0..=0xffffff).contains(&config.color)
        {
            return Some("The verification message configuration is invalid.".into());
        }
        if let Some(channel) = channel {
            let me_id = self.discord.cache.current_user().id;
            let text = matches!(channel.kind, ChannelType::Text | ChannelType::News | ChannelType::Voice | ChannelType::Stage);
            let g = self.discord.cache.guild(guild);
            let Some(me) = g.as_ref().and_then(|g| g.members.get(&me_id)).filter(|_| text) else {
                return Some("Choose a text channel.".into());
            };
            let g = g.as_ref()?;
            let needed = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS;
            if !g.user_permissions_in(channel, me).contains(needed) {
                return Some("Gaia needs View Channel, Send Messages, and Embed Links in that channel.".into());
            }
            if everyone_can_send(g, channel) {
                return Some(
                    "@everyone can send messages in that channel. Gaia kicks anyone who posts in the verification channel, so make it read-only for @everyone first.".into(),
                );
            }
        }
        None
    }

    /// TS `panel()`: `{ embeds, components }`.
    pub fn panel(&self, config: &VerificationConfig, disabled: bool) -> VerificationPanel {
        let embed = CreateEmbed::new().color(config.color as u32).title(&config.title).description(&config.description);
        let button = CreateButton::new("verify:click").label(&config.button_text).style(ButtonStyle::Success).disabled(disabled);
        VerificationPanel { embeds: vec![embed], components: vec![CreateActionRow::Buttons(vec![button])] }
    }

    pub async fn set_enabled(&self, guild: GuildId, enabled: bool, rooms: &VoiceRoomService) -> anyhow::Result<ChangeResult> {
        let gid = guild.to_string();
        let settings = self.repo.settings(&gid)?;
        let roles = self.repo.autoroles(&gid)?;
        let current = self.repo.verification(&gid)?;
        if enabled && settings.autorole_enabled != 0 && roles.user_role_id.is_some() {
            return Ok(failed(
                "🔴 Verification Cannot Be Enabled\n\nDisable User AutoRole first. User AutoRole automatically assigns roles when members join, which conflicts with Gaia verification.",
            ));
        }
        if current.enabled == enabled as i64 {
            return Ok(changed(false));
        }
        self.repo.update_verification(&gid, &VerificationUpdate { enabled: Some(enabled as i64), ..Default::default() })?;
        self.record(AuditEvent {
            title: format!("Verification {}", if enabled { "Enabled" } else { "Disabled" }),
            category: "Verification".into(),
            color: "warning".into(),
            guild: subject(guild, Some(self.discord.cache.guild(guild).map(|g| g.name.clone()).unwrap_or_default())),
            ..Default::default()
        });
        rooms.reconcile_guild_access(guild, current.role_id.as_deref()).await?;
        Ok(changed(true))
    }

    pub async fn disable_for_user_autorole(&self, guild: GuildId, rooms: &VoiceRoomService) -> anyhow::Result<bool> {
        let gid = guild.to_string();
        let config = self.repo.verification(&gid)?;
        if config.enabled == 0 {
            return Ok(false);
        }
        self.repo.update_verification(&gid, &VerificationUpdate { enabled: Some(0), ..Default::default() })?;
        rooms.reconcile_guild_access(guild, config.role_id.as_deref()).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::cache::Cache;
    use serenity::http::Http;

    fn setup() -> (Arc<GaiaRepository>, VerificationService, VoiceRoomService) {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let repo = Arc::new(GaiaRepository::new(conn));
        let discord = Discord { http: Arc::new(Http::new("")), cache: Arc::new(Cache::new()) };
        let service = VerificationService::new(repo.clone(), discord.clone(), None);
        let rooms = VoiceRoomService::new(repo.clone(), discord);
        (repo, service, rooms)
    }

    const A: GuildId = GuildId::new(1);

    #[test]
    fn normalizes_custom_hexadecimal_colors_and_rejects_malformed_values() {
        assert_eq!(normalize_verification_color("#57F287"), Some(0x57f287));
        assert_eq!(normalize_verification_color("57f287"), Some(0x57f287));
        assert_eq!(normalize_verification_color("#GG0000"), None);
        assert_eq!(normalize_verification_color("#12345"), None);
    }

    #[tokio::test]
    async fn refuses_enabling_verification_while_effective_human_autorole_is_active() {
        let (repo, service, rooms) = setup();
        repo.set_autorole("1", "user_role_id", Some("player")).unwrap();
        let result = service.set_enabled(A, true, &rooms).await.unwrap();
        assert!(result.error.unwrap().contains("Verification Cannot Be Enabled"));
        assert_eq!(repo.verification("1").unwrap().enabled, 0);
    }

    #[tokio::test]
    async fn does_not_treat_bot_autorole_or_an_unconfigured_user_autorole_as_a_conflict() {
        let (repo, service, rooms) = setup();
        repo.set_autorole("1", "bot_role_id", Some("bots")).unwrap();
        assert_eq!(service.set_enabled(A, true, &rooms).await.unwrap().error, None);
        assert_eq!(repo.verification("1").unwrap().enabled, 1);
    }

    #[tokio::test]
    async fn automatically_disables_persisted_verification_when_user_autorole_becomes_active() {
        let (repo, service, rooms) = setup();
        let update = VerificationUpdate { enabled: Some(1), role_id: Some(Some("verified".into())), ..Default::default() };
        repo.update_verification("1", &update).unwrap();
        assert!(service.disable_for_user_autorole(A, &rooms).await.unwrap());
        assert_eq!(repo.verification("1").unwrap().enabled, 0);
    }

    #[test]
    fn keeps_verification_and_its_deployment_isolated_between_guilds() {
        let (repo, _, _) = setup();
        for g in ["a", "b"] {
            let update = VerificationUpdate {
                enabled: Some(1),
                channel_id: Some(Some(format!("channel-{g}"))),
                message_id: Some(Some(format!("message-{g}"))),
                ..Default::default()
            };
            repo.update_verification(g, &update).unwrap();
        }
        let a = repo.verification("a").unwrap();
        let b = repo.verification("b").unwrap();
        assert_eq!((a.channel_id.as_deref(), a.message_id.as_deref()), (Some("channel-a"), Some("message-a")));
        assert_eq!((b.channel_id.as_deref(), b.message_id.as_deref()), (Some("channel-b"), Some("message-b")));
    }

    #[tokio::test]
    async fn honeypot_dm_names_the_guild_and_rejoin_link_and_kicks_once_per_burst() {
        let (_, service, _) = setup();
        let dm = honeypot_dm("DarkDragon", Some("https://discord.gg/abc123"));
        assert!(dm.contains("**DarkDragon**") && dm.contains("https://discord.gg/abc123"));
        let dm = honeypot_dm("DarkDragon", None);
        assert!(dm.contains("contact the server's moderators") && !dm.contains("discord.gg"));
        let claims: Vec<bool> = (0..3).map(|_| service.claim_honeypot_kick("g:spammer".into())).collect();
        assert_eq!(claims, [true, false, false]);
    }

    // ---- behaviour against the fake Discord ----

    use crate::test_support::{GuildSeed, Harness, BOT_ID};
    use serde_json::json;

    const VERIFY: u64 = 300;
    const VERIFIED_ROLE: u64 = 30;
    const SPAMMER: u64 = 5;
    const BOSS: u64 = 6;
    const NEWCOMER: u64 = 7;
    const MODERATOR: u64 = 8;
    const VERIFIED: u64 = 9;
    const HELPER: u64 = 10;
    const BOUNCER: u64 = 11;
    const STAFF: u64 = 12;

    /// Gaia (role 20, position 10) can kick and manage roles; the moderator's role 50 sits above it.
    async fn live() -> (Harness, VerificationService) {
        let h = Harness::start().await;
        h.seed(
            GuildSeed::new(1, "DarkDragon")
                .role(VERIFIED_ROLE, "Verified", 1, Permissions::empty())
                .role(20, "Gaia", 10, Permissions::MANAGE_ROLES | Permissions::KICK_MEMBERS)
                .role(40, "Admin", 2, Permissions::ADMINISTRATOR)
                .role(50, "Moderator", 15, Permissions::empty())
                .channel(VERIFY, ChannelType::Text, None)
                .channel(301, ChannelType::Text, None)
                .channel(302, ChannelType::Text, None)
                .overwrite(302, 1, true, Permissions::empty(), Permissions::SEND_MESSAGES)
                .overwrite(302, BOT_ID, false, Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS, Permissions::empty())
                .channel(303, ChannelType::Text, None)
                .overwrite(303, BOT_ID, false, Permissions::EMBED_LINKS, Permissions::empty())
                .role(60, "Helper", 3, Permissions::MANAGE_MESSAGES)
                .role(61, "Bouncer", 3, Permissions::KICK_MEMBERS)
                .member(HELPER, &[60])
                .member(BOUNCER, &[61])
                .member(STAFF, &[])
                .bot(&[20])
                .member(SPAMMER, &[])
                .member(BOSS, &[40])
                .member(NEWCOMER, &[])
                .member(MODERATOR, &[50])
                .member(VERIFIED, &[VERIFIED_ROLE]),
        );
        let update = VerificationUpdate {
            enabled: Some(1),
            role_id: Some(Some(VERIFIED_ROLE.to_string())),
            channel_id: Some(Some(VERIFY.to_string())),
            message_id: Some(Some("400".into())),
            ..Default::default()
        };
        h.repo.update_verification("1", &update).unwrap();
        let service = VerificationService::new(h.repo.clone(), h.discord.clone(), Some(h.audit.clone()));
        (h, service)
    }

    fn field(audit: &serde_json::Value, name: &str) -> Option<String> {
        audit["fields"].as_array()?.iter().find(|f| f["name"] == name).and_then(|f| f["value"].as_str()).map(str::to_string)
    }

    #[tokio::test]
    async fn serializes_rapid_verification_clicks_so_the_role_is_assigned_once() {
        let (h, service) = live().await;
        let (a, b) = (h.button(1, NEWCOMER, "verify:click"), h.button(1, NEWCOMER, "verify:click"));
        let (first, second) = tokio::join!(service.button(&a), service.button(&b));
        first.unwrap();
        second.unwrap();
        let adds = h.calls_to("PUT", "/guilds/1/members/7/roles/30");
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].reason.as_deref(), Some("Gaia verification"));
        let replies: Vec<_> = h.replies().iter().map(|r| r["data"]["content"].clone()).collect();
        assert_eq!(replies, ["🟢 You have been verified.", "🟢 You are already verified."]);
        assert!(h.replies().iter().all(|r| r["data"]["flags"] == 64), "replies are ephemeral");
    }

    #[tokio::test]
    async fn unverifies_only_a_current_guild_member_holding_the_configured_role() {
        let (h, service) = live().await;
        assert_eq!(service.unverify(A, "9", None).await.unwrap(), changed(true));
        assert_eq!(service.unverify(A, "9", None).await.unwrap(), changed(false));
        assert_eq!(service.unverify(A, "999", None).await.unwrap(), failed("That user is not a member of this server."));
        let removals = h.calls_to("DELETE", "/guilds/1/members/*/roles/30");
        assert_eq!(removals.len(), 1);
        assert_eq!(removals[0].path, "/guilds/1/members/9/roles/30");
        assert_eq!(removals[0].reason.as_deref(), Some("Gaia unverify"));
    }

    #[tokio::test]
    async fn honeypot_deletes_the_message_dms_the_rejoin_link_then_kicks() {
        let (h, service) = live().await;
        h.on(
            "POST",
            "/channels/300/invites",
            200,
            json!({
                "code": "back123", "created_at": "2026-11-04T12:00:00.000000+00:00",
                "channel": {"id": "300", "name": "verify", "type": 0},
                "max_age": 86400, "max_uses": 1, "temporary": false, "uses": 0
            }),
        );
        let message = h.message(1, VERIFY, SPAMMER, "buy cheap nitro");
        service.honeypot(&message).await.unwrap();
        let invite = h.calls_to("POST", "/channels/300/invites");
        assert_eq!((invite[0].body["max_uses"].as_u64(), invite[0].body["max_age"].as_u64()), (Some(1), Some(86400)));
        let dm_channel = h.calls_to("POST", "/users/@me/channels");
        assert_eq!(dm_channel[0].body["recipient_id"], "5");
        let trace = h.trace();
        assert_eq!(trace.len(), 5);
        assert_eq!(trace[0], format!("DELETE /channels/300/messages/{}", message.id));
        assert_eq!(trace[1], "POST /channels/300/invites");
        assert_eq!(trace[2], "POST /users/@me/channels");
        assert!(trace[3].starts_with("POST /channels/") && trace[3].ends_with("/messages"));
        assert_eq!(trace[4], "DELETE /guilds/1/members/5");
        let dm = h.dms()[0]["content"].as_str().unwrap().to_string();
        assert!(dm.contains("**DarkDragon**") && dm.contains("https://discord.gg/back123"), "{dm}");
        assert_eq!(h.calls()[4].reason.as_deref(), Some("Gaia honeypot: sent a message in the verification channel"));
        let audits = h.audits().await;
        assert_eq!(audits[0]["title"], "Honeypot Kick");
        assert_eq!(field(&audits[0], "DM Sent").as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn honeypot_still_kicks_when_the_member_has_dms_closed() {
        let (h, service) = live().await;
        h.on("POST", "/channels/*/messages", 403, json!({"code": 50007, "message": "Cannot send messages to this user"}));
        service.honeypot(&h.message(1, VERIFY, SPAMMER, "spam")).await.unwrap();
        assert_eq!(h.calls_to("DELETE", "/channels/300/messages/*").len(), 1);
        assert_eq!(h.calls_to("DELETE", "/guilds/1/members/5").len(), 1);
        let audits = h.audits().await;
        assert_eq!(audits[0]["title"], "Honeypot Kick");
        assert_eq!(field(&audits[0], "DM Sent").as_deref(), Some("false"));
    }

    #[tokio::test]
    async fn honeypot_ignores_other_channels_bots_webhooks_system_messages_and_administrators() {
        let (h, service) = live().await;
        let mut messages = vec![h.message(1, 301, SPAMMER, "hi"), h.message(1, VERIFY, BOSS, "testing the trap")];
        let mut bot = h.message(1, VERIFY, SPAMMER, "beep");
        bot.author.bot = true;
        let mut webhook = h.message(1, VERIFY, SPAMMER, "hook");
        webhook.webhook_id = Some(WebhookId::new(77));
        let mut system = h.message(1, VERIFY, SPAMMER, "");
        system.kind = MessageType::MemberJoin;
        let mut direct = h.message(1, VERIFY, SPAMMER, "dm");
        direct.guild_id = None;
        messages.extend([bot, webhook, system, direct]);
        for message in &messages {
            service.honeypot(message).await.unwrap();
        }
        assert_eq!(h.trace(), Vec::<String>::new());
        assert!(h.audits().await.is_empty());
    }

    #[tokio::test]
    async fn honeypot_does_nothing_until_a_verification_message_has_been_deployed() {
        let (h, service) = live().await;
        h.repo.update_verification("1", &VerificationUpdate { channel_id: Some(None), ..Default::default() }).unwrap();
        service.honeypot(&h.message(1, VERIFY, SPAMMER, "spam")).await.unwrap();
        assert_eq!(h.trace(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn honeypot_deletes_but_reports_a_failure_when_gaia_cannot_kick_the_member() {
        let (h, service) = live().await;
        let message = h.message(1, VERIFY, MODERATOR, "hello?");
        service.honeypot(&message).await.unwrap();
        assert_eq!(h.trace(), [format!("DELETE /channels/300/messages/{}", message.id)]);
        let audits = h.audits().await;
        assert_eq!(audits[0]["title"], "Honeypot Kick Failed");
        assert!(field(&audits[0], "Reason").unwrap().contains("Kick Members"));
    }

    #[tokio::test]
    async fn honeypot_ignores_disabled_verification_and_undeployed_messages() {
        let (h, service) = live().await;
        h.repo.update_verification("1", &VerificationUpdate { enabled: Some(0), ..Default::default() }).unwrap();
        service.honeypot(&h.message(1, VERIFY, SPAMMER, "spam")).await.unwrap();
        h.repo.update_verification("1", &VerificationUpdate { enabled: Some(1), message_id: Some(None), ..Default::default() }).unwrap();
        service.honeypot(&h.message(1, VERIFY, SPAMMER, "spam")).await.unwrap();
        assert_eq!(h.trace(), Vec::<String>::new());
        assert!(h.audits().await.is_empty());
    }

    #[tokio::test]
    async fn honeypot_ignores_gaia_staff_and_members_who_can_manage_messages_or_kick() {
        let (h, service) = live().await;
        h.repo.replace_staff("1", &[STAFF.to_string()], "2").unwrap();
        for author in [HELPER, BOUNCER, STAFF] {
            service.honeypot(&h.message(1, VERIFY, author, "explaining the channel")).await.unwrap();
        }
        assert_eq!(h.trace(), Vec::<String>::new());
        assert!(h.audits().await.is_empty());
    }

    #[tokio::test]
    async fn honeypot_lookup_never_creates_rows_for_unknown_guilds() {
        let (h, service) = live().await;
        service.honeypot(&h.message(2, VERIFY, SPAMMER, "spam")).await.unwrap();
        assert!(h.repo.verification_lookup("2").unwrap().is_none());
        assert_eq!(h.trace(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn refuses_to_deploy_into_a_channel_where_everyone_can_send_messages() {
        let (h, service) = live().await;
        let config = h.repo.verification("1").unwrap();
        let channel = |id: u64| h.discord.cache.guild(A).unwrap().channels.get(&ChannelId::new(id)).cloned().unwrap();
        assert_eq!(service.configuration_error(A, &config, Some(&channel(302))), None);
        let error = service.configuration_error(A, &config, Some(&channel(303))).unwrap();
        assert!(error.contains("@everyone can send messages"), "{error}");
    }
}
