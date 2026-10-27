//! `/admin` ephemeral dashboard: AutoRole, Voice Rooms, Staff, Moderation, Verification, Backups and
//! role management. Every component re-checks Discord Administrator before acting.
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serenity::all::{
    ButtonStyle, ChannelId, ChannelType, CommandInteraction, ComponentInteraction, ComponentInteractionDataKind,
    Context, CreateActionRow, CreateButton, CreateEmbed, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption,
    EditInteractionResponse, GuildChannel, GuildId, InputTextStyle, Member, Message, MessageId, ModalInteraction, Role,
    RoleId, User, UserId,
};

use crate::ctx;
use crate::db::repositories::{BackupSettingsUpdate, GaiaRepository, VerificationConfig, VerificationUpdate};
use crate::discord::Discord;
use crate::logger::log_discord_error;
use crate::services::authorization::{has_permission, PermissionLevel};
use crate::services::audit_log_service::{AuditEvent, AuditLogService, AuditSubject};
use crate::services::backup_service::{BackupAlreadyRunningError, BackupService, MemberSynchronizationError, RestoreProgress};
use crate::services::mute_service::MuteService;
use crate::services::verification_service::{normalize_verification_color, VerificationService};
use crate::services::voice_room_service::VoiceRoomService;

const COLOR: u32 = crate::discord::GAIA_PURPLE;
const AUTOROLE_CONFLICT: &str = "🟡 Verification Disabled\n\nVerification was automatically disabled because User AutoRole is enabled. User AutoRole and Verification cannot be enabled at the same time.";
const CANNOT_USE_ROLE: &str =
    "Gaia cannot use that role. Check Manage Roles, role hierarchy, and that it is not an integration role.";
const DEPLOYMENT_GONE: &str =
    "🟡 The tracked verification message no longer exists. Its deployment record was cleared.";

/// Whether a member's role state would actually change: holders for remove, non-holders for add.
pub fn needs_role_change(is_bot: bool, has_role: bool, mode: &str, target: &str) -> bool {
    (target != "bots" || is_bot) && has_role == (mode == "remove")
}

/// Members whose role state would actually change: holders for remove, non-holders for add.
pub fn role_change_targets(members: impl IntoIterator<Item = Member>, mode: &str, role_id: RoleId, target: &str) -> Vec<Member> {
    members
        .into_iter()
        .filter(|m| needs_role_change(m.user.bot, m.roles.contains(&role_id), mode, target))
        .collect()
}

/// Discord role ordering (position, then older id higher), as discord.js `comparePositionTo`.
type RoleKey = (u16, Reverse<u64>);

/// The pure part of `managedRole`: why a role management step must be refused, if at all.
fn managed_role_error(
    mode: &str,
    role: Option<RoleKey>,
    guild_id: u64,
    assignable: impl FnOnce() -> bool,
    is_owner: bool,
    actor_highest: Option<RoleKey>,
) -> Option<&'static str> {
    if mode != "add" && mode != "remove" {
        return Some("Unknown role action.");
    }
    let Some(role) = role.filter(|r| r.1 .0 != guild_id) else {
        return Some("That role no longer exists or cannot be assigned.");
    };
    if !assignable() {
        return Some(CANNOT_USE_ROLE);
    }
    if !is_owner && !actor_highest.is_some_and(|actor| actor > role) {
        return Some("You can only manage roles below your own highest role.");
    }
    None
}

fn role_result(verb: &str, role_id: RoleId, preposition: &str, group: &str, changed: usize, failed: &[UserId]) -> String {
    let past = if verb == "Add" { "Added" } else { "Removed" };
    let failures = if failed.is_empty() {
        String::new()
    } else {
        format!(
            "\n🔴 Failed for {}: {}",
            failed.len(),
            failed.iter().take(10).map(|id| format!("<@{id}>")).collect::<Vec<_>>().join(" ")
        )
    };
    format!(
        "{} {past} <@&{role_id}> {preposition} {changed} {group}{}.{failures}",
        if failed.is_empty() { "🟢" } else { "🟡" },
        if changed == 1 { "" } else { "s" }
    )
}

fn section_for(key: &str) -> &'static str {
    match key {
        "autorole_enabled" => "autorole",
        "voice_rooms_enabled" => "voice",
        _ => "moderation",
    }
}

fn snowflake(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|v| *v != 0)
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn parse_time(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value).ok().map(|t| t.with_timezone(&chrono::Utc))
}

/// `new Date(iso).toLocaleString("en-GB", { timeZone: "Europe/Ljubljana" })`.
fn ljubljana(value: &str) -> String {
    parse_time(value)
        .map(|t| t.with_timezone(&chrono_tz::Europe::Ljubljana).format("%d/%m/%Y, %H:%M:%S").to_string())
        .unwrap_or_else(|| "Invalid Date".into())
}

fn tag(user: &User) -> String {
    user.tag()
}

/// An interaction payload: `None` content leaves the message's current content untouched (like an absent key in discord.js).
#[derive(Default)]
struct View {
    content: Option<String>,
    embeds: Vec<CreateEmbed>,
    components: Vec<CreateActionRow>,
}

impl View {
    fn text(content: impl Into<String>, components: Vec<CreateActionRow>) -> Self {
        View { content: Some(content.into()), embeds: vec![], components }
    }
    fn embed(embed: CreateEmbed, components: Vec<CreateActionRow>) -> Self {
        View { content: None, embeds: vec![embed], components }
    }
    fn content(mut self, content: Option<String>) -> Self {
        if content.is_some() {
            self.content = content;
        }
        self
    }
    fn message(self) -> CreateInteractionResponseMessage {
        let mut out = CreateInteractionResponseMessage::new().embeds(self.embeds).components(self.components);
        if let Some(content) = self.content {
            out = out.content(content);
        }
        out
    }
    fn update(self) -> CreateInteractionResponse {
        CreateInteractionResponse::UpdateMessage(self.message())
    }
    fn edit(self) -> EditInteractionResponse {
        let mut out = EditInteractionResponse::new().embeds(self.embeds).components(self.components);
        if let Some(content) = self.content {
            out = out.content(content);
        }
        out
    }
}

fn ephemeral(content: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true))
}

fn button(custom_id: String, label: &str, style: ButtonStyle) -> CreateButton {
    CreateButton::new(custom_id).label(label).style(style)
}

fn nav(section: &str, guild_id: GuildId, label: &str) -> CreateButton {
    nav_styled(section, guild_id, label, ButtonStyle::Primary)
}

fn nav_styled(section: &str, guild_id: GuildId, label: &str, style: ButtonStyle) -> CreateButton {
    button(format!("adminui:nav:{section}:{guild_id}"), label, style)
}

fn toggle(key: &str, guild_id: GuildId, enabled: bool) -> CreateButton {
    button(
        format!("adminui:toggle:{key}:{guild_id}"),
        if enabled { "Disable" } else { "Enable" },
        if enabled { ButtonStyle::Danger } else { ButtonStyle::Success },
    )
}

fn pick(action: &str, guild_id: GuildId, label: &str, disabled: bool) -> CreateButton {
    button(format!("adminui:pick:{action}:{guild_id}"), label, ButtonStyle::Primary).disabled(disabled)
}

fn remove(action: &str, guild_id: GuildId, label: &str, disabled: bool) -> CreateButton {
    button(format!("adminui:remove:{action}:{guild_id}"), label, ButtonStyle::Secondary).disabled(disabled)
}

fn row(buttons: Vec<CreateButton>) -> CreateActionRow {
    CreateActionRow::Buttons(buttons)
}

fn select(custom_id: String, kind: CreateSelectMenuKind, placeholder: &str) -> CreateActionRow {
    CreateActionRow::SelectMenu(CreateSelectMenu::new(custom_id, kind).placeholder(placeholder))
}

fn role_select(custom_id: String) -> CreateActionRow {
    select(custom_id, CreateSelectMenuKind::Role { default_roles: None }, "Select a role")
}

fn user_select(custom_id: String) -> CreateActionRow {
    select(custom_id, CreateSelectMenuKind::User { default_users: None }, "Select a member")
}

fn channel_select(custom_id: String, types: Vec<ChannelType>, placeholder: &str) -> CreateActionRow {
    select(custom_id, CreateSelectMenuKind::Channel { channel_types: Some(types), default_channels: None }, placeholder)
}

fn text_input(style: InputTextStyle, label: &str, id: &str, max: u16) -> CreateInputText {
    CreateInputText::new(style, label, id).required(true).max_length(max)
}

fn guild_name(ctx: &Context, guild_id: GuildId) -> String {
    ctx.cache.guild(guild_id).map(|g| g.name.clone()).unwrap_or_default()
}

fn cached_role(ctx: &Context, guild_id: GuildId, role_id: &str) -> Option<Role> {
    let id = RoleId::new(snowflake(role_id)?);
    ctx.cache.guild(guild_id)?.roles.get(&id).cloned()
}

fn role_key(role: &Role) -> RoleKey {
    (role.position, Reverse(role.id.get()))
}

/// Highest role of a member (falls back to @everyone), as discord.js `member.roles.highest`.
fn highest_role_key(ctx: &Context, guild_id: GuildId, member: &Member) -> Option<RoleKey> {
    let guild = ctx.cache.guild(guild_id)?;
    let everyone = guild.roles.get(&RoleId::new(guild_id.get())).map(role_key).unwrap_or((0, Reverse(guild_id.get())));
    Some(member.roles.iter().filter_map(|id| guild.roles.get(id)).map(role_key).fold(everyone, std::cmp::max))
}

fn modal_value(i: &ModalInteraction, id: &str) -> String {
    i.data
        .components
        .iter()
        .flat_map(|row| row.components.iter())
        .find_map(|c| match c {
            serenity::all::ActionRowComponent::InputText(input) if input.custom_id == id => input.value.clone(),
            _ => None,
        })
        .unwrap_or_default()
}

fn first_value(i: &ComponentInteraction) -> String {
    match &i.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => values.first().cloned(),
        ComponentInteractionDataKind::UserSelect { values } => values.first().map(|v| v.to_string()),
        ComponentInteractionDataKind::RoleSelect { values } => values.first().map(|v| v.to_string()),
        ComponentInteractionDataKind::ChannelSelect { values } => values.first().map(|v| v.to_string()),
        ComponentInteractionDataKind::MentionableSelect { values } => values.first().map(|v| v.to_string()),
        _ => None,
    }
    .unwrap_or_default()
}

/// The `[_, kind, action, guildId]` parts of an `adminui:` custom ID; missing parts are empty.
fn parts(custom_id: &str) -> (&str, &str, &str) {
    let mut split = custom_id.split(':').skip(1);
    (split.next().unwrap_or(""), split.next().unwrap_or(""), split.next().unwrap_or(""))
}

fn deployment_token() -> String {
    fn base36(mut n: u128) -> String {
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut out = vec![];
        loop {
            out.push(digits[(n % 36) as usize]);
            n /= 36;
            if n == 0 {
                break;
            }
        }
        out.reverse();
        String::from_utf8(out).unwrap()
    }
    let millis = chrono::Utc::now().timestamp_millis().max(0) as u128;
    let random = base36(uuid::Uuid::new_v4().as_u128());
    format!("{}{}", base36(millis), &random[..6.min(random.len())])
}

/// Pending "Send verification message" confirmations: they expire with the interaction token and are capped.
const DEPLOYMENT_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_DEPLOYMENTS: usize = 256;

struct DeploymentSession {
    guild_id: GuildId,
    user_id: UserId,
    channel_id: ChannelId,
    created: Instant,
}

fn remember_deployment(sessions: &mut HashMap<String, DeploymentSession>, token: String, session: DeploymentSession) {
    sessions.retain(|_, s| s.created.elapsed() < DEPLOYMENT_TTL);
    if sessions.len() >= MAX_DEPLOYMENTS {
        if let Some(oldest) = sessions.iter().min_by_key(|(_, s)| s.created).map(|(k, _)| k.clone()) {
            sessions.remove(&oldest);
        }
    }
    sessions.insert(token, session);
}

