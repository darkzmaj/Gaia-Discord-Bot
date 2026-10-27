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

// shortcut: one rejoin link for every guild Gaia is in; make it a per-guild setting if Gaia serves other communities.
pub const HONEYPOT_REJOIN_URL: &str = "https://darkdragon.dev/discord";

pub fn honeypot_dm(guild_name: &str) -> String {
    format!(
        "You were kicked from **{guild_name}** because you sent a message in the verification channel. That channel is a trap for spam bots.\n\nIf this was a mistake, you can rejoin here: {HONEYPOT_REJOIN_URL}"
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
        let config = self.repo.verification(&guild.to_string())?;
        if config.channel_id.is_none() || config.channel_id != Some(message.channel_id.to_string()) {
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
        let (is_admin, can_kick, guild_name) = {
            let Some(g) = self.discord.cache.guild(guild) else { return Ok(()) };
            let can_kick = g.members.get(&me_id).is_some_and(|me| kickable(&g, me, &member));
            (g.member_permissions(&member).administrator(), can_kick, g.name.clone())
        };
        if is_admin {
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
        // DM before the kick: once the member shares no server with Gaia, Discord refuses the DM.
        let dm_sent = member
            .user
            .id
            .direct_message(http, serenity::builder::CreateMessage::new().content(honeypot_dm(&guild_name)))
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
        let dm = honeypot_dm("DarkDragon");
        assert!(dm.contains("**DarkDragon**"));
        assert!(dm.contains(HONEYPOT_REJOIN_URL));
        assert_eq!(HONEYPOT_REJOIN_URL, "https://darkdragon.dev/discord");
        let claims: Vec<bool> = (0..3).map(|_| service.claim_honeypot_kick("g:spammer".into())).collect();
        assert_eq!(claims, [true, false, false]);
    }
}