/// Interaction details shared by buttons, selects and modals.
struct Actor<'a> {
    id: serenity::all::InteractionId,
    token: &'a str,
    custom_id: &'a str,
    user: &'a User,
    guild_id: GuildId,
}

impl Actor<'_> {
    async fn respond(&self, ctx: &Context, response: CreateInteractionResponse) -> anyhow::Result<()> {
        ctx.http.create_interaction_response(self.id, self.token, &response, vec![]).await?;
        Ok(())
    }
    async fn reply(&self, ctx: &Context, content: impl Into<String>) -> anyhow::Result<()> {
        self.respond(ctx, ephemeral(content)).await
    }
    async fn update(&self, ctx: &Context, view: View) -> anyhow::Result<()> {
        self.respond(ctx, view.update()).await
    }
    async fn edit(&self, ctx: &Context, view: View) -> anyhow::Result<()> {
        ctx.http.edit_original_interaction_response(self.token, &view.edit(), vec![]).await?;
        Ok(())
    }
}

pub struct AdminUi {
    repo: Arc<GaiaRepository>,
    mutes: Arc<MuteService>,
    rooms: Arc<VoiceRoomService>,
    verification: Arc<VerificationService>,
    backups: Arc<BackupService>,
    uptime: Box<dyn Fn() -> String + Send + Sync>,
    audit: Option<Arc<AuditLogService>>,
    deployments: Mutex<HashMap<String, DeploymentSession>>,
}

impl AdminUi {
    pub fn new(
        repo: Arc<GaiaRepository>,
        mutes: Arc<MuteService>,
        rooms: Arc<VoiceRoomService>,
        verification: Arc<VerificationService>,
        backups: Arc<BackupService>,
        uptime: impl Fn() -> String + Send + Sync + 'static,
        audit: Option<Arc<AuditLogService>>,
    ) -> Self {
        AdminUi { repo, mutes, rooms, verification, backups, uptime: Box::new(uptime), audit, deployments: Mutex::default() }
    }

    fn audit_configuration(&self, ctx: &Context, a: &Actor<'_>, title: &str, details: Vec<(&str, String)>) {
        let Some(audit) = &self.audit else { return };
        audit.record(AuditEvent {
            title: title.into(),
            category: "Admin Configuration".into(),
            color: "warning".into(),
            guild: Some(AuditSubject::named(a.guild_id.to_string(), guild_name(ctx, a.guild_id))),
            actor: Some(AuditSubject::named(a.user.id.to_string(), tag(a.user))),
            details: details.into_iter().map(|(k, v)| (k.to_string(), v.into())).collect(),
            ..Default::default()
        });
    }

    /// `/admin`
    pub async fn open(&self, ctx: &Context, i: &CommandInteraction) -> anyhow::Result<()> {
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let a = Actor { id: i.id, token: &i.token, custom_id: "", user: &i.user, guild_id };
        if !self.admin(ctx, &a).await? {
            return Ok(());
        }
        let view = self.dashboard(ctx, guild_id)?;
        a.respond(ctx, CreateInteractionResponse::Message(view.message().ephemeral(true))).await
    }

    /// Guard shared by every component: in a guild, still Administrator, control belongs to this guild.
    async fn guard<'a>(&self, ctx: &Context, i: &'a ComponentInteraction) -> anyhow::Result<Option<Actor<'a>>> {
        let Some(guild_id) = i.guild_id else { return Ok(None) };
        let a = Actor { id: i.id, token: &i.token, custom_id: &i.data.custom_id, user: &i.user, guild_id };
        if !self.admin(ctx, &a).await? {
            return Ok(None);
        }
        if parts(a.custom_id).2 != guild_id.to_string() {
            a.reply(ctx, "That control belongs to a different server.").await?;
            return Ok(None);
        }
        Ok(Some(a))
    }

    pub async fn handle(&self, ctx: &Context, i: &ComponentInteraction) -> anyhow::Result<()> {
        let Some(a) = self.guard(ctx, i).await? else { return Ok(()) };
        let (kind, action, _) = parts(a.custom_id);
        let g = a.guild_id;
        let gs = g.to_string();
        match kind {
            "nav" => return a.update(ctx, self.view(ctx, g, action).await?).await,
            "verify" => {
                let config = self.repo.verification(&gs)?;
                let enable = config.enabled == 0;
                let result = self.verification.set_enabled(g, enable, &self.rooms).await?;
                if result.changed {
                    self.audit_configuration(ctx, &a, &format!("Verification {}", if enable { "Enabled" } else { "Disabled" }), vec![]);
                }
                return a.update(ctx, self.view(ctx, g, "verification").await?.content(result.error)).await;
            }
            "backup" => match action {
                "toggle" => {
                    let settings = self.repo.backup_settings(&gs)?;
                    self.repo.update_backup_settings(&gs, &BackupSettingsUpdate {
                        automatic_enabled: Some(if settings.automatic_enabled != 0 { 0 } else { 1 }),
                        ..Default::default()
                    })?;
                    self.audit_configuration(
                        ctx,
                        &a,
                        &format!("Automatic Backups {}", if settings.automatic_enabled != 0 { "Disabled" } else { "Enabled" }),
                        vec![],
                    );
                    return a.update(ctx, self.view(ctx, g, "backups").await?).await;
                }
                "create" => {
                    return a
                        .update(
                            ctx,
                            View::text(
                                "Create a configuration backup for this guild? Existing backups are retained until the new snapshot is stored successfully.",
                                vec![row(vec![
                                    button(format!("adminui:backupcreate:confirm:{g}"), "Create Backup", ButtonStyle::Success),
                                    nav_styled("backups", g, "Cancel", ButtonStyle::Secondary),
                                ])],
                            ),
                        )
                        .await
                }
                "view" | "restore" => return a.update(ctx, self.backup_picker(g, action, a.user.id)?).await,
                _ => {}
            },
            "backupcreate" if action == "confirm" => return self.create_backup(ctx, &a).await,
            "backupdelete" if action.starts_with("confirm-") => {
                let backup_id = &action[8..];
                return a
                    .update(
                        ctx,
                        View::text(
                            "Delete this backup permanently? This cannot be undone.",
                            vec![row(vec![
                                button(format!("adminui:backupdelete:delete-{backup_id}:{g}"), "Confirm Delete", ButtonStyle::Danger),
                                nav_styled("backups", g, "Cancel", ButtonStyle::Secondary),
                            ])],
                        ),
                    )
                    .await;
            }
            "backupdelete" if action.starts_with("delete-") => {
                let deleted = self.backups.delete(&action[7..], &a.user.id.to_string()).await?;
                let notice = if deleted { "🟢 Backup deleted." } else { "That backup no longer exists." };
                return a.update(ctx, self.view(ctx, g, "backups").await?.content(Some(notice.into()))).await;
            }
            "backuprestore" if action.starts_with("confirm-") => return self.restore_backup(ctx, &a, &action[8..]).await,
            "verifyrole" if action == "remove" => {
                let config = self.repo.verification(&gs)?;
                self.repo.update_verification(&gs, &VerificationUpdate { role_id: Some(None), ..Default::default() })?;
                self.rooms.reconcile_guild_access(g, config.role_id.as_deref()).await?;
                return a
                    .update(
                        ctx,
                        self.view(ctx, g, "verification").await?.content(Some(
                            "🟡 Verification role removed. Verification will not restrict room access until a valid role is configured.".into(),
                        )),
                    )
                    .await;
            }
            "verifymessage" if action == "edit" => {
                return a.respond(ctx, CreateInteractionResponse::Modal(self.message_modal(g)?)).await
            }
            "verifycolor" if action == "show" => return a.update(ctx, color_picker(g)).await,
            "verifycolor" if action == "custom" => return a.respond(ctx, CreateInteractionResponse::Modal(color_modal(g))).await,
            "verifypreview" => {
                let config = self.repo.verification(&gs)?;
                let panel = self.verification.panel(&config, true);
                let message = CreateInteractionResponseMessage::new()
                    .content("Preview — this button is disabled and cannot verify anyone.")
                    .embeds(panel.embeds)
                    .components(panel.components)
                    .ephemeral(true);
                return a.respond(ctx, CreateInteractionResponse::Message(message)).await;
            }
            "verifysend" if action == "choose" => return a.update(ctx, channel_picker(g)).await,
            "verifydeploy" => return self.deploy(ctx, &a, action).await,
            "verifyupdate" if action == "edit" => return self.update_deployed(ctx, &a).await,
            "toggle" => {
                if !matches!(action, "autorole_enabled" | "voice_rooms_enabled" | "moderation_enabled") {
                    anyhow::bail!("unknown admin toggle {action}");
                }
                let enabled = self.repo.toggle(&gs, action)?;
                let label = match action {
                    "autorole_enabled" => "AutoRole",
                    "voice_rooms_enabled" => "Voice Rooms",
                    _ => "Moderation",
                };
                self.audit_configuration(ctx, &a, &format!("{label} {}", if enabled { "Enabled" } else { "Disabled" }), vec![]);
                let mut notice = None;
                if action == "autorole_enabled"
                    && enabled
                    && self.repo.autoroles(&gs)?.user_role_id.is_some()
                    && self.verification.disable_for_user_autorole(g, &self.rooms).await?
                {
                    notice = Some(AUTOROLE_CONFLICT.to_string());
                }
                if action == "voice_rooms_enabled" && !enabled {
                    self.rooms.cleanup_guild(g).await?;
                }
                self.rooms.refresh_guild_config(&gs).await?;
                return a.update(ctx, self.view(ctx, g, section_for(action)).await?.content(notice)).await;
            }
            "remove" => {
                if action == "user" || action == "bot" {
                    self.repo.set_autorole(&gs, if action == "user" { "user_role_id" } else { "bot_role_id" }, None)?;
                    let who = if action == "user" { "User" } else { "Bot" };
                    self.audit_configuration(ctx, &a, &format!("{who} AutoRole Changed"), vec![("After", "None".into())]);
                }
                if action == "join" {
                    self.repo.set_join_channel(&gs, None)?;
                    self.rooms.refresh_guild_config(&gs).await?;
                    self.audit_configuration(ctx, &a, "Join Channel Changed", vec![("After", "None".into())]);
                }
                if action == "ticket" {
                    self.mutes.configure_ticket_category(g, None).await?;
                    self.audit_configuration(ctx, &a, "Ticket Exception Changed", vec![("After", "None".into())]);
                }
                let section = match action {
                    "join" => "voice",
                    "ticket" => "moderation",
                    _ => "autorole",
                };
                return a.update(ctx, self.view(ctx, g, section).await?).await;
            }
            "pick" => return a.update(ctx, self.picker(ctx, g, action)?).await,
            "rolemgmt" if action == "add" || action == "remove" => {
                return a
                    .update(
                        ctx,
                        View::text(
                            format!("Select the role to {action}."),
                            vec![role_select(format!("adminui:rmrole:{action}:{g}")), row(vec![nav("roles", g, "← Back")])],
                        ),
                    )
                    .await
            }
            "rmtarget" | "rmapply" => return self.role_target(ctx, &a, kind, action).await,
            _ => {}
        }
        Ok(())
    }

    async fn create_backup(&self, ctx: &Context, a: &Actor<'_>) -> anyhow::Result<()> {
        a.respond(ctx, CreateInteractionResponse::Acknowledge).await?;
        let backup = match self.backups.create(a.guild_id, Some(&a.user.id.to_string()), "manual", None).await {
            Ok(backup) => backup,
            Err(error) => {
                if error.downcast_ref::<BackupAlreadyRunningError>().is_some() {
                    let embed = CreateEmbed::new()
                        .color(0xfee75c)
                        .title("Backup Already Running")
                        .description("A backup of this server is currently being created.");
                    return a.edit(ctx, View::embed(embed, vec![])).await;
                }
                if error.downcast_ref::<MemberSynchronizationError>().is_some_and(|e| e.rate_limited) {
                    let embed = CreateEmbed::new().color(0xed4245).title("Backup Failed").description(
                        "Discord temporarily rate-limited member synchronization.\nNo backup was created.\nPlease try again shortly.",
                    );
                    return a.edit(ctx, View::embed(embed, vec![])).await;
                }
                return Err(error);
            }
        };
        let content = format!(
            "🟢 Backup Created\n\n{} • {}\nRoles: {} • Channels: {} • Members: {}",
            backup.source_guild_name,
            ljubljana(&backup.created_at),
            backup.role_count,
            backup.channel_count,
            backup.member_count
        );
        a.edit(ctx, self.view(ctx, a.guild_id, "backups").await?.content(Some(content))).await
    }

    async fn restore_backup(&self, ctx: &Context, a: &Actor<'_>, backup_id: &str) -> anyhow::Result<()> {
        a.respond(ctx, CreateInteractionResponse::Acknowledge).await?;
        let g = a.guild_id;
        let context = || {
            ctx! {"service" => "backup-restore-ui", "guildId" => g.to_string(), "interactionId" => a.id.to_string(), "customId" => a.custom_id}
        };
        // (last update, progress message still usable)
        let state = Arc::new(tokio::sync::Mutex::new((None::<std::time::Instant>, true)));
        let progress: RestoreProgress = {
            let state = state.clone();
            let http = ctx.http.clone();
            let token = a.token.to_string();
            let log_context = context();
            Arc::new(move |phase: String, completed: Option<i64>, total: Option<i64>| {
                let state = state.clone();
                let http = http.clone();
                let token = token.clone();
                let log_context = log_context.clone();
                Box::pin(async move {
                    let mut state = state.lock().await;
                    if !state.1 {
                        return;
                    }
                    let now = std::time::Instant::now();
                    if state.0.is_some_and(|last| now.duration_since(last).as_millis() < 1_500) && completed != total {
                        return;
                    }
                    state.0 = Some(now);
                    let counts = match (completed, total) {
                        (Some(c), Some(t)) => format!(" — {c} / {t}"),
                        _ => String::new(),
                    };
                    let edit = EditInteractionResponse::new()
                        .content(format!(
                            "Full Server Restore Running\n\n🟢 {phase}{counts}\n\nProgress is persisted in Gaia's restore operation."
                        ))
                        .embeds(vec![])
                        .components(vec![]);
                    if let Err(error) = http.edit_original_interaction_response(&token, &edit, vec![]).await {
                        state.1 = false;
                        log_discord_error("backup.restore.progress.editReply", &error.into(), log_context);
                    }
                    // The next phase can remove the channel holding this component's message. Do not use it after announcing cleanup.
                    if phase == "Destructive cleanup begins" {
                        state.1 = false;
                    }
                })
            })
        };
        let back = || vec![row(vec![nav("backups", g, "← Back")])];
        match self.backups.restore(backup_id, g, &a.user.id.to_string(), Some(progress)).await {
            Ok(result) => {
                if !state.lock().await.1 {
                    return Ok(());
                }
                let headline = if result.status == "failed_partial" {
                    "🔴 Restore Partially Failed"
                } else if !result.warnings.is_empty() {
                    "🟡 Restore Completed With Warnings"
                } else {
                    "🟢 Restore Completed"
                };
                let list = |title: &str, items: &[String]| {
                    if items.is_empty() {
                        return String::new();
                    }
                    let bullets: Vec<String> = items.iter().take(8).map(|item| format!("• {item}")).collect();
                    format!("\n\n{title}: {}\n{}", items.len(), bullets.join("\n"))
                };
                let content = format!(
                    "{headline}\n\nSafety backup: {}\nRoles: {}\nChannels: {}\nPermission overwrites: {}\nMember roles: {}{}{}",
                    result.safety_backup_id.as_deref().unwrap_or("not created"),
                    result.roles,
                    result.channels,
                    result.overwrites,
                    result.members,
                    list("Critical failures", &result.critical_failures),
                    list("Warnings / unsupported", &result.warnings),
                );
                if let Err(error) = a.edit(ctx, View::text(content, back())).await {
                    log_discord_error("backup.restore.final.editReply", &error, context());
                }
            }
            Err(error) => {
                if !state.lock().await.1 {
                    return Ok(());
                }
                let content = format!("🔴 Restore Aborted Before Destructive Changes\n\n{error}");
                if let Err(reply_error) = a.edit(ctx, View::text(content, back())).await {
                    log_discord_error("backup.restore.abort.editReply", &reply_error, context());
                }
            }
        }
        Ok(())
    }

    async fn role_target(&self, ctx: &Context, a: &Actor<'_>, kind: &str, action: &str) -> anyhow::Result<()> {
        let mut split = action.split('-');
        let (mode, role_id, target) = (split.next().unwrap_or(""), split.next().unwrap_or(""), split.next().unwrap_or(""));
        let Some(role) = self.managed_role(ctx, a, mode, role_id).await? else { return Ok(()) };
        let g = a.guild_id;
        let (verb, preposition) = if mode == "add" { ("Add", "to") } else { ("Remove", "from") };
        if kind == "rmtarget" && target == "user" {
            return a
                .update(
                    ctx,
                    View::text(
                        format!("Select the member to {mode} <@&{}> {preposition}.", role.id),
                        vec![
                            user_select(format!("adminui:rmuser:{mode}-{}:{g}", role.id)),
                            row(vec![nav_styled("roles", g, "Cancel", ButtonStyle::Secondary)]),
                        ],
                    ),
                )
                .await;
        }
        if target != "everyone" && target != "bots" {
            return Ok(());
        }
        let group = if target == "bots" { "bot" } else { "member" };
        if kind == "rmtarget" {
            let count = role_change_targets(all_members(ctx, g).await?, mode, role.id, target).len();
            let content = if count > 0 {
                format!(
                    "{verb} <@&{}> {preposition} **{count}** {group}{}? This cannot be undone automatically.",
                    role.id,
                    if count == 1 { "" } else { "s" }
                )
            } else {
                format!("No {group}s need this change.")
            };
            return a
                .update(
                    ctx,
                    View::text(
                        content,
                        vec![row(vec![
                            button(
                                format!("adminui:rmapply:{action}:{g}"),
                                &format!("Confirm {verb}"),
                                if mode == "add" { ButtonStyle::Success } else { ButtonStyle::Danger },
                            )
                            .disabled(count == 0),
                            nav_styled("roles", g, "Cancel", ButtonStyle::Secondary),
                        ])],
                    ),
                )
                .await;
        }
        a.respond(ctx, CreateInteractionResponse::Acknowledge).await?;
        // Clearing the components first stops a second click from starting a parallel run.
        a.edit(ctx, View::text(format!("🟡 {} <@&{}>…", if verb == "Add" { "Adding" } else { "Removing" }, role.id), vec![]))
            .await?;
        let members = role_change_targets(all_members(ctx, g).await?, mode, role.id, target);
        let (changed, failed) =
            self.apply_role(ctx, a, &role, mode, &members, if target == "bots" { "All bots" } else { "Everyone" }).await;
        // shortcut: one final edit; very large servers can outlive the 15-minute interaction token, the audit record still lands.
        let edit = EditInteractionResponse::new()
            .content(role_result(verb, role.id, preposition, group, changed, &failed))
            .components(vec![row(vec![nav("roles", g, "← Back")])]);
        if let Err(error) = ctx.http.edit_original_interaction_response(a.token, &edit, vec![]).await {
            log_discord_error(
                "roles.bulk.editReply",
                &error.into(),
                ctx! {"service" => "role-management", "guildId" => g.to_string(), "interactionId" => a.id.to_string(), "customId" => a.custom_id},
            );
        }
        Ok(())
    }

    pub async fn handle_select(&self, ctx: &Context, i: &ComponentInteraction) -> anyhow::Result<()> {
        let Some(a) = self.guard(ctx, i).await? else { return Ok(()) };
        let (kind, action, _) = parts(a.custom_id);
        let g = a.guild_id;
        let gs = g.to_string();
        let id = first_value(i);
        match kind {
            "rmrole" => {
                let Some(role) = self.managed_role(ctx, &a, action, &id).await? else { return Ok(()) };
                let target = |key: &str, label: &str| {
                    button(format!("adminui:rmtarget:{action}-{}-{key}:{g}", role.id), label, ButtonStyle::Primary)
                };
                let (verb, preposition) = if action == "add" { ("Add", "to") } else { ("Remove", "from") };
                return a
                    .update(
                        ctx,
                        View::text(
                            format!("{verb} <@&{}> {preposition} whom?", role.id),
                            vec![row(vec![
                                target("user", "Specific User"),
                                target("everyone", "Everyone"),
                                target("bots", "All Bots"),
                                nav_styled("roles", g, "Cancel", ButtonStyle::Secondary),
                            ])],
                        ),
                    )
                    .await;
            }
            "rmuser" => {
                let mut split = action.split('-');
                let (mode, role_id) = (split.next().unwrap_or(""), split.next().unwrap_or(""));
                let Some(role) = self.managed_role(ctx, &a, mode, role_id).await? else { return Ok(()) };
                let member = match snowflake(&id) {
                    Some(user) => g.member(ctx, UserId::new(user)).await.ok(),
                    None => None,
                };
                let Some(member) = member else {
                    return a.reply(ctx, "That user is not a member of this server.").await;
                };
                let (verb, preposition) = if mode == "add" { ("Add", "to") } else { ("Remove", "from") };
                let back = || vec![row(vec![nav("roles", g, "← Back")])];
                if role_change_targets([member.clone()], mode, role.id, "user").is_empty() {
                    let content = format!(
                        "🟡 <@{}> {} <@&{}>.",
                        member.user.id,
                        if mode == "add" { "already has" } else { "does not have" },
                        role.id
                    );
                    return a.update(ctx, View::text(content, back())).await;
                }
                let (changed, failed) =
                    self.apply_role(ctx, &a, &role, mode, std::slice::from_ref(&member), &format!("<@{}>", member.user.id)).await;
                return a.update(ctx, View::text(role_result(verb, role.id, preposition, "member", changed, &failed), back())).await;
            }
            "backupselect" => {
                return match self.backup_preview(ctx, &a, action, &id).await {
                    Ok(view) => a.update(ctx, view).await,
                    Err(error) => a.reply(ctx, error.to_string()).await,
                };
            }
            "role" => {
                // Same hierarchy rules as role management: Gaia assigns this role on the admin's behalf.
                if self.managed_role(ctx, &a, "add", &id).await?.is_none() {
                    return Ok(());
                }
                if action == "verification" {
                    let old = self.repo.verification(&gs)?;
                    self.repo.update_verification(&gs, &VerificationUpdate { role_id: Some(Some(id.clone())), ..Default::default() })?;
                    self.rooms.reconcile_guild_access(g, old.role_id.as_deref()).await?;
                    self.audit_configuration(
                        ctx,
                        &a,
                        "Verification Role Changed",
                        vec![("Before", old.role_id.unwrap_or_else(|| "None".into())), ("After", id.clone())],
                    );
                } else {
                    self.repo.set_autorole(&gs, if action == "user" { "user_role_id" } else { "bot_role_id" }, Some(&id))?;
                    let who = if action == "user" { "User" } else { "Bot" };
                    self.audit_configuration(ctx, &a, &format!("{who} AutoRole Changed"), vec![("After", id.clone())]);
                    if action == "user"
                        && self.repo.settings(&gs)?.autorole_enabled != 0
                        && self.verification.disable_for_user_autorole(g, &self.rooms).await?
                    {
                        return a
                            .update(ctx, self.view(ctx, g, "autorole").await?.content(Some(AUTOROLE_CONFLICT.into())))
                            .await;
                    }
                }
            }
            "color" => {
                if id == "custom" {
                    return a.respond(ctx, CreateInteractionResponse::Modal(color_modal(g))).await;
                }
                let Some(parsed) = normalize_verification_color(&id) else {
                    return a.reply(ctx, "That color is invalid.").await;
                };
                self.repo.update_verification(&gs, &VerificationUpdate { color: Some(parsed), ..Default::default() })?;
            }
            "verifychannel" => {
                let channel = fetch_guild_channel(ctx, g, &id).await;
                let config = self.repo.verification(&gs)?;
                if let Some(error) = self.verification.configuration_error(g, &config, channel.as_ref()) {
                    return a.reply(ctx, format!("🔴 Cannot deploy verification message: {error}")).await;
                }
                let Some(channel) = channel else { return Ok(()) };
                let token = deployment_token();
                remember_deployment(
                    &mut self.deployments.lock().unwrap(),
                    token.clone(),
                    DeploymentSession { guild_id: g, user_id: a.user.id, channel_id: channel.id, created: Instant::now() },
                );
                let panel = self.verification.panel(&config, true);
                let mut components = panel.components;
                components.push(row(vec![
                    button(format!("adminui:verifydeploy:{token}:{g}"), "Send", ButtonStyle::Success),
                    nav_styled("verification", g, "Cancel", ButtonStyle::Secondary),
                ]));
                let view = View {
                    content: Some(format!(
                        "Ready to send the verification message to <#{}>. Review the preview, then confirm.",
                        channel.id
                    )),
                    embeds: panel.embeds,
                    components,
                };
                return a.update(ctx, view).await;
            }
            "staff" => {
                if action == "add" {
                    self.repo.add_staff(&gs, &id, &a.user.id.to_string())?;
                } else {
                    self.repo.remove_staff(&gs, &id)?;
                }
                let title = if action == "add" { "Staff Added" } else { "Staff Removed" };
                self.audit_configuration(ctx, &a, title, vec![("Member", id.clone())]);
            }
            "channel" => {
                if action == "join" {
                    self.repo.set_join_channel(&gs, Some(&id))?;
                    self.rooms.refresh_guild_config(&gs).await?;
                    self.audit_configuration(ctx, &a, "Join Channel Changed", vec![("After", id.clone())]);
                } else {
                    self.mutes.configure_ticket_category(g, Some(id.clone())).await?;
                    self.audit_configuration(ctx, &a, "Ticket Exception Changed", vec![("After", id.clone())]);
                }
            }
            _ => {}
        }
        let section = match action {
            "join" => "voice",
            "ticket" => "moderation",
            "verification" | "set" => "verification",
            "user" | "bot" => "autorole",
            _ => "staff",
        };
        a.update(ctx, self.view(ctx, g, section).await?).await
    }

    async fn backup_preview(&self, ctx: &Context, a: &Actor<'_>, action: &str, backup_id: &str) -> anyhow::Result<View> {
        let g = a.guild_id;
        let preview = self.backups.preview(backup_id, g, &a.user.id.to_string()).await?;
        let backup = &preview.backup;
        if action == "view" {
            let guild = &preview.snapshot.guild;
            let icon = match guild["assets"]["icon"]["state"].as_str() {
                Some("captured") => "Captured",
                Some("absent") => "None",
                _ => "Legacy backup",
            };
            let description = format!(
                "**{}** • {}\n{}\n\n**Identity**\nName: Captured\nDescription: {}\nIcon: {icon}\nCommunity: {}\n\n**Structure**\nRoles: {}\nChannels: {}\nMember assignments: {}\nPermission overwrites: {}\nStorage: {:.2} MB\nSchema: v{}{}",
                backup.source_guild_name,
                ljubljana(&backup.created_at),
                backup.creation_type,
                if guild.get("description").is_some() { "Captured" } else { "Legacy backup" },
                if guild["communityEnabled"].as_bool() == Some(true) { "Enabled" } else { "Disabled" },
                backup.role_count,
                backup.channel_count,
                backup.member_count,
                backup.overwrite_count,
                backup.snapshot_size as f64 / 1024.0 / 1024.0,
                backup.snapshot_schema_version,
                if preview.unsupported.is_empty() { String::new() } else { format!("\nUnsupported: {}", preview.unsupported.len()) },
            );
            return Ok(View {
                content: Some(String::new()),
                embeds: vec![CreateEmbed::new().color(COLOR).title("Guild Backup").description(description)],
                components: vec![row(vec![
                    button(format!("adminui:backupdelete:confirm-{}:{g}", backup.backup_id), "Delete Backup", ButtonStyle::Danger),
                    nav("backups", g, "← Back"),
                ])],
            });
        }
        let description = format!(
            "⚠️ **Destructive replacement**\n\nSource: **{}**\nDestination: **{}**\n\nGaia will delete destination channels and non-managed roles it can manage, then reconstruct the backup. @everyone, managed/integration roles, Gaia's bot role, Discord-protected objects, and active temporary Gaia rooms are preserved. Existing member role assignments are replaced where Gaia can manage them.\n\nRoles to recreate: {}\nChannels to recreate: {}\nPermission overwrites: {}\nMember assignments to evaluate: {}{}\n\nA fresh destination safety backup is captured, committed, hashed and revalidated before any deletion. Partial Discord failures cannot be rolled back automatically.",
            backup.source_guild_name,
            guild_name(ctx, g),
            preview.roles_to_create,
            preview.channels_to_create,
            preview.overwrites,
            preview.member_assignments,
            if preview.unsupported.is_empty() {
                String::new()
            } else {
                format!("\nUnsupported in snapshot: {}", preview.unsupported.join(" "))
            },
        );
        Ok(View {
            content: Some(String::new()),
            embeds: vec![CreateEmbed::new().color(0xed4245).title("Full Server Restore").description(description)],
            components: vec![row(vec![
                button(
                    format!("adminui:backuprestore:confirm-{}:{g}", backup.backup_id),
                    "I Understand — Start Full Restore",
                    ButtonStyle::Danger,
                ),
                nav_styled("backups", g, "Cancel", ButtonStyle::Secondary),
            ])],
        })
    }

    pub async fn handle_modal(&self, ctx: &Context, i: &ModalInteraction) -> anyhow::Result<()> {
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let a = Actor { id: i.id, token: &i.token, custom_id: &i.data.custom_id, user: &i.user, guild_id };
        if !self.admin(ctx, &a).await? {
            return Ok(());
        }
        let (kind, action, guild) = parts(a.custom_id);
        if kind != "modal" || guild != guild_id.to_string() {
            return Ok(());
        }
        let gs = guild_id.to_string();
        if action == "message" {
            let title = modal_value(i, "title").trim().to_string();
            let description = modal_value(i, "description").trim().to_string();
            let button = modal_value(i, "button").trim().to_string();
            if title.is_empty()
                || utf16_len(&title) > 256
                || description.is_empty()
                || utf16_len(&description) > 4096
                || button.is_empty()
                || utf16_len(&button) > 80
            {
                return a
                    .reply(
                        ctx,
                        "Use a title up to 256 characters, description up to 4096 characters, and button text up to 80 characters.",
                    )
                    .await;
            }
            self.repo.update_verification(
                &gs,
                &VerificationUpdate { title: Some(title), description: Some(description), button_text: Some(button), ..Default::default() },
            )?;
            let mut view = self.view(ctx, guild_id, "verification").await?;
            view.content = view.content.or(Some("🟢 Verification message configuration saved.".into()));
            return a.update(ctx, view).await;
        }
        if action == "color" {
            let Some(parsed) = normalize_verification_color(&modal_value(i, "color")) else {
                return a.reply(ctx, "Enter a six-digit hexadecimal color, for example `#57F287`.").await;
            };
            self.repo.update_verification(&gs, &VerificationUpdate { color: Some(parsed), ..Default::default() })?;
            let mut view = self.view(ctx, guild_id, "verification").await?;
            view.content = view.content.or(Some("🟢 Verification color saved.".into()));
            return a.update(ctx, view).await;
        }
        Ok(())
    }

    async fn deploy(&self, ctx: &Context, a: &Actor<'_>, token: &str) -> anyhow::Result<()> {
        let session = self
            .deployments
            .lock()
            .unwrap()
            .get(token)
            .filter(|s| s.guild_id == a.guild_id && s.user_id == a.user.id && s.created.elapsed() < DEPLOYMENT_TTL)
            .map(|s| s.channel_id);
        let Some(channel_id) = session else {
            return a.reply(ctx, "That deployment confirmation expired. Start again from Verification.").await;
        };
        let gs = a.guild_id.to_string();
        let channel = fetch_guild_channel(ctx, a.guild_id, &channel_id.to_string()).await;
        let config = self.repo.verification(&gs)?;
        let error = self.verification.configuration_error(a.guild_id, &config, channel.as_ref());
        let channel = match (error, channel) {
            (None, Some(channel)) if channel.is_text_based() => channel,
            (error, _) => {
                let reason = error.unwrap_or_else(|| "Choose a text channel.".into());
                return a.reply(ctx, format!("🔴 Cannot deploy verification message: {reason}")).await;
            }
        };
        let panel = self.verification.panel(&config, false);
        let message = serenity::all::CreateMessage::new().embeds(panel.embeds).components(panel.components);
        let Ok(message) = channel.id.send_message(&ctx.http, message).await else {
            return a.reply(ctx, "Gaia could not send the verification message. Check its channel permissions.").await;
        };
        self.repo.update_verification(
            &gs,
            &VerificationUpdate {
                channel_id: Some(Some(channel.id.to_string())),
                message_id: Some(Some(message.id.to_string())),
                ..Default::default()
            },
        )?;
        self.audit_configuration(
            ctx,
            a,
            "Verification Message Deployed",
            vec![("Channel", channel.id.to_string()), ("Message", message.id.to_string())],
        );
        self.deployments.lock().unwrap().remove(token);
        let content = format!("🟢 Verification message deployed in <#{}>.", channel.id);
        a.update(ctx, self.view(ctx, a.guild_id, "verification").await?.content(Some(content))).await
    }

    async fn update_deployed(&self, ctx: &Context, a: &Actor<'_>) -> anyhow::Result<()> {
        let gs = a.guild_id.to_string();
        let config = self.repo.verification(&gs)?;
        let Some((channel, message)) = self.deployed_message(ctx, a.guild_id, &config).await? else {
            return a.update(ctx, self.view(ctx, a.guild_id, "verification").await?.content(Some(DEPLOYMENT_GONE.into()))).await;
        };
        if let Some(error) = self.verification.configuration_error(a.guild_id, &config, Some(&channel)) {
            return a.reply(ctx, format!("🔴 Cannot update verification message: {error}")).await;
        }
        let panel = self.verification.panel(&config, false);
        let edit = serenity::all::EditMessage::new().embeds(panel.embeds).components(panel.components);
        if let Err(error) = channel.id.edit_message(&ctx.http, message.id, edit).await {
            log_discord_error(
                "verification.deployment.edit",
                &error.into(),
                ctx! {"service" => "verification", "guildId" => gs.clone(), "channelId" => channel.id.to_string(), "messageId" => message.id.to_string(), "interactionId" => a.id.to_string(), "customId" => a.custom_id},
            );
            self.repo.update_verification(&gs, &VerificationUpdate { channel_id: Some(None), message_id: Some(None), ..Default::default() })?;
            return a.update(ctx, self.view(ctx, a.guild_id, "verification").await?.content(Some(DEPLOYMENT_GONE.into()))).await;
        }
        a.update(ctx, self.view(ctx, a.guild_id, "verification").await?.content(Some("🟢 Verification message updated.".into())))
            .await
    }

    async fn deployed_message(
        &self,
        ctx: &Context,
        guild_id: GuildId,
        config: &VerificationConfig,
    ) -> anyhow::Result<Option<(GuildChannel, Message)>> {
        let (Some(channel_id), Some(message_id)) = (config.channel_id.as_deref(), config.message_id.as_deref()) else {
            return Ok(None);
        };
        let log = |operation: &str, error: serenity::Error| {
            log_discord_error(
                operation,
                &error.into(),
                ctx! {"service" => "verification", "guildId" => guild_id.to_string(), "channelId" => channel_id, "messageId" => message_id},
            )
        };
        let channel = match snowflake(channel_id) {
            Some(id) => match ChannelId::new(id).to_channel(ctx).await {
                Ok(channel) => channel.guild().filter(|c| c.guild_id == guild_id),
                Err(error) => {
                    log("verification.deployment.channel.fetch", error);
                    None
                }
            },
            None => None,
        };
        let mut found = None;
        if let (Some(channel), Some(id)) = (channel.filter(|c| c.is_text_based()), snowflake(message_id)) {
            match channel.id.message(ctx, MessageId::new(id)).await {
                Ok(message) => found = Some((channel, message)),
                Err(error) => log("verification.deployment.message.fetch", error),
            }
        }
        let bot = ctx.cache.current_user().id;
        match found {
            Some((channel, message)) if message.author.id == bot => Ok(Some((channel, message))),
            _ => {
                self.repo.update_verification(
                    &guild_id.to_string(),
                    &VerificationUpdate { channel_id: Some(None), message_id: Some(None), ..Default::default() },
                )?;
                Ok(None)
            }
        }
    }

    async fn deployment_text(&self, ctx: &Context, guild_id: GuildId, config: &VerificationConfig) -> anyhow::Result<String> {
        Ok(if self.deployed_message(ctx, guild_id, config).await?.is_some() {
            format!("🟢 Deployed in <#{}>", config.channel_id.as_deref().unwrap_or_default())
        } else {
            "🟡 Not deployed".into()
        })
    }

    async fn admin(&self, ctx: &Context, a: &Actor<'_>) -> anyhow::Result<bool> {
        let member = a.guild_id.member(ctx, a.user.id).await.ok();
        let allowed = match &member {
            Some(member) => has_permission(&self.repo, &Discord { http: ctx.http.clone(), cache: ctx.cache.clone() }, member, PermissionLevel::Administrator).await?,
            None => false,
        };
        if !allowed {
            a.reply(ctx, "You need Discord Administrator permission to use Gaia Administration.").await?;
        }
        Ok(allowed)
    }

    fn dashboard(&self, ctx: &Context, guild_id: GuildId) -> anyhow::Result<View> {
        let gs = guild_id.to_string();
        let s = self.repo.settings(&gs)?;
        let roles = self.repo.autoroles(&gs)?;
        let join = self.repo.join_channel(&gs)?;
        let staff = self.repo.staff_count(&gs)?;
        let verification = self.repo.verification(&gs)?;
        let icon = |v: i64| if v != 0 { "🟢" } else { "🔴" };
        let configured = |id: &Option<String>, name: &str| match id {
            Some(id) => format!("🟢 {name} — <@&{id}>"),
            None => format!("🟡 {name} — Not configured"),
        };
        let verify_status = if s.autorole_enabled != 0 && roles.user_role_id.is_some() {
            "🔴 Verification — Disabled (User AutoRole active)"
        } else if verification.enabled == 0 {
            "⚪ Verification — Disabled"
        } else if verification.role_id.is_some() {
            "🟢 Verification — configured"
        } else {
            "🟡 Verification — Missing role"
        };
        let description = format!(
            "Managing **{}**\n\n🟢 **Gaia**\nOnline • Uptime: {}\n\n**Modules**\n{} AutoRole\n{} Voice Rooms\n{} Moderation\n{verify_status}\n🟢 Backups\n\n**Configuration**\n{}\n{}\n{}\n{}\n\n**Staff**\n🟢 {staff} configured (Administrators always count)",
            guild_name(ctx, guild_id),
            (self.uptime)(),
            icon(s.autorole_enabled),
            icon(s.voice_rooms_enabled),
            icon(s.moderation_enabled),
            configured(&roles.user_role_id, "User AutoRole"),
            configured(&roles.bot_role_id, "Bot AutoRole"),
            match &join {
                Some(join) => format!("🟢 Voice Rooms — <#{join}>"),
                None => "🟡 Voice Rooms — Not configured".into(),
            },
            match &s.ticket_category_id {
                Some(id) => format!("🟢 Ticket Category — <#{id}>"),
                None => "🟡 Ticket Category — Not configured".into(),
            },
        );
        Ok(View::embed(
            CreateEmbed::new().color(COLOR).title("🌿 Gaia Administration").description(description),
            vec![
                row(vec![
                    nav("autorole", guild_id, "AutoRole"),
                    nav("voice", guild_id, "Voice Rooms"),
                    nav("staff", guild_id, "Staff"),
                    nav("moderation", guild_id, "Moderation"),
                    nav("verification", guild_id, "Verification"),
                ]),
                row(vec![
                    nav("backups", guild_id, "Backups"),
                    nav("roles", guild_id, "Remove/Add Roles"),
                    nav_styled("dashboard", guild_id, "Refresh", ButtonStyle::Secondary),
                ]),
            ],
        ))
    }

    async fn view(&self, ctx: &Context, guild_id: GuildId, section: &str) -> anyhow::Result<View> {
        let g = guild_id;
        let gs = g.to_string();
        if section == "dashboard" {
            return self.dashboard(ctx, g);
        }
        if section == "roles" {
            return Ok(View {
                content: Some(String::new()),
                embeds: vec![CreateEmbed::new().color(COLOR).title("Remove/Add Roles").description(
                    "Add or remove a role for a specific member, everyone, or all bots in this server.\n\nGaia can only manage roles below its own highest role, and you can only manage roles below yours.",
                )],
                components: vec![row(vec![
                    button(format!("adminui:rolemgmt:add:{g}"), "Add Role", ButtonStyle::Success),
                    button(format!("adminui:rolemgmt:remove:{g}"), "Remove Role", ButtonStyle::Danger),
                    nav("dashboard", g, "← Back"),
                ])],
            });
        }
        let s = self.repo.settings(&gs)?;
        let status = |v: i64| if v != 0 { "🟢 Enabled" } else { "🔴 Disabled" };
        match section {
            "autorole" => {
                let r = self.repo.autoroles(&gs)?;
                let note = if s.autorole_enabled != 0 && r.user_role_id.is_some() {
                    "\n\n🟡 User AutoRole is active. Verification cannot be enabled while it assigns a role to human members."
                } else {
                    ""
                };
                let mention = |id: &Option<String>| id.as_ref().map(|id| format!("<@&{id}>")).unwrap_or_else(|| "🟡 Not configured".into());
                let description = format!(
                    "Status: {}\n\nUser AutoRole: {}\nBot AutoRole: {}{note}",
                    status(s.autorole_enabled),
                    mention(&r.user_role_id),
                    mention(&r.bot_role_id)
                );
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("AutoRole").description(description),
                    vec![
                        row(vec![
                            toggle("autorole_enabled", g, s.autorole_enabled != 0),
                            pick("user", g, "Set User Role", false),
                            pick("bot", g, "Set Bot Role", false),
                        ]),
                        row(vec![
                            remove("user", g, "Remove User Role", r.user_role_id.is_none()),
                            remove("bot", g, "Remove Bot Role", r.bot_role_id.is_none()),
                            nav("dashboard", g, "← Back"),
                        ]),
                    ],
                ))
            }
            "voice" => {
                let join = self.repo.join_channel(&gs)?;
                let v = self.repo.verification(&gs)?;
                let access = if v.enabled != 0 {
                    match v.role_id.as_deref().filter(|id| cached_role(ctx, g, id).is_some()) {
                        Some(id) => format!("<@&{id}>\nControlled by Verification"),
                        None => "🟡 Verification role missing".into(),
                    }
                } else {
                    "Everyone\nVerification disabled".into()
                };
                let description = format!(
                    "Status: {}\n\nJoin-to-Create: {}\n\nDefault Access\n{access}",
                    status(s.voice_rooms_enabled),
                    join.as_ref().map(|j| format!("<#{j}>")).unwrap_or_else(|| "🟡 Not configured".into())
                );
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Voice Rooms").description(description),
                    vec![row(vec![
                        toggle("voice_rooms_enabled", g, s.voice_rooms_enabled != 0),
                        pick("join", g, "Set Join Channel", false),
                        remove("join", g, "Remove Join Channel", join.is_none()),
                        nav("dashboard", g, "← Back"),
                    ])],
                ))
            }
            "moderation" => {
                let now = chrono::Utc::now();
                let count =
                    self.repo.active_mutes(&gs)?.iter().filter(|m| parse_time(&m.expires_at).is_some_and(|t| t > now)).count();
                let description = format!(
                    "Status: {}\n\nTicket Exception: {}\n\nActive Mutes: {count} member{}",
                    status(s.moderation_enabled),
                    s.ticket_category_id.as_ref().map(|id| format!("<#{id}>")).unwrap_or_else(|| "🟡 Not configured".into()),
                    if count == 1 { "" } else { "s" }
                );
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Moderation").description(description),
                    vec![row(vec![
                        toggle("moderation_enabled", g, s.moderation_enabled != 0),
                        pick("ticket", g, "Set Ticket Category", false),
                        pick("mutes", g, "View Mutes", count == 0),
                        remove("ticket", g, "Remove Ticket Category", s.ticket_category_id.is_none()),
                        nav("dashboard", g, "← Back"),
                    ])],
                ))
            }
            "backups" => {
                let settings = self.repo.backup_settings(&gs)?;
                let backups = self.repo.backups(&gs)?;
                let automatic = settings.automatic_enabled != 0;
                let latest = backups
                    .first()
                    .map(|b| format!("{} • {}", b.source_guild_name, ljubljana(&b.created_at)))
                    .unwrap_or_else(|| "None".into());
                let storage = backups.iter().map(|b| b.snapshot_size as f64).sum::<f64>() / 1024.0 / 1024.0;
                let description = format!(
                    "Automatic Backups: {}\n\nStored Backups: {} / 2\nLatest Backup: {latest}\nStorage: {storage:.2} MB\n\nConfiguration snapshots include roles, supported channels, overwrites, member role assignments, and Gaia configuration. They do not include messages, DMs, integrations, managed roles, or a byte-for-byte server image.",
                    if automatic { "🟢 Enabled — daily at 03:00 Europe/Ljubljana" } else { "⚪ Disabled" },
                    backups.len()
                );
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Guild Backups").description(description),
                    vec![row(vec![
                        button(
                            format!("adminui:backup:toggle:{g}"),
                            if automatic { "Disable Automatic" } else { "Enable Automatic" },
                            if automatic { ButtonStyle::Danger } else { ButtonStyle::Success },
                        ),
                        button(format!("adminui:backup:create:{g}"), "Create Backup", ButtonStyle::Primary),
                        button(format!("adminui:backup:view:{g}"), "View Backups", ButtonStyle::Secondary).disabled(backups.is_empty()),
                        button(format!("adminui:backup:restore:{g}"), "Restore Backup", ButtonStyle::Danger),
                        nav("dashboard", g, "← Back"),
                    ])],
                ))
            }
            "verification" => {
                let v = self.repo.verification(&gs)?;
                let ar = self.repo.autoroles(&gs)?;
                let deployment = self.deployment_text(ctx, g, &v).await?;
                let conflict = s.autorole_enabled != 0 && ar.user_role_id.is_some();
                let role_state = match v.role_id.as_deref().filter(|id| cached_role(ctx, g, id).is_some()) {
                    Some(id) => format!("<@&{id}>"),
                    None => "🟡 Not configured or missing".into(),
                };
                let enabled = v.enabled != 0;
                let description = format!(
                    "Status: {}{}\n\nVerified Role: {role_state}\nVerification Message: {deployment}\n\nTitle: {}\nButton: {}",
                    if enabled { "🟢 Enabled" } else { "⚪ Disabled" },
                    if conflict { "\n🔴 Unavailable while User AutoRole is enabled." } else { "" },
                    v.title,
                    v.button_text
                );
                Ok(View::embed(
                    CreateEmbed::new().color(v.color as u32).title("Verification").description(description),
                    vec![
                        row(vec![
                            button(
                                format!("adminui:verify:toggle:{g}"),
                                if enabled { "Disable" } else { "Enable" },
                                if enabled { ButtonStyle::Danger } else { ButtonStyle::Success },
                            )
                            .disabled(conflict && !enabled),
                            pick("verification", g, "Set Role", false),
                            button(format!("adminui:verifymessage:edit:{g}"), "Edit Message", ButtonStyle::Primary),
                            button(format!("adminui:verifycolor:show:{g}"), "Set Color", ButtonStyle::Secondary),
                            button(format!("adminui:verifypreview:show:{g}"), "Preview", ButtonStyle::Secondary),
                        ]),
                        row(vec![
                            button(format!("adminui:verifysend:choose:{g}"), "Send Verification", ButtonStyle::Success),
                            button(format!("adminui:verifyupdate:edit:{g}"), "Update Message", ButtonStyle::Primary)
                                .disabled(v.channel_id.is_none() || v.message_id.is_none()),
                            button(format!("adminui:verifyrole:remove:{g}"), "Remove Role", ButtonStyle::Secondary)
                                .disabled(v.role_id.is_none()),
                            nav("dashboard", g, "← Back"),
                        ]),
                    ],
                ))
            }
            _ => {
                let names = self.repo.staff_members(&gs)?;
                let list = if names.is_empty() {
                    "None".to_string()
                } else {
                    names.iter().map(|id| format!("• <@{id}>")).collect::<Vec<_>>().join("\n")
                };
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Gaia Staff").description(format!(
                        "Gaia Staff can mute and unmute members. Discord Administrators automatically have Staff access.\n\n**Configured Staff**\n{list}"
                    )),
                    vec![row(vec![
                        pick("staffadd", g, "Add Staff", false),
                        pick("staffremove", g, "Remove Staff", names.is_empty()),
                        nav("dashboard", g, "← Back"),
                    ])],
                ))
            }
        }
    }

    fn backup_picker(&self, guild_id: GuildId, action: &str, user_id: UserId) -> anyhow::Result<View> {
        let backups = self.backups.available_to(user_id, guild_id)?;
        let back = row(vec![nav("backups", guild_id, "← Back")]);
        if backups.is_empty() {
            return Ok(View::text("No backups you are authorized to access are available.", vec![back]));
        }
        let options = backups
            .iter()
            .take(25)
            .map(|b| {
                let date = parse_time(&b.created_at).map(|t| t.format("%d/%m/%Y").to_string()).unwrap_or_else(|| "Invalid Date".into());
                CreateSelectMenuOption::new(truncate(&format!("{} — {date}", b.source_guild_name), 100), b.backup_id.clone())
                    .description(truncate(
                        &format!("{} • {} channels • {} roles", b.creation_type, b.channel_count, b.role_count),
                        100,
                    ))
            })
            .collect();
        Ok(View::text(
            if action == "restore" {
                "Select a backup for a destructive full-server restore."
            } else {
                "Select a backup to inspect."
            },
            vec![
                select(format!("adminui:backupselect:{action}:{guild_id}"), CreateSelectMenuKind::String { options }, "Select a backup"),
                back,
            ],
        ))
    }

    fn picker(&self, ctx: &Context, guild_id: GuildId, action: &str) -> anyhow::Result<View> {
        let g = guild_id;
        let gs = g.to_string();
        match action {
            "mutes" => {
                let now = chrono::Utc::now();
                let mutes: Vec<String> = self
                    .repo
                    .active_mutes(&gs)?
                    .into_iter()
                    .filter_map(|m| parse_time(&m.expires_at).filter(|t| *t > now).map(|t| (m, t)))
                    .take(10)
                    .map(|(m, t)| {
                        format!(
                            "🔇 <@{}>\nExpires: <t:{}:R>\nReason: {}",
                            m.user_id,
                            t.timestamp(),
                            m.reason.as_deref().unwrap_or("No reason provided.")
                        )
                    })
                    .collect();
                let description = if mutes.is_empty() { "No active mutes.".to_string() } else { mutes.join("\n\n") };
                Ok(View {
                    content: Some(String::new()),
                    embeds: vec![CreateEmbed::new().color(COLOR).title("Active Mutes • Page 1").description(description)],
                    components: vec![row(vec![nav("moderation", g, "← Back")])],
                })
            }
            "user" | "bot" | "verification" => {
                Ok(View::text(format!("Select the {action} role."), vec![role_select(format!("adminui:role:{action}:{g}"))]))
            }
            "join" => Ok(View::text(
                "Select the join-to-create voice channel.",
                vec![channel_select(format!("adminui:channel:join:{g}"), vec![ChannelType::Voice], "Select a voice channel")],
            )),
            "ticket" => Ok(View::text(
                "Select the ticket category.",
                vec![channel_select(format!("adminui:channel:ticket:{g}"), vec![ChannelType::Category], "Select a category")],
            )),
            "staffadd" => {
                Ok(View::text("Select a member to add to Gaia Staff.", vec![user_select(format!("adminui:staff:add:{g}"))]))
            }
            _ => {
                let staff = self.repo.staff_members(&gs)?;
                let options = staff
                    .iter()
                    .take(25)
                    .map(|id| {
                        let name = snowflake(id).and_then(|uid| {
                            ctx.cache.guild(g)?.members.get(&UserId::new(uid)).map(|m| m.display_name().to_string())
                        });
                        CreateSelectMenuOption::new(name.unwrap_or_else(|| format!("User {id}")), id.clone())
                    })
                    .collect();
                Ok(View::text(
                    "Select a configured staff member to remove.",
                    vec![select(format!("adminui:staff:remove:{g}"), CreateSelectMenuKind::String { options }, "Select staff")],
                ))
            }
        }
    }

    fn message_modal(&self, guild_id: GuildId) -> anyhow::Result<CreateModal> {
        let v = self.repo.verification(&guild_id.to_string())?;
        Ok(CreateModal::new(format!("adminui:modal:message:{guild_id}"), "Edit Verification Message").components(vec![
            CreateActionRow::InputText(text_input(InputTextStyle::Short, "Embed Title", "title", 256).value(v.title)),
            CreateActionRow::InputText(
                text_input(InputTextStyle::Paragraph, "Embed Description", "description", 4000).value(v.description),
            ),
            CreateActionRow::InputText(
                text_input(InputTextStyle::Short, "Verification Button Text", "button", 80).value(v.button_text),
            ),
        ]))
    }

    /// Rechecked on every step: the role, Gaia's hierarchy and the actor's hierarchy can change between clicks.
    async fn managed_role(&self, ctx: &Context, a: &Actor<'_>, mode: &str, role_id: &str) -> anyhow::Result<Option<Role>> {
        let g = a.guild_id;
        let role = cached_role(ctx, g, role_id);
        let owner = ctx.cache.guild(g).map(|guild| guild.owner_id);
        let is_owner = owner == Some(a.user.id);
        // Only fetch the actor when the earlier checks pass, like the TS short-circuit.
        let early = managed_role_error(
            mode,
            role.as_ref().map(role_key),
            g.get(),
            || role.as_ref().is_some_and(|r| self.verification.assignable(g, r)),
            true,
            None,
        );
        let error = match early {
            Some(error) => Some(error),
            None if is_owner => None,
            None => {
                let actor = g.member(ctx, a.user.id).await.ok();
                let highest = actor.as_ref().and_then(|m| highest_role_key(ctx, g, m));
                managed_role_error(mode, role.as_ref().map(role_key), g.get(), || true, false, highest)
            }
        };
        if let Some(error) = error {
            a.reply(ctx, error).await?;
            return Ok(None);
        }
        Ok(role)
    }

    async fn apply_role(
        &self,
        ctx: &Context,
        a: &Actor<'_>,
        role: &Role,
        mode: &str,
        members: &[Member],
        target: &str,
    ) -> (usize, Vec<UserId>) {
        let mut changed = 0;
        let mut failed = vec![];
        let reason = format!("Gaia role management by {}", tag(a.user));
        for member in members {
            let user = member.user.id;
            let result = if mode == "add" {
                ctx.http.add_member_role(a.guild_id, user, role.id, Some(&reason)).await
            } else {
                ctx.http.remove_member_role(a.guild_id, user, role.id, Some(&reason)).await
            };
            match result {
                Ok(()) => changed += 1,
                Err(error) => {
                    failed.push(user);
                    log_discord_error(
                        &format!("roles.{mode}"),
                        &error.into(),
                        ctx! {"service" => "role-management", "guildId" => a.guild_id.to_string(), "userId" => user.to_string()},
                    );
                }
            }
        }
        if let Some(audit) = &self.audit {
            audit.record(AuditEvent {
                title: if mode == "add" { "Role Added" } else { "Role Removed" }.into(),
                category: "Role Management".into(),
                color: if failed.is_empty() { "warning" } else { "danger" }.into(),
                guild: Some(AuditSubject::named(a.guild_id.to_string(), guild_name(ctx, a.guild_id))),
                actor: Some(AuditSubject::named(a.user.id.to_string(), tag(a.user))),
                details: vec![
                        ("Role".to_string(), role.name.clone().into()),
                        ("Target".to_string(), target.to_string().into()),
                        ("Changed".to_string(), (changed as i64).into()),
                        ("Failed".to_string(), (failed.len() as i64).into()),
                    ],
                ..Default::default()
            });
        }
        (changed, failed)
    }
}

fn color_picker(guild_id: GuildId) -> View {
    let options = [("Green", "57F287"), ("Blue", "5865F2"), ("Red", "ED4245"), ("Yellow", "FEE75C"), ("Purple", "9B59B6"), ("Custom…", "custom")]
        .into_iter()
        .map(|(label, value)| CreateSelectMenuOption::new(label, value))
        .collect();
    View::text(
        "Choose a verification embed color.",
        vec![
            select(format!("adminui:color:set:{guild_id}"), CreateSelectMenuKind::String { options }, "Select a color"),
            row(vec![nav("verification", guild_id, "← Back")]),
        ],
    )
}

fn color_modal(guild_id: GuildId) -> CreateModal {
    CreateModal::new(format!("adminui:modal:color:{guild_id}"), "Custom Verification Color").components(vec![
        CreateActionRow::InputText(text_input(InputTextStyle::Short, "Hex color (for example #57F287)", "color", 7)),
    ])
}

fn channel_picker(guild_id: GuildId) -> View {
    View::text(
        "Select a text channel for the verification message.",
        vec![
            channel_select(
                format!("adminui:verifychannel:choose:{guild_id}"),
                vec![ChannelType::Text, ChannelType::News],
                "Select a text channel",
            ),
            row(vec![nav("verification", guild_id, "← Back")]),
        ],
    )
}

/// `guild.channels.fetch(id).catch(() => null)`: only channels of this guild.
async fn fetch_guild_channel(ctx: &Context, guild_id: GuildId, id: &str) -> Option<GuildChannel> {
    let channel = ChannelId::new(snowflake(id)?).to_channel(ctx).await.ok()?.guild()?;
    (channel.guild_id == guild_id).then_some(channel)
}

/// Every member of the guild, paged over REST (1000 per request).
// ponytail: REST paging instead of a gateway opcode 8 request, so the 30s opcode 8 rate limit cannot bite; very large guilds take ~N/1000 requests.
async fn all_members(ctx: &Context, guild_id: GuildId) -> anyhow::Result<Vec<Member>> {
    let mut all = vec![];
    let mut after = None;
    loop {
        let page = guild_id.members(&ctx.http, Some(1000), after).await?;
        let full = page.len() == 1000;
        after = page.last().map(|m| m.user.id);
        all.extend(page);
        if !full {
            return Ok(all);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_change_targets_add_and_remove() {
        // (id, bot, holds r1) mirrors the TS fixture.
        let members = [("admin", false, false), ("human1", false, false), ("human2", false, true), ("bot1", true, true), ("bot2", true, false)];
        let pick = |mode: &str, target: &str| -> Vec<&str> {
            members.iter().filter(|(_, bot, has)| needs_role_change(*bot, *has, mode, target)).map(|m| m.0).collect()
        };
        assert_eq!(pick("add", "everyone"), ["admin", "human1", "bot2"]);
        assert_eq!(pick("remove", "bots"), ["bot1"]);
        assert_eq!(pick("add", "user"), ["admin", "human1", "bot2"]);
        assert_eq!(pick("remove", "everyone"), ["human2", "bot1"]);
    }

    #[test]
    fn role_result_text() {
        let role = RoleId::new(7);
        assert_eq!(role_result("Add", role, "to", "member", 3, &[]), "🟢 Added <@&7> to 3 members.");
        assert_eq!(role_result("Add", role, "to", "member", 1, &[]), "🟢 Added <@&7> to 1 member.");
        assert_eq!(
            role_result("Remove", role, "from", "bot", 2, &[UserId::new(9)]),
            "🟡 Removed <@&7> from 2 bots.\n🔴 Failed for 1: <@9>"
        );
    }

    #[test]
    fn refuses_everyone_unassignable_and_higher_roles() {
        let role = Some((5, Reverse(11)));
        let guild = 1;
        let check = |mode, role, assignable: bool, owner, actor| managed_role_error(mode, role, guild, || assignable, owner, actor);
        assert_eq!(check("add", Some((0, Reverse(guild))), true, false, Some((10, Reverse(2)))), Some("That role no longer exists or cannot be assigned."));
        assert_eq!(check("add", None, true, false, Some((10, Reverse(2)))), Some("That role no longer exists or cannot be assigned."));
        assert_eq!(check("add", role, false, false, Some((10, Reverse(2)))), Some(CANNOT_USE_ROLE));
        assert!(CANNOT_USE_ROLE.contains("Gaia cannot use that role"));
        assert_eq!(check("add", role, true, false, Some((5, Reverse(12)))), Some("You can only manage roles below your own highest role."));
        assert_eq!(check("add", role, true, false, None), Some("You can only manage roles below your own highest role."));
        assert_eq!(check("grant", role, true, false, Some((10, Reverse(2)))), Some("Unknown role action."));
        assert_eq!(check("add", role, true, false, Some((10, Reverse(2)))), None);
        // Same position: the older (smaller id) role is higher.
        assert_eq!(check("add", role, true, false, Some((5, Reverse(10)))), None);
        assert_eq!(check("remove", role, true, true, None), None);
    }

    #[test]
    fn custom_id_parts_and_routing() {
        assert_eq!(parts("adminui:rmapply:add-r1-everyone:a"), ("rmapply", "add-r1-everyone", "a"));
        assert_eq!(parts("adminui:nav"), ("nav", "", ""));
        assert_eq!(section_for("voice_rooms_enabled"), "voice");
    }

    #[test]
    fn dates_use_ljubljana_time() {
        assert_eq!(ljubljana("2026-01-01T00:00:00.000Z"), "01/01/2026, 01:00:00");
        assert_eq!(ljubljana("2026-07-01T00:00:00.000Z"), "01/07/2026, 02:00:00");
        assert!(deployment_token().len() >= 8);
    }
}

/// Behavioural tests over the fake Discord REST harness (`crate::test_support`).
#[cfg(test)]
mod harness_tests {
    use super::*;
    use crate::db::repositories::GuildBackup;
    use crate::test_support::{GuildSeed, Harness, BOT_ID};
    use serde_json::{json, Value};
    use serenity::all::Permissions;

    const G: u64 = 1;
    const OWNER: u64 = 2;
    const ADMIN: u64 = 5;
    const HUMAN1: u64 = 6;
    const HUMAN2: u64 = 7;
    const BOT1: u64 = 8;
    const BOT2: u64 = 9;
    const GAIA_ROLE: u64 = 20;
    const R1: u64 = 30;
    const ADMIN_ROLE: u64 = 40;
    /// Between the admin's highest role (10) and Gaia's (20).
    const ABOVE_ACTOR: u64 = 50;
    const MANAGED: u64 = 51;
    /// Above Gaia's highest role.
    const ABOVE_GAIA: u64 = 52;

    fn ui_with(h: &Harness, repo: Arc<GaiaRepository>) -> AdminUi {
        let mutes = Arc::new(MuteService::new(repo.clone(), h.discord.clone(), None));
        let rooms = Arc::new(VoiceRoomService::new(repo.clone(), h.discord.clone()));
        let verification = Arc::new(VerificationService::new(repo.clone(), h.discord.clone(), None));
        let backups = Arc::new(BackupService::new(repo.clone(), h.discord.clone(), Some(h.audit.clone())));
        AdminUi::new(repo, mutes, rooms, verification, backups, || "0s".to_string(), Some(h.audit.clone()))
    }

    fn ui(h: &Harness) -> AdminUi {
        ui_with(h, h.repo.clone())
    }

    fn id_of(object: &Value) -> u64 {
        object["id"].as_str().and_then(|id| id.parse().ok()).unwrap_or_default()
    }

    /// Mirrors the TS role-management fixture: r1 at position 5, the admin's highest role at `actor_position`,
    /// bots 8 and 9, and r1 held by human2 and bot1. The member list endpoint returns everyone but Gaia.
    async fn fixture(actor_position: u16, administrator: bool) -> (Harness, AdminUi) {
        let h = Harness::start().await;
        let admin = if administrator { Permissions::ADMINISTRATOR } else { Permissions::empty() };
        let mut seed = GuildSeed::new(G, "Guild")
            .role(GAIA_ROLE, "Gaia", 20, Permissions::MANAGE_ROLES)
            .role(ADMIN_ROLE, "Admin", actor_position, admin)
            .role(R1, "role-r1", 5, Permissions::empty())
            .role(ABOVE_ACTOR, "Senior", 15, Permissions::empty())
            .role(MANAGED, "Integration", 3, Permissions::empty())
            .role(ABOVE_GAIA, "Top", 25, Permissions::empty())
            .bot(&[GAIA_ROLE])
            .member(OWNER, &[])
            .member(ADMIN, &[ADMIN_ROLE])
            .member(HUMAN1, &[])
            .member(HUMAN2, &[R1])
            .member(BOT1, &[R1])
            .member(BOT2, &[]);
        for role in seed.json["roles"].as_array_mut().unwrap() {
            if id_of(role) == MANAGED {
                role["managed"] = json!(true);
            }
        }
        for member in seed.json["members"].as_array_mut().unwrap() {
            if [BOT1, BOT2].contains(&id_of(&member["user"])) {
                member["user"]["bot"] = json!(true);
            }
        }
        let listed: Vec<Value> = seed.json["members"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| ![BOT_ID, OWNER].contains(&id_of(&m["user"])))
            .cloned()
            .collect();
        h.seed(seed);
        h.on("GET", "/guilds/1/members", 200, json!(listed));
        let ui = ui(&h);
        (h, ui)
    }

    fn ids(reply: &Value) -> Vec<String> {
        reply["data"]["components"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| row["components"].as_array().unwrap())
            .filter_map(|c| c["custom_id"].as_str().map(str::to_string))
            .collect()
    }

    fn content(reply: &Value) -> String {
        reply["data"]["content"].as_str().unwrap_or_default().to_string()
    }

    fn edits(h: &Harness) -> Vec<Value> {
        h.calls_to("PATCH", "/webhooks/*/*/messages/@original").into_iter().map(|c| c.body).collect()
    }

    fn role_puts(h: &Harness) -> Vec<String> {
        let mut paths: Vec<String> = h.calls_to("PUT", "/guilds/1/members/*/roles/*").into_iter().map(|c| c.path).collect();
        paths.sort();
        paths
    }

    fn field(audit: &Value, name: &str) -> Option<String> {
        audit["fields"].as_array()?.iter().find(|f| f["name"] == name).and_then(|f| f["value"].as_str()).map(str::to_string)
    }

    // ---- L5: role pickers use the role-management hierarchy checks

    #[tokio::test]
    async fn verification_and_autorole_pickers_refuse_roles_the_admin_or_gaia_cannot_manage() {
        let (h, ui) = fixture(10, true).await;
        let ctx = h.ctx();
        let cases = [
            ("verification", G, "That role no longer exists or cannot be assigned."),
            ("verification", MANAGED, CANNOT_USE_ROLE),
            ("verification", ABOVE_GAIA, CANNOT_USE_ROLE),
            ("verification", ABOVE_ACTOR, "You can only manage roles below your own highest role."),
            ("user", ABOVE_ACTOR, "You can only manage roles below your own highest role."),
            ("bot", G, "That role no longer exists or cannot be assigned."),
        ];
        for (action, role, message) in cases {
            h.clear_calls();
            ui.handle_select(&ctx, &h.select(G, ADMIN, &format!("adminui:role:{action}:1"), 6, &[&role.to_string()])).await.unwrap();
            let replies = h.replies();
            assert_eq!(replies.len(), 1, "{action} {role}");
            assert_eq!(content(&replies[0]), message, "{action} {role}");
            assert_eq!(replies[0]["data"]["flags"], 64);
        }
        assert_eq!(h.repo.verification("1").unwrap().role_id, None);
        let autoroles = h.repo.autoroles("1").unwrap();
        assert_eq!((autoroles.user_role_id, autoroles.bot_role_id), (None, None));

        ui.handle_select(&ctx, &h.select(G, ADMIN, "adminui:role:verification:1", 6, &[&R1.to_string()])).await.unwrap();
        assert_eq!(h.repo.verification("1").unwrap().role_id, Some(R1.to_string()));
        // The guild owner is not limited by their own role position, only by Gaia's.
        ui.handle_select(&ctx, &h.select(G, OWNER, "adminui:role:user:1", 6, &[&ABOVE_ACTOR.to_string()])).await.unwrap();
        assert_eq!(h.repo.autoroles("1").unwrap().user_role_id, Some(ABOVE_ACTOR.to_string()));
    }

    // ---- unbounded deployments map

    #[test]
    fn deployment_sessions_expire_and_are_capped() {
        let session = |age: u64| DeploymentSession {
            guild_id: GuildId::new(G),
            user_id: UserId::new(ADMIN),
            channel_id: ChannelId::new(9),
            created: Instant::now().checked_sub(Duration::from_secs(age)).unwrap(),
        };
        let mut sessions = HashMap::new();
        sessions.insert("old".to_string(), session(DEPLOYMENT_TTL.as_secs() + 1));
        remember_deployment(&mut sessions, "new".into(), session(0));
        assert!(!sessions.contains_key("old") && sessions.contains_key("new"));
        for i in 0..MAX_DEPLOYMENTS + 10 {
            remember_deployment(&mut sessions, i.to_string(), session(0));
        }
        assert_eq!(sessions.len(), MAX_DEPLOYMENTS);
        assert!(sessions.contains_key(&(MAX_DEPLOYMENTS + 9).to_string()), "the newest session is kept");
    }

    #[tokio::test]
    async fn refuses_an_expired_deployment_confirmation() {
        let (h, ui) = fixture(10, true).await;
        let created = Instant::now().checked_sub(DEPLOYMENT_TTL + Duration::from_secs(1)).unwrap();
        let session = DeploymentSession { guild_id: GuildId::new(G), user_id: UserId::new(ADMIN), channel_id: ChannelId::new(9), created };
        ui.deployments.lock().unwrap().insert("tok".into(), session);
        ui.handle(&h.ctx(), &h.button(G, ADMIN, "adminui:verifydeploy:tok:1")).await.unwrap();
        assert_eq!(content(&h.replies()[0]), "That deployment confirmation expired. Start again from Verification.");
        assert!(h.calls_to("POST", "/channels/*/messages").is_empty());
    }

    // ---- L6: backup picker

    fn store_backup(repo: &GaiaRepository, id: &str, guild: &str) {
        let backup = GuildBackup {
            backup_id: id.into(),
            source_guild_id: guild.into(),
            source_guild_name: format!("Guild {guild}"),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            created_by: Some(ADMIN.to_string()),
            creation_type: "manual".into(),
            snapshot_json: "{}".into(),
            ..Default::default()
        };
        repo.store_backup(&backup, None, None).unwrap();
    }

    #[tokio::test]
    async fn backup_picker_lists_only_accessible_backups_from_the_cache() {
        let (h, ui) = fixture(10, true).await;
        h.seed(GuildSeed::new(2, "Elsewhere").role(60, "Admin", 1, Permissions::ADMINISTRATOR).member(ADMIN, &[60]));
        h.seed(GuildSeed::new(3, "Member only").member(ADMIN, &[]));
        for (id, guild) in [("here", "1"), ("admin-elsewhere", "2"), ("member-only", "3"), ("gaia-left", "4")] {
            store_backup(&h.repo, id, guild);
        }
        h.clear_calls();
        ui.handle(&h.ctx(), &h.button(G, ADMIN, "adminui:backup:restore:1")).await.unwrap();
        let reply = &h.replies()[0];
        let options: Vec<&str> = reply["data"]["components"][0]["components"][0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap())
            .collect();
        // "gaia-left" stays reachable for its creator (README creator fallback); "member-only" does not.
        assert_eq!(options.len(), 3, "{options:?}");
        assert!(options.contains(&"here") && options.contains(&"admin-elsewhere") && options.contains(&"gaia-left"));
        assert_eq!(h.trace().len(), 1, "answered from cache and SQLite only: {:?}", h.trace());
    }

    // ---- administrator component rechecks

    #[tokio::test]
    async fn rejects_admin_dashboard_and_backup_actions_after_administrator_permission_is_removed() {
        let (h, ui) = fixture(10, false).await;
        let ctx = h.ctx();
        for custom_id in ["adminui:backup:create:1", "adminui:backupcreate:confirm:1", "adminui:nav:dashboard:1"] {
            h.clear_calls();
            ui.handle(&ctx, &h.button(G, ADMIN, custom_id)).await.unwrap();
            assert!(content(&h.replies()[0]).contains("Administrator"), "{custom_id}");
            assert_eq!(h.trace().len(), 1, "{custom_id}: {:?}", h.trace());
        }
        assert!(h.repo.backups("1").unwrap().is_empty());
    }

    // ---- admin role management

    #[tokio::test]
    async fn shows_a_single_remove_add_roles_entry_on_the_dashboard_that_opens_add_and_remove_choices() {
        let (h, ui) = fixture(10, true).await;
        let ctx = h.ctx();
        ui.open(&ctx, &h.command(G, ADMIN, "admin", json!([]))).await.unwrap();
        let dashboard = ids(&h.replies()[0]);
        assert_eq!(dashboard.iter().filter(|id| *id == "adminui:nav:roles:1").count(), 1);
        h.clear_calls();
        ui.handle(&ctx, &h.button(G, ADMIN, "adminui:nav:roles:1")).await.unwrap();
        assert_eq!(ids(&h.replies()[0]), ["adminui:rolemgmt:add:1", "adminui:rolemgmt:remove:1", "adminui:nav:dashboard:1"]);
    }

    #[tokio::test]
    async fn walks_add_role_target_and_offers_user_everyone_and_bots() {
        let (h, ui) = fixture(10, true).await;
        let ctx = h.ctx();
        ui.handle(&ctx, &h.button(G, ADMIN, "adminui:rolemgmt:add:1")).await.unwrap();
        assert_eq!(ids(&h.replies()[0])[0], "adminui:rmrole:add:1");
        h.clear_calls();
        ui.handle_select(&ctx, &h.select(G, ADMIN, "adminui:rmrole:add:1", 6, &["30"])).await.unwrap();
        assert_eq!(
            ids(&h.replies()[0]),
            ["adminui:rmtarget:add-30-user:1", "adminui:rmtarget:add-30-everyone:1", "adminui:rmtarget:add-30-bots:1", "adminui:nav:roles:1"]
        );
    }

    #[tokio::test]
    async fn adds_a_role_to_everyone_who_lacks_it_after_confirmation() {
        let (h, ui) = fixture(10, true).await;
        let ctx = h.ctx();
        ui.handle(&ctx, &h.button(G, ADMIN, "adminui:rmtarget:add-30-everyone:1")).await.unwrap();
        assert!(content(&h.replies()[0]).contains("**3** members"));
        assert!(role_puts(&h).is_empty());
        ui.handle(&ctx, &h.button(G, ADMIN, "adminui:rmapply:add-30-everyone:1")).await.unwrap();
        assert_eq!(role_puts(&h), ["/guilds/1/members/5/roles/30", "/guilds/1/members/6/roles/30", "/guilds/1/members/9/roles/30"]);
        let edits = edits(&h);
        assert_eq!(edits[0]["components"], json!([]));
        assert!(edits.last().unwrap()["content"].as_str().unwrap().contains("Added <@&30> to 3 members"));
        let audits = h.audits().await;
        assert_eq!(audits[0]["title"], "Role Added");
        assert_eq!(field(&audits[0], "Target").as_deref(), Some("Everyone"));
        assert_eq!(field(&audits[0], "Changed").as_deref(), Some("3"));
        assert_eq!(field(&audits[0], "Failed").as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn removes_a_role_only_from_bots_that_hold_it() {
        let (h, ui) = fixture(10, true).await;
        let ctx = h.ctx();
        ui.handle(&ctx, &h.button(G, ADMIN, "adminui:rmtarget:remove-30-bots:1")).await.unwrap();
        assert!(content(&h.replies()[0]).contains("**1** bot?"));
        ui.handle(&ctx, &h.button(G, ADMIN, "adminui:rmapply:remove-30-bots:1")).await.unwrap();
        let removed: Vec<String> = h.calls_to("DELETE", "/guilds/1/members/*/roles/*").into_iter().map(|c| c.path).collect();
        assert_eq!(removed, ["/guilds/1/members/8/roles/30"]);
    }

    #[tokio::test]
    async fn adds_or_removes_a_role_for_a_specific_user_and_skips_no_op_changes() {
        let (h, ui) = fixture(10, true).await;
        let ctx = h.ctx();
        ui.handle(&ctx, &h.button(G, ADMIN, "adminui:rmtarget:add-30-user:1")).await.unwrap();
        assert_eq!(ids(&h.replies()[0])[0], "adminui:rmuser:add-30:1");
        h.clear_calls();
        ui.handle_select(&ctx, &h.select(G, ADMIN, "adminui:rmuser:add-30:1", 5, &["6"])).await.unwrap();
        assert!(content(&h.replies()[0]).contains("Added <@&30> to 1 member."));
        h.clear_calls();
        // human2 already holds r1 (the cache only changes through gateway events, which the harness has none of).
        ui.handle_select(&ctx, &h.select(G, ADMIN, "adminui:rmuser:add-30:1", 5, &["7"])).await.unwrap();
        assert!(content(&h.replies()[0]).contains("already has"));
        h.clear_calls();
        ui.handle_select(&ctx, &h.select(G, ADMIN, "adminui:rmuser:remove-30:1", 5, &["7"])).await.unwrap();
        assert_eq!(h.trace().iter().filter(|c| c.contains("/roles/")).collect::<Vec<_>>(), ["DELETE /guilds/1/members/7/roles/30"]);
        h.clear_calls();
        ui.handle_select(&ctx, &h.select(G, ADMIN, "adminui:rmuser:add-30:1", 5, &["999"])).await.unwrap();
        assert!(content(&h.replies()[0]).contains("not a member"));
        assert!(role_puts(&h).is_empty());
    }

    #[tokio::test]
    async fn reports_per_member_failures_without_stopping_the_run() {
        let (h, ui) = fixture(10, true).await;
        h.on("PUT", "/guilds/1/members/6/roles/30", 403, json!({"code": 50013, "message": "Missing Permissions"}));
        ui.handle(&h.ctx(), &h.button(G, ADMIN, "adminui:rmapply:add-30-everyone:1")).await.unwrap();
        assert_eq!(role_puts(&h).len(), 3, "the failure for member 6 did not stop the run");
        assert!(edits(&h).last().unwrap()["content"].as_str().unwrap().contains("Failed for 1: <@6>"));
        let audits = h.audits().await;
        assert_eq!(audits[0]["color"], 0xed4245);
        assert_eq!(field(&audits[0], "Changed").as_deref(), Some("2"));
        assert_eq!(field(&audits[0], "Failed").as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn rejects_role_management_once_administrator_permission_is_gone() {
        let (h, ui) = fixture(10, false).await;
        ui.handle(&h.ctx(), &h.button(G, ADMIN, "adminui:rmapply:add-30-everyone:1")).await.unwrap();
        assert!(content(&h.replies()[0]).contains("Administrator"));
        assert!(role_puts(&h).is_empty());
    }

    // ---- destructive restore progress lifecycle

    /// Guild 1 with the admin as cached owner; its REST view (Gaia as owner) is served for backups.
    fn backup_guild(h: &Harness, member_count: u64) {
        let mut seed = GuildSeed::new(G, "Guild").owner(ADMIN).bot(&[]).member(ADMIN, &[]);
        seed.json["member_count"] = json!(member_count);
        let mut raw = seed.json.clone();
        raw["owner_id"] = json!(BOT_ID.to_string());
        let mut listed = seed.json["members"].as_array().unwrap().clone();
        listed.push(json!({"user": crate::test_support::user_json(HUMAN1), "roles": [], "joined_at": "2026-01-01T00:00:00.000Z", "deaf": false, "mute": false, "flags": 0}));
        h.seed(seed);
        h.on("GET", "/guilds/1", 200, raw.clone());
        h.on("PATCH", "/guilds/1", 200, raw);
        h.on("GET", "/guilds/1/channels", 200, json!([]));
        h.on("GET", "/guilds/1/members", 200, json!(listed));
    }

    #[tokio::test]
    async fn acknowledges_manual_backup_creation_before_member_acquisition_starts() {
        let h = Harness::start().await;
        backup_guild(&h, 3);
        let ui = ui(&h);
        ui.handle(&h.ctx(), &h.button(G, ADMIN, "adminui:backupcreate:confirm:1")).await.unwrap();
        let trace = h.trace();
        let ack = trace.iter().position(|c| c.starts_with("POST /interactions/")).unwrap();
        let members = trace.iter().position(|c| c == "GET /guilds/1/members").expect("member synchronization ran");
        assert!(ack < members, "{trace:?}");
        assert_eq!(h.replies()[0]["type"], 6);
        assert!(edits(&h).last().unwrap()["content"].as_str().unwrap().starts_with("🟢 Backup Created"));
        assert_eq!(h.repo.backups("1").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn continues_and_persists_restore_work_when_its_progress_message_is_deleted() {
        let h = Harness::start().await;
        backup_guild(&h, 0);
        let ui = ui(&h);
        let backup = ui.backups.create(GuildId::new(G), Some(&ADMIN.to_string()), "manual", None).await.unwrap();
        h.on("PATCH", "/webhooks/*/*/messages/@original", 404, json!({"code": 10008, "message": "Unknown Message"}));
        h.clear_calls();
        let custom_id = format!("adminui:backuprestore:confirm-{}:1", backup.backup_id);
        ui.handle(&h.ctx(), &h.button(G, ADMIN, &custom_id)).await.unwrap();
        assert_eq!(edits(&h).len(), 1, "the first failed edit stops further progress edits");
        let trace = h.trace();
        let failed_edit = trace.iter().position(|c| c.starts_with("PATCH /webhooks/")).unwrap();
        assert!(trace[failed_edit..].iter().any(|c| c == "PATCH /guilds/1"), "restore work continued: {trace:?}");
        let titles: Vec<Value> = h.audits().await.iter().map(|a| a["title"].clone()).collect();
        assert!(titles.contains(&json!("Restore Completed")) || titles.contains(&json!("Restore Completed With Warnings")), "{titles:?}");
        let completion = h.calls_to("PATCH", "/channels/*/messages/*").into_iter().last().expect("progress DM finalized");
        assert!(completion.body["embeds"][0]["title"].as_str().unwrap().contains("Completed"), "{}", completion.body);
    }
}
