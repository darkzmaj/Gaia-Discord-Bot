//! Gaia bot: gateway event wiring, interaction dispatch, moderation commands and the per-server Embed Manager UI.
use crate::config::Config;
use crate::ctx;
use crate::db::repositories::{GaiaRepository, ScheduledEmbedSend};
use crate::discord::Discord;
use crate::logger::{self, log_discord_error};
use crate::services::admin_ui::AdminUi;
use crate::services::audit_log_service::{AuditEvent, AuditLogService, AuditSubject};
use crate::services::authorization::{has_permission, PermissionLevel};
use crate::services::backup_service::BackupService;
use crate::services::command_sync::CommandSynchronizer;
use crate::services::duration::{discord_timestamp_seconds, human_duration, parse_duration};
use crate::services::embed_renderer::{with_default_color, 
    attachment_record, download_attachment, is_discord_cdn, mention_label, parse_mention, render_template, resolve_mention,
    resolve_mention_with, with_delivery_mention, DeliveryMention, RenderedMessage, MAX_ATTACHMENT_BYTES, MAX_TEMPLATE_ATTACHMENTS,
    MAX_TEMPLATE_ATTACHMENT_BYTES,
};
use crate::services::embed_scheduler::EmbedScheduler;
use crate::services::embed_validation::validate_embed_payload;
use crate::services::member_log_service::MemberLogService;
use crate::services::mute_service::MuteService;
use crate::services::schedule_time::parse_ljubljana_schedule;
use crate::services::verification_service::VerificationService;
use crate::services::voice_room_service::VoiceRoomService;
use serenity::all::{
    ActionRowComponent, ButtonStyle, ChannelId, ChannelType, CommandDataOptionValue, CommandInteraction,
    ComponentInteraction, ComponentInteractionDataKind, Context, CreateActionRow, CreateAttachment,
    CreateAutocompleteResponse, CreateButton, CreateEmbed, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, CreateModal, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, EditAttachments, EditInteractionResponse, EditMessage, EventHandler, GatewayIntents, GuildChannel, GuildId,
    InputTextStyle, Interaction, InteractionId, Member, Message, MessageId, ModalInteraction, Permissions, Ready,
    Role, RoleId, User, UserId, VoiceState,
};
use serenity::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const COLOR: u32 = crate::discord::GAIA_PURPLE;

/// GUILD_MESSAGES without MESSAGE_CONTENT: the verification honeypot needs to know a message exists, not its text.
pub fn intents() -> GatewayIntents {
    GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MESSAGES
}

#[derive(Clone, Debug)]
struct PendingEmbed {
    name: String,
    json: String,
    replace: bool,
    user_id: String,
    guild_id: String,
}

/// Shared shape of the TS `embedSessions` and `scheduleSessions` entries.
#[derive(Clone, Debug, Default)]
struct Session {
    guild_id: String,
    user_id: String,
    name: Option<String>,
    template_id: Option<String>,
    channel_id: Option<String>,
    utc: Option<String>,
    display: Option<String>,
    mention: Option<DeliveryMention>,
}

/// Embed Manager sessions expire after this long (swept whenever a new one is created).
const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
/// Live sessions per user per store; creating another evicts that user's oldest.
const SESSIONS_PER_USER: usize = 5;

/// Short-lived interaction state keyed by token, bounded by [`SESSION_TTL`] and [`SESSIONS_PER_USER`].
struct SessionStore<T> {
    entries: Mutex<HashMap<String, (Instant, String, T)>>,
}

impl<T> Default for SessionStore<T> {
    fn default() -> Self {
        Self { entries: Mutex::default() }
    }
}

impl<T: Clone> SessionStore<T> {
    fn insert(&self, token: String, user_id: &str, value: T) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, (at, _, _)| now.duration_since(*at) < SESSION_TTL);
        let mut mine: Vec<(Instant, String)> =
            entries.iter().filter(|(_, (_, user, _))| user == user_id).map(|(token, (at, _, _))| (*at, token.clone())).collect();
        mine.sort();
        let excess = (mine.len() + 1).saturating_sub(SESSIONS_PER_USER);
        for (_, token) in mine.into_iter().take(excess) {
            entries.remove(&token);
        }
        entries.insert(token, (now, user_id.to_string(), value));
    }
    fn get(&self, token: &str) -> Option<T> {
        self.entries.lock().unwrap().get(token).filter(|(at, _, _)| at.elapsed() < SESSION_TTL).map(|(_, _, value)| value.clone())
    }
    /// Applies `f` to a live entry; `None` when it is missing or expired.
    fn update<R>(&self, token: &str, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        self.entries.lock().unwrap().get_mut(token).filter(|(at, _, _)| at.elapsed() < SESSION_TTL).map(|(_, _, value)| f(value))
    }
    fn remove(&self, token: &str) {
        self.entries.lock().unwrap().remove(token);
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
    #[cfg(test)]
    fn backdate(&self, token: &str, by: Duration) {
        if let Some((at, _, _)) = self.entries.lock().unwrap().get_mut(token) {
            *at -= by;
        }
    }
}

const INVALID_NAME: &str = "Embed names must be 1–64 characters without control characters.";

/// Template names (from modals and custom IDs): trimmed, 1–64 characters, no control characters.
fn template_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty() && crate::services::embed_validation::js_len(name) <= 64 && !name.chars().any(char::is_control))
        .then(|| name.to_string())
}

/// One uploaded file from the modal FileUpload component.
#[derive(Clone, Debug, PartialEq)]
pub struct FileUpload {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub content_type: Option<String>,
}

/// `${Date.now().toString(36)}${random base36 chars}`.
fn session_token(random_len: usize) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut stamp = Vec::new();
    loop {
        stamp.push(DIGITS[(millis % 36) as usize]);
        millis /= 36;
        if millis == 0 {
            break;
        }
    }
    stamp.reverse();
    let random = uuid::Uuid::new_v4();
    let tail = random.as_bytes().iter().take(random_len).map(|b| DIGITS[(*b % 36) as usize]);
    String::from_utf8(stamp.into_iter().chain(tail).collect()).unwrap()
}

/// encodeURIComponent.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// decodeURIComponent (malformed sequences are an error, like JS).
fn decode_uri_component(value: &str) -> anyhow::Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3).ok_or_else(|| anyhow::anyhow!("URI malformed"))?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| anyhow::anyhow!("URI malformed"))?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    Ok(String::from_utf8(out)?)
}

pub fn compact_uptime(ms: u128) -> String {
    let total = (ms / 1000) as u64;
    if total < 60 {
        return format!("{total}s");
    }
    let mut parts = Vec::new();
    let mut remaining = total;
    for (suffix, divisor) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        let value = remaining / divisor;
        remaining %= divisor;
        if value != 0 {
            parts.push(format!("{value}{suffix}"));
        }
    }
    parts.join(" ")
}

fn kb(bytes: i64) -> String {
    format!("{:.1}", bytes as f64 / 1024.0)
}

fn unix_seconds(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso).map(|d| d.timestamp()).unwrap_or(0)
}

fn snowflake(id: &str) -> Option<u64> {
    id.parse::<u64>().ok().filter(|v| *v != 0)
}

fn field(name: impl Into<String>, value: impl Into<String>, inline: bool) -> (String, String, bool) {
    (name.into(), value.into(), inline)
}

fn button(id: impl Into<String>, label: impl Into<String>, style: ButtonStyle) -> CreateButton {
    CreateButton::new(id).label(label).style(style)
}

fn string_select(id: impl Into<String>, placeholder: &str, options: Vec<CreateSelectMenuOption>) -> CreateActionRow {
    CreateActionRow::SelectMenu(
        CreateSelectMenu::new(id, CreateSelectMenuKind::String { options }).placeholder(placeholder),
    )
}

fn name_options(names: &[String]) -> Vec<CreateSelectMenuOption> {
    names.iter().map(|name| CreateSelectMenuOption::new(name, name)).collect()
}

fn text_channel_select(id: impl Into<String>) -> CreateActionRow {
    CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            id,
            CreateSelectMenuKind::Channel {
                channel_types: Some(vec![ChannelType::Text, ChannelType::News]),
                default_channels: None,
            },
        )
        .placeholder("Select a text channel"),
    )
}

fn text_modal(custom_id: String, title: &str, inputs: Vec<CreateInputText>) -> CreateInteractionResponse {
    CreateInteractionResponse::Modal(
        CreateModal::new(custom_id, title).components(inputs.into_iter().map(CreateActionRow::InputText).collect()),
    )
}

fn ephemeral() -> CreateInteractionResponseMessage {
    CreateInteractionResponseMessage::new().ephemeral(true)
}

/// The Embed Manager panel shown by `/embed` (rows of 5 and 1 buttons).
pub fn embed_manager_panel(guild_id: &str) -> (CreateEmbed, Vec<CreateActionRow>) {
    let embed = CreateEmbed::new().color(COLOR).title("Gaia Embed Manager").description(
        "Embed templates belong to this server; other servers using Gaia cannot see or use them.\n\nScheduled sends use the latest saved template at delivery time. Use **Attachments** to add, replace, or remove a saved file.",
    );
    let rows = vec![
        CreateActionRow::Buttons(vec![
            button(format!("embedui:create:{guild_id}"), "Create", ButtonStyle::Success),
            button(format!("embedui:edit:{guild_id}"), "Edit", ButtonStyle::Primary),
            button(format!("embedui:send:{guild_id}"), "Send", ButtonStyle::Primary),
            button(format!("embedui:schedule:{guild_id}"), "Scheduled Sends", ButtonStyle::Primary),
            button(format!("embedui:list:{guild_id}"), "List", ButtonStyle::Secondary),
        ]),
        CreateActionRow::Buttons(vec![button(
            format!("embedui:attachments:{guild_id}"),
            "Attachments",
            ButtonStyle::Secondary,
        )]),
    ];
    (embed, rows)
}

/// The mention picker shared by manual sends and scheduled sends.
pub fn mention_settings(mode: &str, token: &str, mention: &DeliveryMention, can_mass: bool) -> (CreateEmbed, Vec<CreateActionRow>) {
    let mass = if mention.everyone {
        "@everyone"
    } else if mention.here {
        "@here"
    } else {
        "None"
    };
    let roles = if mention.roles.is_empty() {
        "None".to_string()
    } else {
        mention.roles.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(", ")
    };
    let mut fields = vec![
        field("Current", mention_label(mention), false),
        field("Roles", roles, true),
        field("Mass Mention", mass, true),
    ];
    if !can_mass {
        fields.push(field(
            "Permission",
            "Gaia currently cannot use @everyone or @here. Delivery will safely degrade if that remains true.",
            false,
        ));
    }
    let embed = CreateEmbed::new()
        .color(COLOR)
        .title("Mention Settings")
        .description("Choose who should be notified when this embed is delivered.")
        .fields(fields);
    let style = |on: bool| if on { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let rows = vec![
        CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                format!("embedui:mentionroles:{mode}:{token}"),
                CreateSelectMenuKind::Role { default_roles: None },
            )
            .placeholder("Select roles to mention")
            .min_values(0)
            .max_values(25),
        ),
        CreateActionRow::Buttons(vec![
            button(format!("embedui:mentionmass:everyone:{mode}:{token}"), "@everyone", style(mention.everyone)),
            button(format!("embedui:mentionmass:here:{mode}:{token}"), "@here", style(mention.here)),
            button(format!("embedui:nomention:{mode}:{token}"), "No Mention", ButtonStyle::Secondary),
            button(format!("embedui:mentioncontinue:{mode}:{token}"), "Continue", ButtonStyle::Success),
        ]),
    ];
    (embed, rows)
}

/// Toggles one mass mention; selecting one clears the other (TS `mentionmass`).
fn toggle_mass(current: &DeliveryMention, mass: &str) -> DeliveryMention {
    DeliveryMention {
        roles: current.roles.clone(),
        everyone: mass == "everyone" && !current.everyone,
        here: mass == "here" && !current.here,
    }
}

/// Extracts the first file of the `attachment` FileUpload from a modal-submit interaction serialized as JSON.
/// Accepts both the full interaction and its `data` object; Label-wrapped and bare components are both searched.
pub fn parse_file_upload(raw: &Value, custom_id: &str) -> Option<FileUpload> {
    let data = raw.get("data").unwrap_or(raw);
    let mut ids: Vec<String> = Vec::new();
    let mut stack: Vec<&Value> = data.get("components")?.as_array()?.iter().collect();
    while let Some(component) = stack.pop() {
        if component.get("type").and_then(Value::as_u64) == Some(19)
            && component.get("custom_id").and_then(Value::as_str) == Some(custom_id)
        {
            ids.extend(component.get("values")?.as_array()?.iter().filter_map(|v| v.as_str().map(String::from)));
        }
        if let Some(inner) = component.get("component") {
            stack.push(inner);
        }
        if let Some(children) = component.get("components").and_then(Value::as_array) {
            stack.extend(children);
        }
    }
    let attachment = data.get("resolved")?.get("attachments")?.get(ids.first()?)?;
    Some(FileUpload {
        name: attachment.get("filename")?.as_str()?.to_string(),
        url: attachment.get("url")?.as_str()?.to_string(),
        size: attachment.get("size")?.as_u64()?,
        content_type: attachment.get("content_type").and_then(Value::as_str).map(String::from),
    })
}

/// One interaction regardless of its kind, so the embed handlers can share response helpers.
#[derive(Clone, Copy)]
enum Ix<'a> {
    Command(&'a CommandInteraction),
    Component(&'a ComponentInteraction),
    Modal(&'a ModalInteraction),
}

impl<'a> Ix<'a> {
    fn id(&self) -> InteractionId {
        match self {
            Ix::Command(i) => i.id,
            Ix::Component(i) => i.id,
            Ix::Modal(i) => i.id,
        }
    }
    fn guild_id(&self) -> Option<GuildId> {
        match self {
            Ix::Command(i) => i.guild_id,
            Ix::Component(i) => i.guild_id,
            Ix::Modal(i) => i.guild_id,
        }
    }
    fn user(&self) -> &'a User {
        match self {
            Ix::Command(i) => &i.user,
            Ix::Component(i) => &i.user,
            Ix::Modal(i) => &i.user,
        }
    }
    fn custom_id(&self) -> Option<&'a str> {
        match self {
            Ix::Command(_) => None,
            Ix::Component(i) => Some(&i.data.custom_id),
            Ix::Modal(i) => Some(&i.data.custom_id),
        }
    }
    fn is_button(&self) -> bool {
        matches!(self, Ix::Component(i) if matches!(i.data.kind, ComponentInteractionDataKind::Button))
    }
    fn is_string_select(&self) -> bool {
        matches!(self, Ix::Component(i) if matches!(i.data.kind, ComponentInteractionDataKind::StringSelect { .. }))
    }
    fn is_role_select(&self) -> bool {
        matches!(self, Ix::Component(i) if matches!(i.data.kind, ComponentInteractionDataKind::RoleSelect { .. }))
    }
    fn is_channel_select(&self) -> bool {
        matches!(self, Ix::Component(i) if matches!(i.data.kind, ComponentInteractionDataKind::ChannelSelect { .. }))
    }
    fn is_modal(&self) -> bool {
        matches!(self, Ix::Modal(_))
    }
    fn values(&self) -> Vec<String> {
        match self {
            Ix::Component(i) => match &i.data.kind {
                ComponentInteractionDataKind::StringSelect { values } => values.clone(),
                ComponentInteractionDataKind::RoleSelect { values } => values.iter().map(|v| v.to_string()).collect(),
                ComponentInteractionDataKind::ChannelSelect { values } => values.iter().map(|v| v.to_string()).collect(),
                ComponentInteractionDataKind::UserSelect { values } => values.iter().map(|v| v.to_string()).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
    /// discord.js `fields.getTextInputValue`.
    fn text(&self, custom_id: &str) -> String {
        let Ix::Modal(i) = self else { return String::new() };
        i.data
            .components
            .iter()
            .flat_map(|row| row.components.iter())
            .find_map(|component| match component {
                ActionRowComponent::InputText(input) if input.custom_id == custom_id => {
                    Some(input.value.clone().unwrap_or_default())
                }
                _ => None,
            })
            .unwrap_or_default()
    }
    async fn respond(&self, discord: &Discord, response: CreateInteractionResponse) -> serenity::Result<()> {
        let http = &discord.http;
        match self {
            Ix::Command(i) => i.create_response(http, response).await,
            Ix::Component(i) => i.create_response(http, response).await,
            Ix::Modal(i) => i.create_response(http, response).await,
        }
    }
}

pub struct GaiaBot {
    pub discord: Discord,
    repo: Arc<GaiaRepository>,
    pub mutes: Arc<MuteService>,
    pub rooms: Arc<VoiceRoomService>,
    pub admin_ui: Arc<AdminUi>,
    pub verification: Arc<VerificationService>,
    pub backups: Arc<BackupService>,
    pub embed_scheduler: Arc<EmbedScheduler>,
    pub audit: Arc<AuditLogService>,
    pub member_log: Arc<MemberLogService>,
    command_sync: Arc<CommandSynchronizer>,
    started_at: Arc<Mutex<Instant>>,
    ready_once: AtomicBool,
    /// Notified when the ready sequence fails; `main` then exits with code 1.
    pub startup_failed: tokio::sync::Notify,
    pending_embeds: SessionStore<PendingEmbed>,
    embed_sessions: SessionStore<Session>,
    schedule_sessions: SessionStore<Session>,
}

fn audit_event(title: &str, category: &str, color: &str) -> AuditEvent {
    AuditEvent { title: title.into(), category: category.into(), color: color.into(), ..Default::default() }
}

fn subject(id: impl ToString, name: Option<String>) -> Option<AuditSubject> {
    Some(AuditSubject { id: Some(id.to_string()), name })
}

fn details(pairs: &[(&str, String)]) -> Vec<(String, Value)> {
    pairs.iter().map(|(k, v)| (k.to_string(), Value::String(v.clone()))).collect()
}

impl GaiaBot {
    pub fn new(repo: Arc<GaiaRepository>, application_id: Option<String>, config: &Config, discord: Discord) -> Self {
        let audit = Arc::new(AuditLogService::new(config.audit_logging.clone(), reqwest::Client::new()));
        let member_log = Arc::new(MemberLogService::new(config.member_logging.clone(), reqwest::Client::new()));
        let mutes = Arc::new(MuteService::new(repo.clone(), discord.clone(), Some(audit.clone())));
        let rooms = Arc::new(VoiceRoomService::new(repo.clone(), discord.clone()));
        let verification = Arc::new(VerificationService::new(repo.clone(), discord.clone(), Some(audit.clone())));
        let backups = Arc::new(BackupService::new(repo.clone(), discord.clone(), Some(audit.clone())));
        let embed_scheduler = Arc::new(EmbedScheduler::new(repo.clone(), discord.clone(), Some(audit.clone())));
        let command_sync = Arc::new(CommandSynchronizer::new(repo.clone(), application_id));
        let started_at = Arc::new(Mutex::new(Instant::now()));
        let uptime_start = started_at.clone();
        let admin_ui = Arc::new(AdminUi::new(
            repo.clone(),
            mutes.clone(),
            rooms.clone(),
            verification.clone(),
            backups.clone(),
            move || compact_uptime(uptime_start.lock().unwrap().elapsed().as_millis()),
            Some(audit.clone()),
        ));
        Self {
            discord,
            repo,
            mutes,
            rooms,
            admin_ui,
            verification,
            backups,
            embed_scheduler,
            audit,
            member_log,
            command_sync,
            started_at,
            ready_once: AtomicBool::new(false),
            startup_failed: tokio::sync::Notify::new(),
            pending_embeds: SessionStore::default(),
            embed_sessions: SessionStore::default(),
            schedule_sessions: SessionStore::default(),
        }
    }

    // ---- cache helpers (guards are dropped before any await) ----

    fn guild_name(&self, guild_id: GuildId) -> Option<String> {
        self.discord.cache.guild(guild_id).map(|g| g.name.clone())
    }

    fn bot_id(&self) -> UserId {
        self.discord.cache.current_user().id
    }

    fn bot_permissions(&self, guild_id: GuildId) -> Permissions {
        let me = self.bot_id();
        let Some(guild) = self.discord.cache.guild(guild_id) else { return Permissions::empty() };
        guild.members.get(&me).map(|m| guild.member_permissions(m)).unwrap_or_else(Permissions::empty)
    }

    /// A cached guild channel (or thread) that can receive messages, like discord.js `isTextBased() && "send" in channel`.
    fn text_channel(&self, guild_id: GuildId, channel_id: &str) -> Option<GuildChannel> {
        let id = ChannelId::new(snowflake(channel_id)?);
        let guild = self.discord.cache.guild(guild_id)?;
        let channel = guild.channels.get(&id).cloned().or_else(|| guild.threads.iter().find(|t| t.id == id).cloned())?;
        matches!(
            channel.kind,
            ChannelType::Text
                | ChannelType::News
                | ChannelType::Voice
                | ChannelType::Stage
                | ChannelType::PublicThread
                | ChannelType::PrivateThread
                | ChannelType::NewsThread
        )
        .then_some(channel)
    }

    async fn fetch_member(&self, guild_id: GuildId, user_id: UserId) -> Option<Member> {
        guild_id.member((&self.discord.cache, self.discord.http.as_ref()), user_id).await.ok()
    }

    async fn has_level(&self, member: &Member, level: PermissionLevel) -> bool {
        has_permission(&self.repo, &self.discord, member, level).await.unwrap_or(false)
    }

    // ---- responses ----

    async fn reply(&self, ix: Ix<'_>, content: &str) {
        if let Err(error) = ix.respond(&self.discord, CreateInteractionResponse::Message(ephemeral().content(content))).await {
            log_discord_error(
                "interaction.reply",
                &error.into(),
                ctx! {"interactionId" => ix.id().to_string(), "guildId" => ix.guild_id().map(|g| g.to_string())},
            );
        }
    }

    async fn update(&self, ix: Ix<'_>, message: CreateInteractionResponseMessage) -> anyhow::Result<()> {
        Ok(ix.respond(&self.discord, CreateInteractionResponse::UpdateMessage(message)).await?)
    }

    async fn send(&self, ix: Ix<'_>, message: CreateInteractionResponseMessage) -> anyhow::Result<()> {
        Ok(ix.respond(&self.discord, CreateInteractionResponse::Message(message)).await?)
    }

    // ---- gateway events ----

    async fn on_ready(self: &Arc<Self>) -> anyhow::Result<()> {
        *self.started_at.lock().unwrap() = Instant::now();
        let guilds = self.discord.cache.guilds();
        logger::info(
            "Gaia started",
            ctx! {"guilds" => guilds.len(), "userId" => self.bot_id().to_string(), "applicationId" => self.discord.http.application_id().map(|a| a.to_string())},
        );
        self.audit.record(AuditEvent {
            details: vec![("Guilds".into(), json!(guilds.len())), ("Version".into(), json!("1.0.0"))],
            ..audit_event("Gaia Started", "Bot", "success")
        });
        // Webhook destination validation is observability-only and never affects startup.
        let audit = self.audit.clone();
        tokio::spawn(async move { audit.initialize().await });
        let member_log = self.member_log.clone();
        tokio::spawn(async move { member_log.initialize().await });
        self.command_sync.synchronize(&self.discord, false).await?;
        self.mutes.start();
        self.repo.interrupt_running_restores()?;
        for guild_id in guilds {
            let id = guild_id.to_string();
            self.repo.ensure_guild(&id)?;
            let settings = self.repo.settings(&id)?;
            let roles = self.repo.autoroles(&id)?;
            if settings.autorole_enabled != 0
                && roles.user_role_id.is_some()
                && self.verification.disable_for_user_autorole(guild_id, &self.rooms).await?
            {
                logger::warn(
                    "[verification] disabled conflicting verification configuration because user autorole is active",
                    ctx! {"guildId" => id},
                );
            }
            self.rooms.recover(guild_id).await?;
        }
        self.backups.start();
        self.embed_scheduler.start().await?;
        Ok(())
    }

    async fn on_guild_create(&self, guild_id: GuildId, name: String) -> anyhow::Result<()> {
        let id = guild_id.to_string();
        self.repo.ensure_guild(&id)?;
        self.repo.mark_backup_source_available(&id)?;
        self.command_sync.clear_guild_on_join(&self.discord, guild_id).await;
        self.audit.record(AuditEvent { guild: subject(&id, Some(name)), ..audit_event("Joined Guild", "Bot", "info") });
        Ok(())
    }

    fn on_guild_delete(&self, guild_id: GuildId, name: Option<String>) {
        let id = guild_id.to_string();
        self.audit.record(AuditEvent {
            priority: "critical".into(),
            guild: subject(&id, name),
            ..audit_event("Left Guild", "Bot", "danger")
        });
        let result = self.repo.mark_backup_source_removed(&id).and_then(|_| self.repo.purge_guild(&id));
        match result {
            Ok(()) => {
                self.rooms.forget_guild(&id);
                logger::info("guild data purged after removal", ctx! {"guildId" => id});
            }
            Err(_) => logger::error("guild cleanup failed", ctx! {"guildId" => id}),
        }
    }

    async fn on_member_add(&self, member: &Member) -> anyhow::Result<()> {
        self.mutes.apply_mute_on_join(member).await?;
        let guild_id = member.guild_id.to_string();
        if self.repo.settings(&guild_id)?.autorole_enabled == 0 {
            return Ok(());
        }
        let roles = self.repo.autoroles(&guild_id)?;
        let Some(role_id) = (if member.user.bot { roles.bot_role_id } else { roles.user_role_id }) else {
            return Ok(());
        };
        let guild_name = self.guild_name(member.guild_id);
        let target = subject(member.user.id, Some(member.user.tag()));
        let role = self.assignable_autorole(member.guild_id, &role_id);
        let Some(role) = role else {
            logger::warn("autorole unavailable", ctx! {"guildId" => guild_id, "userId" => member.user.id.to_string(), "roleId" => role_id});
            self.audit.record(AuditEvent {
                priority: "critical".into(),
                guild: subject(&guild_id, guild_name),
                target,
                details: details(&[("Role", role_id.clone()), ("Reason", "Configured role is unavailable or above Gaia.".into())]),
                ..audit_event("AutoRole Assignment Failed", "AutoRole", "danger")
            });
            return Ok(());
        };
        if self
            .discord
            .http
            .add_member_role(member.guild_id, member.user.id, role.id, Some("Gaia autorole"))
            .await
            .is_err()
        {
            logger::warn("autorole assignment failed", ctx! {"guildId" => guild_id, "userId" => member.user.id.to_string(), "roleId" => role_id});
            self.audit.record(AuditEvent {
                priority: "critical".into(),
                guild: subject(&guild_id, guild_name),
                target,
                details: details(&[("Role", role.name.clone())]),
                ..audit_event("AutoRole Assignment Failed", "AutoRole", "danger")
            });
        }
        Ok(())
    }

    /// The autorole when Gaia can assign it: exists, not managed, Manage Roles, and below Gaia's highest role.
    fn assignable_autorole(&self, guild_id: GuildId, role_id: &str) -> Option<Role> {
        let role_id = RoleId::new(snowflake(role_id)?);
        let me = self.bot_id();
        let guild = self.discord.cache.guild(guild_id)?;
        let role = guild.roles.get(&role_id)?.clone();
        let me = guild.members.get(&me)?;
        if !guild.member_permissions(me).manage_roles() || role.managed {
            return None;
        }
        // discord.js comparePositionTo: position first, then the older (smaller) id ranks higher.
        let highest = me
            .roles
            .iter()
            .filter_map(|id| guild.roles.get(id))
            .map(|r| (r.position, std::cmp::Reverse(r.id)))
            .max()
            .unwrap_or((0, std::cmp::Reverse(RoleId::new(guild_id.get()))));
        (highest > (role.position, std::cmp::Reverse(role.id))).then_some(role)
    }

    async fn on_channel_delete(&self, guild_id: GuildId, channel_id: ChannelId) -> anyhow::Result<()> {
        let (guild, channel) = (guild_id.to_string(), channel_id.to_string());
        let settings = self.repo.settings(&guild)?;
        let join = self.repo.join_channel(&guild)?;
        let verification = self.repo.verification(&guild)?;
        if join.as_deref() == Some(channel.as_str()) {
            self.repo.set_join_channel(&guild, None)?;
        }
        if settings.ticket_category_id.as_deref() == Some(channel.as_str()) {
            self.repo.update_setting(&guild, "ticket_category_id", rusqlite::types::Value::Null)?;
        }
        if verification.channel_id.as_deref() == Some(channel.as_str()) {
            self.repo.update_verification(
                &guild,
                &crate::db::repositories::VerificationUpdate {
                    channel_id: Some(None),
                    message_id: Some(None),
                    ..Default::default()
                },
            )?;
        }
        self.repo.delete_room(&guild, &channel)?;
        self.repo.remove_mute_restores_for_channel(&guild, &channel)?;
        self.repo.remove_sent_embeds_in_channel(&guild, &channel)?;
        Ok(())
    }

    async fn on_role_delete(&self, guild_id: GuildId, role_id: RoleId) -> anyhow::Result<()> {
        let (guild, role) = (guild_id.to_string(), role_id.to_string());
        let roles = self.repo.autoroles(&guild)?;
        let verification = self.repo.verification(&guild)?;
        if roles.user_role_id.as_deref() == Some(role.as_str()) {
            self.repo.set_autorole(&guild, "user_role_id", None)?;
        }
        if roles.bot_role_id.as_deref() == Some(role.as_str()) {
            self.repo.set_autorole(&guild, "bot_role_id", None)?;
        }
        if verification.role_id.as_deref() == Some(role.as_str()) {
            self.repo.update_verification(
                &guild,
                &crate::db::repositories::VerificationUpdate { role_id: Some(None), ..Default::default() },
            )?;
            self.rooms.reconcile_guild_access(guild_id, Some(&role)).await?;
            logger::warn("verification role deleted; configuration requires repair", ctx! {"guildId" => guild, "roleId" => role});
        }
        Ok(())
    }

    // ---- interactions ----

    pub async fn on_interaction(&self, ctx: &Context, interaction: &Interaction) {
        let (ix, custom_id) = match interaction {
            Interaction::Command(i) | Interaction::Autocomplete(i) => (Ix::Command(i), None),
            Interaction::Component(i) => (Ix::Component(i), Some(i.data.custom_id.clone())),
            Interaction::Modal(i) => (Ix::Modal(i), Some(i.data.custom_id.clone())),
            _ => return,
        };
        if let Err(error) = self.route(ctx, interaction).await {
            log_discord_error(
                "interaction.handle",
                &error,
                ctx! {"interactionId" => ix.id().to_string(), "guildId" => ix.guild_id().map(|g| g.to_string()), "customId" => custom_id},
            );
            if !matches!(interaction, Interaction::Autocomplete(_)) {
                // No replied/deferred tracking: Discord rejects the reply with 40060 when it was already acknowledged.
                let response = CreateInteractionResponse::Message(
                    ephemeral().content("Gaia encountered an unexpected error. Please try again."),
                );
                if let Err(error) = ix.respond(&self.discord, response).await {
                    if logger::discord_code(&error) != Some(40060) {
                        log_discord_error(
                            "interaction.reply",
                            &error.into(),
                            ctx! {"interactionId" => ix.id().to_string(), "guildId" => ix.guild_id().map(|g| g.to_string())},
                        );
                    }
                }
            }
        }
    }

    async fn route(&self, ctx: &Context, interaction: &Interaction) -> anyhow::Result<()> {
        match interaction {
            Interaction::Component(i) => {
                let id = i.data.custom_id.as_str();
                match &i.data.kind {
                    ComponentInteractionDataKind::Button => {
                        if id == "verify:click" {
                            self.verification.button(i).await?;
                        } else if id.starts_with("adminui:") {
                            self.admin_ui.handle(ctx, i).await?;
                        } else if id.starts_with("embedui:") {
                            self.handle_embed_ui(Ix::Component(i)).await?;
                        } else if id.starts_with("roomreset:") {
                            self.rooms.handle_reset(i).await?;
                        } else if id.starts_with("room:") {
                            self.rooms.handle_button(i).await?;
                        } else if id.starts_with("embedflow:") {
                            self.handle_embed_flow(i).await?;
                        }
                    }
                    ComponentInteractionDataKind::MentionableSelect { .. } | ComponentInteractionDataKind::Unknown(_) => {}
                    kind => {
                        if id.starts_with("adminui:") {
                            self.admin_ui.handle_select(ctx, i).await?;
                        } else if id.starts_with("embedui:") && !matches!(kind, ComponentInteractionDataKind::UserSelect { .. }) {
                            self.handle_embed_ui(Ix::Component(i)).await?;
                        } else if id.starts_with("roompick:") && matches!(kind, ComponentInteractionDataKind::UserSelect { .. }) {
                            self.rooms.handle_user_select(i).await?;
                        } else if id.starts_with("roomaccess:") && matches!(kind, ComponentInteractionDataKind::StringSelect { .. }) {
                            self.rooms.handle_string_select(i).await?;
                        }
                    }
                }
            }
            Interaction::Modal(i) => {
                let id = i.data.custom_id.as_str();
                if id.starts_with("adminui:") {
                    self.admin_ui.handle_modal(ctx, i).await?;
                } else if id.starts_with("embedui:") {
                    self.handle_embed_ui(Ix::Modal(i)).await?;
                } else if id.starts_with("roommodal:") {
                    self.rooms.handle_modal(i).await?;
                } else if id.starts_with("embedmodal:") {
                    self.handle_embed_modal(i).await?;
                }
            }
            Interaction::Autocomplete(i) => self.handle_autocomplete(i).await?,
            Interaction::Command(i) => self.handle_command(ctx, i).await?,
            _ => {}
        }
        Ok(())
    }

    /// The invoking member when they hold `level`; otherwise replies with the standard refusal.
    async fn allowed(&self, i: &CommandInteraction, level: PermissionLevel) -> Option<Member> {
        let member = match i.guild_id {
            Some(guild_id) => self.fetch_member(guild_id, i.user.id).await,
            None => None,
        };
        match member {
            Some(member) if self.has_level(&member, level).await => Some(member),
            _ => {
                self.reply(Ix::Command(i), "You do not have permission to use that Gaia command.").await;
                None
            }
        }
    }

    async fn handle_command(&self, ctx: &Context, i: &CommandInteraction) -> anyhow::Result<()> {
        if i.guild_id.is_none() {
            self.reply(Ix::Command(i), "Gaia commands can only be used in a server.").await;
            return Ok(());
        }
        match i.data.name.as_str() {
            "admin" => self.admin_ui.open(ctx, i).await?,
            "mute" | "unmute" => self.moderate(i).await?,
            "unverify" => self.unverify(i).await?,
            "embed" => self.embed(i).await?,
            _ => {}
        }
        Ok(())
    }

    fn option_user(i: &CommandInteraction, name: &str) -> Option<UserId> {
        i.data.options.iter().find(|o| o.name == name).and_then(|o| match o.value {
            CommandDataOptionValue::User(id) => Some(id),
            _ => None,
        })
    }

    fn option_string(i: &CommandInteraction, name: &str) -> Option<String> {
        i.data.options.iter().find(|o| o.name == name).and_then(|o| match &o.value {
            CommandDataOptionValue::String(value) => Some(value.clone()),
            _ => None,
        })
    }

    async fn moderate(&self, i: &CommandInteraction) -> anyhow::Result<()> {
        let Some(moderator) = self.allowed(i, PermissionLevel::Staff).await else { return Ok(()) };
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let ix = Ix::Command(i);
        let user = Self::option_user(i, "user").ok_or_else(|| anyhow::anyhow!("missing user option"))?;
        let raw_reason = Self::option_string(i, "reason");
        let reason = raw_reason.clone().unwrap_or_else(|| "No reason provided.".into());
        let dm = |delivered: bool| if delivered { "🟢 Sent" } else { "🟡 Could not deliver" };
        if i.data.name == "unmute" {
            let result = self.mutes.unmute(guild_id, &user.to_string(), &reason, Some(&moderator)).await?;
            if result.error.is_some() || !result.changed {
                self.reply(ix, result.error.as_deref().unwrap_or("That member is not muted.")).await;
                return Ok(());
            }
            let embed = CreateEmbed::new()
                .color(0x57f287)
                .title("Member Unmuted")
                .description(format!("<@{user}> has been unmuted."))
                .field("Reason", &reason, false)
                .field("DM", dm(result.dm_delivered.unwrap_or(false)), false);
            return self.send(ix, ephemeral().embed(embed)).await;
        }
        let Some(member) = self.fetch_member(guild_id, user).await else {
            self.reply(ix, "That user is not a member of this server.").await;
            return Ok(());
        };
        let Some(duration) = parse_duration(&Self::option_string(i, "duration").unwrap_or_default()) else {
            self.reply(ix, "Use a valid positive duration up to 30 days, such as `1d 4h 30m`.").await;
            return Ok(());
        };
        let result = self.mutes.mute(guild_id, &member, &moderator, duration, raw_reason.as_deref()).await?;
        if let Some(error) = &result.error {
            self.reply(ix, error).await;
            return Ok(());
        }
        let expires = discord_timestamp_seconds(result.expires_at.unwrap_or_default());
        let embed = CreateEmbed::new()
            .color(0xed4245)
            .title("Member Muted")
            .description(format!("<@{}> has been muted for {}.", member.user.id, human_duration(duration)))
            .field("Expires", format!("<t:{expires}:F>\n<t:{expires}:R>"), false)
            .field("Reason", &reason, false)
            .field("DM", dm(result.dm_delivered.unwrap_or(false)), false);
        self.send(ix, ephemeral().embed(embed)).await
    }

    async fn unverify(&self, i: &CommandInteraction) -> anyhow::Result<()> {
        let Some(administrator) = self.allowed(i, PermissionLevel::Administrator).await else { return Ok(()) };
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        i.create_response(&self.discord.http, CreateInteractionResponse::Defer(ephemeral())).await?;
        let user = Self::option_user(i, "user").ok_or_else(|| anyhow::anyhow!("missing user option"))?;
        let result = self.verification.unverify(guild_id, &user.to_string(), Some(&administrator)).await?;
        let edit = if let Some(error) = result.error {
            EditInteractionResponse::new().content(error)
        } else if !result.changed {
            EditInteractionResponse::new().embed(
                CreateEmbed::new()
                    .color(0xfee75c)
                    .title("🟡 Member Not Verified")
                    .description(format!("<@{user}> does not currently have the verification role.")),
            )
        } else {
            EditInteractionResponse::new().embed(
                CreateEmbed::new()
                    .color(0x57f287)
                    .title("🟢 Member Unverified")
                    .description(format!("<@{user}> is no longer verified.")),
            )
        };
        i.edit_response(&self.discord.http, edit).await?;
        Ok(())
    }

    /// `/embed`: attachments are intentionally managed from the Embed Manager modal, not slash options.
    async fn embed(&self, i: &CommandInteraction) -> anyhow::Result<()> {
        if self.allowed(i, PermissionLevel::Administrator).await.is_none() {
            return Ok(());
        }
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let (embed, rows) = embed_manager_panel(&guild_id.to_string());
        self.send(Ix::Command(i), ephemeral().embed(embed).components(rows)).await
    }

    fn sessions(&self, mode: &str) -> &SessionStore<Session> {
        if mode == "manual" {
            &self.embed_sessions
        } else {
            &self.schedule_sessions
        }
    }

    fn mention_message(&self, mode: &str, token: &str, mention: &DeliveryMention, guild_id: GuildId) -> CreateInteractionResponseMessage {
        let can_mass = self.bot_permissions(guild_id).mention_everyone();
        let (embed, rows) = mention_settings(mode, token, mention, can_mass);
        CreateInteractionResponseMessage::new().embed(embed).components(rows)
    }

    /// A session owned by this user in this guild (TS `mentionSession` checks).
    fn owned_session(&self, mode: &str, token: &str, user: UserId, guild_id: GuildId) -> Option<Session> {
        self.sessions(mode).get(token).filter(|s| s.user_id == user.to_string() && s.guild_id == guild_id.to_string())
    }

    fn set_mention(&self, mode: &str, token: &str, mention: DeliveryMention) {
        self.sessions(mode).update(token, |session| session.mention = Some(mention));
    }

    /// Every embed handler (buttons, selects, modals) re-checks current Administrator permission.
    async fn is_embed_admin(&self, guild_id: GuildId, user: UserId) -> bool {
        match self.fetch_member(guild_id, user).await {
            Some(member) => self.has_level(&member, PermissionLevel::Administrator).await,
            None => false,
        }
    }

    async fn handle_embed_ui(&self, i: Ix<'_>) -> anyhow::Result<()> {
        let Some(guild_id) = i.guild_id() else { return Ok(()) };
        let user = i.user();
        if !self.is_embed_admin(guild_id, user.id).await {
            self.reply(i, "You need Discord Administrator permission to manage embeds.").await;
            return Ok(());
        }
        let custom_id = i.custom_id().unwrap_or_default();
        let pieces: Vec<&str> = custom_id.split(':').collect();
        let piece = |n: usize| pieces.get(n).copied().unwrap_or("");
        let (action, token_or_guild) = (piece(1), piece(2));
        let gid = guild_id.to_string();
        let uid = user.id.to_string();
        let expired = "That mention session expired.";

        if i.is_role_select() && action == "mentionroles" {
            let (mode, token) = (piece(2), piece(3));
            let Some(session) = self.owned_session(mode, token, user.id, guild_id) else {
                self.reply(i, expired).await;
                return Ok(());
            };
            let values = i.values();
            let roles: Vec<String> = {
                let guild = self.discord.cache.guild(guild_id);
                values
                    .iter()
                    .filter(|id| {
                        let role = snowflake(id).and_then(|v| guild.as_ref()?.roles.get(&RoleId::new(v)).cloned());
                        role.is_some_and(|r| !r.managed)
                    })
                    .cloned()
                    .collect()
            };
            if roles.len() != values.len() {
                self.reply(i, "One or more selected roles are unavailable or managed by an integration.").await;
                return Ok(());
            }
            let mention = DeliveryMention { roles, ..session.mention.unwrap_or_default() };
            self.set_mention(mode, token, mention.clone());
            return self.update(i, self.mention_message(mode, token, &mention, guild_id)).await;
        }
        if i.is_button() && action == "mentionmass" {
            let (mass, mode, token) = (piece(2), piece(3), piece(4));
            let Some(session) = self.owned_session(mode, token, user.id, guild_id) else {
                self.reply(i, expired).await;
                return Ok(());
            };
            let mention = toggle_mass(&session.mention.unwrap_or_default(), mass);
            self.set_mention(mode, token, mention.clone());
            return self.update(i, self.mention_message(mode, token, &mention, guild_id)).await;
        }
        if i.is_button() && action == "nomention" {
            let (mode, token) = (piece(2), piece(3));
            if self.owned_session(mode, token, user.id, guild_id).is_none() {
                self.reply(i, expired).await;
                return Ok(());
            }
            let mention = DeliveryMention::default();
            self.set_mention(mode, token, mention.clone());
            return self.update(i, self.mention_message(mode, token, &mention, guild_id)).await;
        }
        if i.is_button() && action == "mentioncontinue" {
            let (mode, token) = (piece(2), piece(3));
            let Some(session) = self.owned_session(mode, token, user.id, guild_id).filter(|s| s.channel_id.is_some()) else {
                self.reply(i, expired).await;
                return Ok(());
            };
            let mention = session.mention.clone().unwrap_or_default();
            let mass = mention.everyone || mention.here;
            let mut fields = vec![
                field("Embed", session.name.clone().unwrap_or_default(), true),
                field("Destination", format!("<#{}>", session.channel_id.clone().unwrap_or_default()), true),
                field("Mentions", mention_label(&mention), false),
            ];
            if mode == "schedule" {
                fields.insert(2, field("Delivery", session.display.clone().unwrap_or_else(|| "Unknown".into()), false));
            }
            if mass {
                let who = if mention.everyone { "@everyone" } else { "@here" };
                fields.push(field("Warning", format!("This will notify {who}."), false));
            }
            let manual = mode == "manual";
            let confirm = if manual { format!("embedui:manualconfirm:{token}") } else { format!("embedui:scheduleconfirm:{token}") };
            let embed = CreateEmbed::new()
                .color(if mass { 0xed4245 } else { COLOR })
                .title(if manual { "Send Embed?" } else { "Schedule Embed?" })
                .fields(fields);
            let row = CreateActionRow::Buttons(vec![
                button(confirm, if manual { "Send" } else { "Confirm Schedule" }, if mass { ButtonStyle::Danger } else { ButtonStyle::Success }),
                button(format!("embedui:mentionback:{mode}:{token}"), "Back", ButtonStyle::Secondary),
                button(format!("embedui:mentioncancel:{mode}:{token}"), "Cancel", ButtonStyle::Secondary),
            ]);
            return self.update(i, CreateInteractionResponseMessage::new().embed(embed).components(vec![row])).await;
        }
        if i.is_button() && action == "mentionback" {
            let (mode, token) = (piece(2), piece(3));
            let Some(session) = self.owned_session(mode, token, user.id, guild_id) else {
                self.reply(i, expired).await;
                return Ok(());
            };
            let mention = session.mention.unwrap_or_default();
            return self.update(i, self.mention_message(mode, token, &mention, guild_id)).await;
        }
        if i.is_button() && action == "mentioncancel" {
            let (mode, token) = (piece(2), piece(3));
            if self.owned_session(mode, token, user.id, guild_id).is_none() {
                self.reply(i, expired).await;
                return Ok(());
            }
            self.sessions(mode).remove(token);
            let embed = CreateEmbed::new().color(0xfee75c).title("Cancelled").description("No embed was sent or scheduled.");
            return self.update(i, CreateInteractionResponseMessage::new().embed(embed).components(vec![])).await;
        }
        if i.is_string_select() && action == "attachmentremove" {
            let Some(template) = self.repo.embed_by_id(&gid, token_or_guild)? else {
                self.reply(i, "That template no longer exists.").await;
                return Ok(());
            };
            let filename = i.values().into_iter().next().unwrap_or_default();
            if template.payload_json.contains(&format!("attachment://{filename}")) {
                self.reply(
                    i,
                    &format!("Attachment {filename} is still referenced by this template. Edit the JSON first or replace the attachment."),
                )
                .await;
                return Ok(());
            }
            self.repo.delete_attachment(&gid, &template.template_id, &filename)?;
            return self
                .update(
                    i,
                    CreateInteractionResponseMessage::new()
                        .content(format!("Removed attachment **{filename}** from **{}**.", template.name))
                        .components(vec![]),
                )
                .await;
        }
        if i.is_button() && action == "cancelschedule" {
            let schedule = self.repo.schedule(&gid, token_or_guild)?.filter(|s| s.status == "scheduled");
            let Some(schedule) = schedule else {
                self.reply(i, "That pending schedule is unavailable.").await;
                return Ok(());
            };
            let content = format!(
                "Cancel scheduled send?\n\nTemplate: **{}**\nChannel: <#{}>\nTime: <t:{}:F>",
                schedule.template_name_snapshot,
                schedule.channel_id,
                unix_seconds(&schedule.scheduled_for_utc)
            );
            let row = CreateActionRow::Buttons(vec![
                button(format!("embedui:cancelconfirm:{}", schedule.id), "Confirm Cancel", ButtonStyle::Danger),
                button(format!("embedui:cancelback:{}", schedule.id), "Keep Schedule", ButtonStyle::Secondary),
            ]);
            return self.update(i, CreateInteractionResponseMessage::new().content(content).components(vec![row])).await;
        }
        if i.is_button() && action == "cancelconfirm" {
            let schedule = self.repo.schedule(&gid, token_or_guild)?;
            let cancelled = self.repo.cancel_schedule(token_or_guild, &gid, &uid)?;
            if let (true, Some(schedule)) = (cancelled, &schedule) {
                self.audit.record(AuditEvent {
                    guild: subject(&gid, self.guild_name(guild_id)),
                    channel: subject(&schedule.channel_id, None),
                    actor: subject(&uid, Some(user.tag())),
                    details: details(&[("Template", schedule.template_name_snapshot.clone()), ("Schedule", schedule.id.clone())]),
                    ..audit_event("Embed Schedule Cancelled", "Embeds", "warning")
                });
            }
            let content = if cancelled { "Schedule cancelled. History is retained." } else { "Schedule was already changed or is unavailable." };
            return self.update(i, CreateInteractionResponseMessage::new().content(content).components(vec![])).await;
        }
        if i.is_button() && action == "cancelback" {
            return self.update(i, CreateInteractionResponseMessage::new().content("Schedule kept.").components(vec![])).await;
        }
        if i.is_button() && action == "schedule" {
            let row = CreateActionRow::Buttons(vec![
                button(format!("embedui:schedulecreate:{gid}"), "Schedule Send", ButtonStyle::Success),
                button(format!("embedui:upcoming:{gid}"), "View Upcoming", ButtonStyle::Primary),
                button(format!("embedui:history:{gid}"), "View History", ButtonStyle::Secondary),
            ]);
            return self
                .send(i, ephemeral().content("Scheduled sends use the latest saved template at delivery time.").components(vec![row]))
                .await;
        }
        if i.is_button() && (action == "upcoming" || action == "history") {
            let upcoming = action == "upcoming";
            let schedules = if upcoming { self.repo.upcoming_schedules(&gid, None)? } else { self.repo.schedule_history(&gid, None)? };
            let fields: Vec<_> = schedules.iter().take(10).map(schedule_field).collect();
            let mut embed = CreateEmbed::new()
                .color(if upcoming { 0xfee75c } else { COLOR })
                .title(if upcoming { "Scheduled Sends" } else { "Schedule History" });
            if fields.is_empty() {
                embed = embed.description(format!("No {} scheduled sends.", if upcoming { "upcoming" } else { "historical" }));
            }
            embed = embed.fields(fields);
            let components = if upcoming && !schedules.is_empty() {
                vec![CreateActionRow::Buttons(
                    schedules
                        .iter()
                        .take(5)
                        .map(|s| {
                            let label: String = format!("Cancel {}", s.template_name_snapshot).chars().take(80).collect();
                            button(format!("embedui:cancelschedule:{}", s.id), label, ButtonStyle::Danger)
                        })
                        .collect(),
                )]
            } else {
                vec![]
            };
            return self.send(i, ephemeral().embed(embed).components(components)).await;
        }
        if i.is_button() && action == "schedulecreate" {
            let names = self.repo.embeds(&gid, Some(0), Some(25))?;
            if names.is_empty() {
                return self.send(i, ephemeral().content("Save an embed before scheduling it.")).await;
            }
            let token = session_token(5);
            self.embed_sessions.insert(token.clone(), &uid, Session { guild_id: gid.clone(), user_id: uid.clone(), ..Default::default() });
            let row = string_select(format!("embedui:select:schedule:{token}"), "Select an embed", name_options(&names));
            return self.send(i, ephemeral().content("Choose a template to schedule.").components(vec![row])).await;
        }
        if i.is_button() && matches!(action, "edit" | "send" | "json") {
            let names = self.repo.embeds(&gid, Some(0), Some(25))?;
            if names.is_empty() {
                return self.send(i, ephemeral().content("No embeds have been saved in this server.")).await;
            }
            let token = session_token(5);
            self.embed_sessions.insert(token.clone(), &uid, Session { guild_id: gid.clone(), user_id: uid.clone(), ..Default::default() });
            let row = string_select(format!("embedui:select:{action}:{token}"), "Select an embed", name_options(&names));
            return self.send(i, ephemeral().content(format!("Select an embed to {action}.")).components(vec![row])).await;
        }
        if i.is_button() && action == "list" {
            let names = self.repo.embeds(&gid, Some(0), Some(25))?;
            let content = if names.is_empty() {
                "No embeds have been saved in this server.".to_string()
            } else {
                format!("**Embeds**\n{}", names.iter().map(|n| format!("• {n}")).collect::<Vec<_>>().join("\n"))
            };
            return self.send(i, ephemeral().content(content)).await;
        }
        if i.is_button() && action == "attachments" {
            let names = self.repo.embeds(&gid, Some(0), Some(25))?;
            if names.is_empty() {
                self.reply(i, "Create an embed before adding attachments.").await;
                return Ok(());
            }
            let row = string_select(format!("embedui:attachmenttemplate:{gid}"), "Select an embed", name_options(&names));
            return self
                .send(i, ephemeral().content("Select the saved embed whose attachments you want to manage.").components(vec![row]))
                .await;
        }
        if i.is_string_select() && action == "attachmenttemplate" {
            let name = i.values().into_iter().next().unwrap_or_default();
            let Some(template) = self.repo.embed(&gid, &name)? else {
                self.reply(i, "That embed no longer exists.").await;
                return Ok(());
            };
            let attachments = self.repo.attachments(&gid, &template.template_id)?;
            let content = if attachments.is_empty() {
                format!("**{}** has no stored attachments.", template.name)
            } else {
                format!(
                    "**{} attachments**\n{}",
                    template.name,
                    attachments.iter().map(|a| format!("• {} — {} KB", a.filename, kb(a.size_bytes))).collect::<Vec<_>>().join("\n")
                )
            };
            let mut rows = vec![CreateActionRow::Buttons(vec![button(
                format!("embedui:attachmentupload:{}", template.template_id),
                "Upload or Replace",
                ButtonStyle::Primary,
            )])];
            if !attachments.is_empty() {
                let options = attachments
                    .iter()
                    .take(25)
                    .map(|a| CreateSelectMenuOption::new(&a.filename, &a.filename).description(format!("{} KB", kb(a.size_bytes))))
                    .collect();
                rows.push(string_select(format!("embedui:attachmentremove:{}", template.template_id), "Remove an attachment", options));
            }
            return self.update(i, CreateInteractionResponseMessage::new().content(content).components(rows)).await;
        }
        if i.is_button() && action == "attachmentupload" {
            // serenity 0.12 has no Label/FileUpload builders: send the modal as raw JSON.
            let modal = json!({
                "type": 9,
                "data": {
                    "custom_id": format!("embedui:attachmentupload:{token_or_guild}"),
                    "title": "Upload embed attachment",
                    "components": [{
                        "type": 18,
                        "label": "Attachment",
                        "description": "Uploading a same-named file replaces it.",
                        "component": { "type": 19, "custom_id": "attachment", "min_values": 1, "max_values": 1, "required": true }
                    }]
                }
            });
            self.discord.http.create_interaction_response(i.id(), interaction_token(i), &modal, vec![]).await?;
            return Ok(());
        }
        if let (true, "attachmentupload", Ix::Modal(modal)) = (i.is_modal(), action, i) {
            let Some(template) = self.repo.embed_by_id(&gid, token_or_guild)? else {
                self.reply(i, "That embed no longer exists.").await;
                return Ok(());
            };
            // serenity drops the Label/FileUpload payload while deserializing; recover it from the
            // serialized interaction (works once serenity keeps the raw component data).
            let upload = serde_json::to_value(modal).ok().and_then(|raw| parse_file_upload(&raw, "attachment"));
            let Some(upload) = upload else {
                self.reply(i, "No attachment was received.").await;
                return Ok(());
            };
            return self.store_upload(i, &gid, &template.template_id, &template.name, upload).await;
        }
        if i.is_button() && action == "create" {
            let input = CreateInputText::new(InputTextStyle::Short, "Embed name", "name").required(true).max_length(64);
            i.respond(&self.discord, text_modal(format!("embedui:name:create:{gid}"), "Create Embed", vec![input])).await?;
            return Ok(());
        }
        if i.is_modal() && action == "name" {
            let raw = i.text("name");
            if raw.trim().is_empty() {
                self.reply(i, "Provide an embed name.").await;
                return Ok(());
            }
            let Some(name) = template_name(&raw) else {
                self.reply(i, INVALID_NAME).await;
                return Ok(());
            };
            if self.repo.embed(&gid, &name)?.is_some() {
                self.reply(i, "That embed already exists in this server.").await;
                return Ok(());
            }
            let embed = CreateEmbed::new().color(COLOR).title("Create Embed").description(format!(
                "**{name}** will be saved for this server only.\n\nEnter Discord-compatible message JSON next. Use the large-payload option only when the JSON exceeds one modal field."
            ));
            let encoded = encode_uri_component(&name);
            let row = CreateActionRow::Buttons(vec![
                button(format!("embedflow:input:create:{encoded}"), "Enter JSON", ButtonStyle::Primary),
                button(format!("embedflow:large:create:{encoded}"), "Large JSON", ButtonStyle::Secondary),
            ]);
            return self.send(i, ephemeral().embed(embed).components(vec![row])).await;
        }
        if i.is_string_select() && action == "select" {
            let (mode, token) = (token_or_guild, piece(3));
            let name = i.values().into_iter().next().unwrap_or_default();
            let session = self.embed_sessions.get(token);
            if !session.is_some_and(|s| s.user_id == uid && s.guild_id == gid) {
                self.reply(i, "That selection expired.").await;
                return Ok(());
            }
            self.embed_sessions.remove(token);
            let Some(stored) = self.repo.embed(&gid, &name)? else {
                self.reply(i, "That embed no longer exists.").await;
                return Ok(());
            };
            if mode == "schedule" {
                let schedule_token = session_token(5);
                self.schedule_sessions.insert(
                    schedule_token.clone(),
                    &uid,
                    Session {
                        guild_id: gid.clone(),
                        user_id: uid.clone(),
                        name: Some(stored.name.clone()),
                        template_id: Some(stored.template_id.clone()),
                        ..Default::default()
                    },
                );
                return self
                    .update(
                        i,
                        CreateInteractionResponseMessage::new()
                            .content(format!("Select a destination for scheduled **{name}**."))
                            .components(vec![text_channel_select(format!("embedui:channel:schedule:{schedule_token}"))]),
                    )
                    .await;
            }
            if mode == "edit" {
                let row = CreateActionRow::Buttons(vec![button(
                    format!("embedflow:input:edit:{}", encode_uri_component(&name)),
                    "Edit JSON",
                    ButtonStyle::Primary,
                )]);
                return self
                    .update(i, CreateInteractionResponseMessage::new().content(format!("Editing embed **{name}**.")).components(vec![row]))
                    .await;
            }
            if mode == "json" {
                let message = if stored.payload_json.encode_utf16().count() < 1900 {
                    CreateMessage::new().content(format!("**{}**\n```json\n{}\n```", stored.name, stored.payload_json))
                } else {
                    CreateMessage::new()
                        .content(format!("**{}**", stored.name))
                        .add_file(CreateAttachment::bytes(stored.payload_json.clone().into_bytes(), format!("{}.json", stored.name)))
                };
                let content = match user.direct_message((&self.discord.cache, self.discord.http.as_ref()), message).await {
                    Ok(_) => "I sent the exact JSON to your DMs.",
                    Err(_) => "Gaia could not DM you. Enable DMs from this server and try again.",
                };
                return self.update(i, CreateInteractionResponseMessage::new().content(content).components(vec![])).await;
            }
            let send_token = session_token(5);
            self.embed_sessions.insert(
                send_token.clone(),
                &uid,
                Session { guild_id: gid.clone(), user_id: uid.clone(), name: Some(name.clone()), ..Default::default() },
            );
            return self
                .update(
                    i,
                    CreateInteractionResponseMessage::new()
                        .content(format!("Select a destination for **{name}**."))
                        .components(vec![text_channel_select(format!("embedui:channel:send:{send_token}"))]),
                )
                .await;
        }
        if i.is_channel_select() && action == "channel" && token_or_guild == "schedule" {
            let token = piece(3);
            let channel = i.values().first().and_then(|id| self.text_channel(guild_id, id));
            let claimed = channel.is_some_and(|channel| {
                self.schedule_sessions
                    .update(token, |s| {
                        let owned = s.user_id == uid && s.guild_id == gid;
                        if owned {
                            s.channel_id = Some(channel.id.to_string());
                        }
                        owned
                    })
                    .unwrap_or(false)
            });
            if !claimed {
                self.reply(i, "That schedule session expired or the channel is unavailable.").await;
                return Ok(());
            }
            let input = CreateInputText::new(InputTextStyle::Short, "YYYY-MM-DD HH:mm (Europe/Ljubljana)", "time")
                .placeholder("2026-08-30 18:30")
                .required(true)
                .max_length(32);
            i.respond(&self.discord, text_modal(format!("embedui:time:{token}"), "Schedule delivery", vec![input])).await?;
            return Ok(());
        }
        if i.is_modal() && action == "time" {
            let session = self.schedule_sessions.get(token_or_guild);
            if !session.is_some_and(|s| s.user_id == uid && s.guild_id == gid && s.channel_id.is_some()) {
                self.reply(i, "That schedule session expired.").await;
                return Ok(());
            }
            match parse_ljubljana_schedule(&i.text("time"), chrono::Utc::now()) {
                Ok(parsed) => {
                    self.schedule_sessions.update(token_or_guild, |session| {
                        session.utc = Some(parsed.utc.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
                        session.display = Some(parsed.display);
                        session.mention = Some(DeliveryMention::default());
                    });
                    let message = self.mention_message("schedule", token_or_guild, &DeliveryMention::default(), guild_id);
                    return self.send(i, message.ephemeral(true)).await;
                }
                Err(error) => {
                    self.reply(i, &error).await;
                    return Ok(());
                }
            }
        }
        if i.is_button() && action == "schedulecancel" {
            if self.owned_session("schedule", token_or_guild, user.id, guild_id).is_none() {
                self.reply(i, "That schedule session expired.").await;
                return Ok(());
            }
            self.schedule_sessions.remove(token_or_guild);
            return self.update(i, CreateInteractionResponseMessage::new().content("Schedule cancelled.").components(vec![])).await;
        }
        if i.is_button() && action == "scheduleconfirm" {
            let session = self.schedule_sessions.get(token_or_guild);
            let Some(session) =
                session.filter(|s| s.user_id == uid && s.guild_id == gid && s.channel_id.is_some() && s.utc.is_some())
            else {
                self.reply(i, "That schedule session expired.").await;
                return Ok(());
            };
            let channel_id = session.channel_id.clone().unwrap_or_default();
            let Some(channel) = self.text_channel(guild_id, &channel_id) else {
                self.reply(i, "Destination channel is unavailable.").await;
                return Ok(());
            };
            let stamp = crate::db::now();
            let mention = session.mention.clone().unwrap_or_default();
            let utc = session.utc.clone().unwrap_or_default();
            let name = session.name.clone().unwrap_or_default();
            self.repo.create_schedule(&ScheduledEmbedSend {
                id: uuid::Uuid::new_v4().to_string(),
                template_id: session.template_id.clone().unwrap_or_default(),
                guild_id: gid.clone(),
                channel_id: channel_id.clone(),
                scheduled_for_utc: utc.clone(),
                timezone: "Europe/Ljubljana".into(),
                created_by_user_id: uid.clone(),
                created_at: stamp.clone(),
                updated_by_user_id: uid.clone(),
                updated_at: stamp,
                status: "scheduled".into(),
                sent_at: None,
                sent_message_id: None,
                failure_code: None,
                failure_reason: None,
                attempt_count: 0,
                last_attempt_at: None,
                processing_started_at: None,
                template_name_snapshot: name.clone(),
                mention_json: Some(serde_json::to_string(&mention)?),
                mention_outcome: None,
            })?;
            let delivery = session.display.clone().unwrap_or_else(|| utc.clone());
            self.audit.record(AuditEvent {
                guild: subject(&gid, self.guild_name(guild_id)),
                channel: subject(&channel_id, Some(channel.name.clone())),
                actor: subject(&uid, Some(user.tag())),
                details: details(&[("Template", name.clone()), ("Delivery", delivery.clone())]),
                ..audit_event("Embed Scheduled", "Embeds", "info")
            });
            self.schedule_sessions.remove(token_or_guild);
            let embed = CreateEmbed::new().color(0x57f287).title("Embed Scheduled").fields(vec![
                field("Embed", name, true),
                field("Destination", format!("<#{channel_id}>"), true),
                field("Delivery", delivery, false),
                field("Mentions", mention_label(&mention), false),
            ]);
            return self.update(i, CreateInteractionResponseMessage::new().embed(embed).components(vec![])).await;
        }
        if i.is_button() && action == "manualconfirm" {
            let session = self.embed_sessions.get(token_or_guild);
            let Some(session) =
                session.filter(|s| s.user_id == uid && s.guild_id == gid && s.name.is_some() && s.channel_id.is_some())
            else {
                self.reply(i, "That send session expired.").await;
                return Ok(());
            };
            let channel = self.text_channel(guild_id, session.channel_id.as_deref().unwrap_or_default());
            let stored = self.repo.embed(&gid, session.name.as_deref().unwrap_or_default())?;
            let (Some(stored), Some(channel)) = (stored, channel) else {
                self.reply(i, "Embed or destination channel is unavailable.").await;
                return Ok(());
            };
            let requested = session.mention.clone().unwrap_or_default();
            let resolved = match self.discord.cache.guild(guild_id) {
                Some(guild) => resolve_mention(&guild, self.bot_id(), &requested),
                None => resolve_mention_with(|_| false, false, &requested),
            };
            let rendered = render_template(&self.repo, &gid, &stored.template_id, &stored.payload_json).await?;
            let message =
                channel.id.send_message(&self.discord.http, with_delivery_mention(rendered, &resolved.mention).create_message()).await?;
            self.repo.track_sent_embed(&stored.template_id, &gid, &channel.id.to_string(), &message.id.to_string())?;
            self.audit.record(AuditEvent {
                guild: subject(&gid, self.guild_name(guild_id)),
                channel: subject(channel.id, Some(channel.name.clone())),
                actor: subject(&uid, Some(user.tag())),
                details: details(&[("Template", stored.name.clone())]),
                ..audit_event("Embed Manually Sent", "Embeds", "success")
            });
            self.embed_sessions.remove(token_or_guild);
            let mut fields = vec![
                field("Embed", stored.name.clone(), true),
                field("Destination", format!("<#{}>", channel.id), true),
                field("Mentions", mention_label(&resolved.mention), false),
            ];
            if let Some(warning) = &resolved.warning {
                fields.push(field("Warning", warning.clone(), false));
            }
            let embed = CreateEmbed::new().color(0x57f287).title("Embed Sent").fields(fields);
            return self.update(i, CreateInteractionResponseMessage::new().embed(embed).components(vec![])).await;
        }
        if i.is_channel_select() && action == "channel" {
            let token = piece(3);
            let channel = i.values().first().and_then(|id| self.text_channel(guild_id, id));
            let claimed = channel.is_some_and(|channel| {
                self.embed_sessions
                    .update(token, |s| {
                        let owned = s.user_id == uid && s.guild_id == gid && s.name.is_some();
                        if owned {
                            s.channel_id = Some(channel.id.to_string());
                            s.mention = Some(DeliveryMention::default());
                        }
                        owned
                    })
                    .unwrap_or(false)
            });
            if !claimed {
                self.reply(i, "That send session expired or the channel is unavailable.").await;
                return Ok(());
            }
            return self.update(i, self.mention_message("manual", token, &DeliveryMention::default(), guild_id)).await;
        }
        Ok(())
    }

    /// Downloads a modal upload and stores it on the template (TS attachment upload branch).
    async fn store_upload(&self, i: Ix<'_>, guild_id: &str, template_id: &str, template_name: &str, upload: FileUpload) -> anyhow::Result<()> {
        if upload.size > MAX_ATTACHMENT_BYTES as u64 {
            self.reply(i, &format!("Attachment exceeds the {} MB limit.", MAX_ATTACHMENT_BYTES / 1024 / 1024)).await;
            return Ok(());
        }
        let stored: anyhow::Result<()> = async {
            let data = download_attachment(&upload.url, MAX_ATTACHMENT_BYTES, is_discord_cdn).await?;
            if data.len() as u64 != upload.size {
                anyhow::bail!("Attachment size changed during upload.");
            }
            let record = attachment_record(
                template_id,
                &upload.name,
                upload.content_type.as_deref().unwrap_or("application/octet-stream"),
                data,
                &i.user().id.to_string(),
            )?;
            self.repo.save_attachment(guild_id, &record, MAX_TEMPLATE_ATTACHMENTS, MAX_TEMPLATE_ATTACHMENT_BYTES)?;
            Ok(())
        }
        .await;
        match stored {
            Ok(()) => {
                self.send(
                    i,
                    ephemeral().content(format!(
                        "Stored attachment **{}** on **{template_name}**. Use `attachment://{}` in embed image or thumbnail URLs.",
                        upload.name, upload.name
                    )),
                )
                .await
            }
            Err(error) => {
                self.reply(i, &error.to_string()).await;
                Ok(())
            }
        }
    }

    async fn handle_embed_flow(&self, i: &ComponentInteraction) -> anyhow::Result<()> {
        let ix = Ix::Component(i);
        let pieces: Vec<&str> = i.data.custom_id.split(':').collect();
        let piece = |n: usize| pieces.get(n).copied().unwrap_or("");
        let (action, mode, raw) = (piece(1), piece(2), piece(3));
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        if !self.is_embed_admin(guild_id, i.user.id).await {
            self.reply(ix, "You do not have permission to edit embeds.").await;
            return Ok(());
        }
        let (gid, uid) = (guild_id.to_string(), i.user.id.to_string());
        if action == "input" || action == "large" {
            let Some(name) = decode_uri_component(raw).ok().and_then(|n| template_name(&n)) else {
                self.reply(ix, INVALID_NAME).await;
                return Ok(());
            };
            let existing = self.repo.embed(&gid, &name)?;
            if action == "large" {
                let inputs = (0..5)
                    .map(|n| {
                        CreateInputText::new(InputTextStyle::Paragraph, format!("JSON part {} of 5", n + 1), format!("json{n}"))
                            .required(n == 0)
                            .max_length(4000)
                    })
                    .collect();
                ix.respond(&self.discord, text_modal(format!("embedmodal:large-{mode}:{}", encode_uri_component(&name)), "Large Embed JSON", inputs))
                    .await?;
                return Ok(());
            }
            let mut input = CreateInputText::new(InputTextStyle::Paragraph, "Discord-compatible message JSON", "json")
                .required(true)
                .max_length(4000);
            if let Some(existing) = existing {
                let length = existing.payload_json.encode_utf16().count();
                if length > 0 && length <= 4000 {
                    input = input.value(existing.payload_json);
                }
            }
            ix.respond(&self.discord, text_modal(format!("embedmodal:{mode}:{}", encode_uri_component(&name)), "Enter Embed JSON", vec![input]))
                .await?;
            return Ok(());
        }
        if action == "save" {
            let pending = self.pending_embeds.get(mode);
            let Some(pending) = pending.filter(|p| p.user_id == uid && p.guild_id == gid) else {
                self.reply(ix, "That preview expired. Please start again.").await;
                return Ok(());
            };
            if template_name(&pending.name).as_deref() != Some(pending.name.as_str()) {
                self.reply(ix, INVALID_NAME).await;
                return Ok(());
            }
            if !self.repo.save_embed(&gid, &pending.name, &pending.json, &uid, Some(pending.replace))? {
                return self
                    .update(ix, CreateInteractionResponseMessage::new().content("That embed name already exists.").embeds(vec![]).components(vec![]))
                    .await;
            }
            let tracked = match self.repo.embed(&gid, &pending.name)? {
                Some(stored) => self.repo.sent_embeds(&stored.template_id, &gid)?,
                None => vec![],
            };
            self.audit.record(AuditEvent {
                guild: subject(&gid, self.guild_name(guild_id)),
                actor: subject(&uid, Some(i.user.tag())),
                details: details(&[("Template", pending.name.clone())]),
                ..audit_event(if pending.replace { "Embed Edited" } else { "Embed Created" }, "Embeds", "success")
            });
            let components = if tracked.is_empty() {
                vec![]
            } else {
                vec![CreateActionRow::Buttons(vec![button(
                    format!("embedflow:update:{mode}"),
                    if tracked.len() == 1 { "Update Sent Embed" } else { "Update Sent Embeds" },
                    ButtonStyle::Primary,
                )])]
            };
            return self
                .update(
                    ix,
                    CreateInteractionResponseMessage::new()
                        .content(format!("Embed **{}** saved.", pending.name))
                        .embeds(vec![])
                        .components(components),
                )
                .await;
        }
        if action == "update" {
            let pending = self.pending_embeds.get(mode);
            let Some(pending) = pending.filter(|p| p.user_id == uid && p.guild_id == gid) else {
                self.reply(ix, "That update session expired.").await;
                return Ok(());
            };
            let Some(stored) = self.repo.embed(&gid, &pending.name)? else {
                self.reply(ix, "That embed no longer exists.").await;
                return Ok(());
            };
            let (mut updated, mut stale) = (0, 0);
            let bot_id = self.bot_id();
            for sent in self.repo.sent_embeds(&stored.template_id, &gid)? {
                let Some(channel) = self.text_channel(guild_id, &sent.channel_id) else {
                    self.repo.remove_sent_embed(sent.id)?;
                    stale += 1;
                    continue;
                };
                let context = || {
                    ctx! {"service" => "embed-manager", "guildId" => gid.clone(), "channelId" => sent.channel_id.clone(),
                    "messageId" => sent.message_id.clone(), "interactionId" => i.id.to_string(), "customId" => i.data.custom_id.clone()}
                };
                let message_id = MessageId::new(snowflake(&sent.message_id).unwrap_or(1));
                let message = match channel.id.message((&self.discord.cache, self.discord.http.as_ref()), message_id).await {
                    Ok(message) => Some(message),
                    Err(error) => {
                        log_discord_error("embed.tracked.fetch", &error.into(), context());
                        None
                    }
                };
                if !message.is_some_and(|m: Message| m.author.id == bot_id) {
                    self.repo.remove_sent_embed(sent.id)?;
                    stale += 1;
                    continue;
                }
                let edited: anyhow::Result<()> = async {
                    let rendered = render_template(&self.repo, &gid, &stored.template_id, &stored.payload_json).await?;
                    let edit = edit_from_rendered(with_delivery_mention(rendered, &DeliveryMention::default()));
                    channel.id.edit_message(&self.discord.http, message_id, edit).await?;
                    Ok(())
                }
                .await;
                match edited {
                    Ok(()) => updated += 1,
                    Err(error) => {
                        stale += 1;
                        self.repo.remove_sent_embed(sent.id)?;
                        log_discord_error("embed.tracked.edit", &error, context());
                    }
                }
            }
            self.pending_embeds.remove(mode);
            let mut content = format!("🟢 Updated {updated} sent embed{}.", if updated == 1 { "" } else { "s" });
            if stale > 0 {
                content.push_str(&format!(
                    "\n🟡 {stale} tracked message{} unavailable.",
                    if stale == 1 { " was" } else { "s were" }
                ));
            }
            return self.update(ix, CreateInteractionResponseMessage::new().content(content).components(vec![])).await;
        }
        if action == "cancel" {
            if self.pending_embeds.get(mode).is_some_and(|p| p.user_id == uid && p.guild_id == gid) {
                self.pending_embeds.remove(mode);
            }
            return self
                .update(ix, CreateInteractionResponseMessage::new().content("Embed changes cancelled.").embeds(vec![]).components(vec![]))
                .await;
        }
        Ok(())
    }

    async fn handle_embed_modal(&self, i: &ModalInteraction) -> anyhow::Result<()> {
        let ix = Ix::Modal(i);
        let pieces: Vec<&str> = i.data.custom_id.split(':').collect();
        let mode = pieces.get(1).copied().unwrap_or("");
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        if !self.is_embed_admin(guild_id, i.user.id).await {
            self.reply(ix, "You need Discord Administrator permission to manage embeds.").await;
            return Ok(());
        }
        let Some(name) = decode_uri_component(pieces.get(2).copied().unwrap_or("")).ok().and_then(|n| template_name(&n)) else {
            self.reply(ix, INVALID_NAME).await;
            return Ok(());
        };
        let large = mode.starts_with("large-");
        let json = if large { (0..5).map(|n| ix.text(&format!("json{n}"))).collect::<String>() } else { ix.text("json") };
        let replace = if large { &mode[6..] == "edit" } else { mode == "edit" };
        self.preview_embed(i, name, json, replace).await
    }

    async fn preview_embed(&self, i: &ModalInteraction, name: String, json: String, replace: bool) -> anyhow::Result<()> {
        let ix = Ix::Modal(i);
        let value = match validate_embed_payload(&json) {
            Ok(value) => value,
            Err(errors) => {
                let text = format!("Invalid embed JSON:\n{}", errors.iter().map(|e| format!("• {e}")).collect::<Vec<_>>().join("\n"));
                let text: String = text.chars().take(1900).collect();
                self.reply(ix, &text).await;
                return Ok(());
            }
        };
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let token = session_token(6);
        let user_id = i.user.id.to_string();
        self.pending_embeds.insert(
            token.clone(),
            &user_id,
            PendingEmbed { name, json, replace, user_id: user_id.clone(), guild_id: guild_id.to_string() },
        );
        let row = CreateActionRow::Buttons(vec![
            button(format!("embedflow:save:{token}"), if replace { "Save Changes" } else { "Save Embed" }, ButtonStyle::Success),
            button(format!("embedflow:cancel:{token}"), "Cancel", ButtonStyle::Secondary),
        ]);
        // Raw JSON keeps the validated embeds byte-for-byte instead of round-tripping them through builders.
        let content = match value.get("content") {
            None | Some(Value::Null) => json!("Preview — this template is not saved until you confirm."),
            Some(content) => content.clone(),
        };
        let mut data = json!({ "content": content, "components": [row], "flags": 64 });
        if let Some(embeds) = value.get("embeds") {
            data["embeds"] = embeds.as_array().map(|list| list.iter().map(with_default_color).collect()).unwrap_or_else(|| embeds.clone());
        }
        self.discord.http.create_interaction_response(i.id, &i.token, &json!({ "type": 4, "data": data }), vec![]).await?;
        Ok(())
    }

    async fn handle_autocomplete(&self, i: &CommandInteraction) -> anyhow::Result<()> {
        if i.data.name != "embed" {
            return Ok(());
        }
        let focused = i.data.autocomplete().map(|o| o.value.to_lowercase()).unwrap_or_default();
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let mut response = CreateAutocompleteResponse::new();
        for name in self.repo.embeds(&guild_id.to_string(), Some(0), Some(25))?.into_iter().filter(|n| n.to_lowercase().contains(&focused)) {
            response = response.add_string_choice(name.clone(), name);
        }
        i.create_response(&self.discord.http, CreateInteractionResponse::Autocomplete(response)).await?;
        Ok(())
    }
}

fn interaction_token<'a>(i: Ix<'a>) -> &'a str {
    match i {
        Ix::Command(i) => &i.token,
        Ix::Component(i) => &i.token,
        Ix::Modal(i) => &i.token,
    }
}

fn schedule_field(schedule: &ScheduledEmbedSend) -> (String, String, bool) {
    let mut value = format!(
        "Channel: <#{}>\nDelivery: <t:{}:F>\nMentions: {}",
        schedule.channel_id,
        unix_seconds(&schedule.scheduled_for_utc),
        mention_label(&parse_mention(schedule.mention_json.as_deref()))
    );
    if let Some(outcome) = schedule.mention_outcome.as_deref().filter(|o| !o.is_empty()) {
        value.push_str(&format!("\nOutcome: {outcome}"));
    }
    (format!("{} — {}", schedule.template_name_snapshot, schedule.status), value, false)
}

/// discord.js `message.edit(rendered)`: content only when present; new files replace the old attachments.
fn edit_from_rendered(rendered: RenderedMessage) -> EditMessage {
    let mut edit = EditMessage::new().embeds(rendered.embeds);
    if let Some(content) = rendered.content {
        edit = edit.content(content);
    }
    if let Some(allowed) = rendered.allowed_mentions {
        edit = edit.allowed_mentions(allowed);
    }
    if !rendered.files.is_empty() {
        edit = edit.attachments(rendered.files.into_iter().fold(EditAttachments::new(), EditAttachments::add));
    }
    edit
}

/// serenity event handler; the bot is installed after the client (and thus its Http/Cache) exists.
pub struct Handler {
    pub bot: Arc<OnceLock<Arc<GaiaBot>>>,
}

impl Handler {
    fn bot(&self) -> Option<Arc<GaiaBot>> {
        self.bot.get().cloned()
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, _ready: Ready) {
        let Some(bot) = self.bot() else { return };
        if bot.ready_once.swap(true, Ordering::SeqCst) {
            return;
        }
        // discord.js fires clientReady once every guild has arrived (or after 15s); mirror that.
        let deadline = Instant::now() + Duration::from_secs(15);
        while ctx.cache.unavailable_guilds().len() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if let Err(error) = bot.on_ready().await {
            let message = error.to_string();
            logger::error("Gaia startup failed", ctx! {"error" => message.clone()});
            bot.audit.record(AuditEvent {
                priority: "critical".into(),
                reason: Some(message),
                ..audit_event("Gaia Startup Failed", "Bot", "danger")
            });
            bot.startup_failed.notify_one();
        }
    }

    async fn guild_create(&self, _ctx: Context, guild: serenity::all::Guild, is_new: Option<bool>) {
        // discord.js only emits guildCreate for joins after ready; initial guild loads are not "new".
        let (Some(bot), Some(true)) = (self.bot(), is_new) else { return };
        if let Err(error) = bot.on_guild_create(guild.id, guild.name.clone()).await {
            logger::error("guild initialization failed", ctx! {"guildId" => guild.id.to_string(), "error" => error.to_string()});
        }
    }

    async fn guild_delete(&self, _ctx: Context, incomplete: serenity::all::UnavailableGuild, full: Option<serenity::all::Guild>) {
        // `unavailable` means a Discord outage, not removal (discord.js emits guildUnavailable instead).
        let Some(bot) = self.bot() else { return };
        if incomplete.unavailable {
            return;
        }
        bot.on_guild_delete(incomplete.id, full.map(|g| g.name));
    }

    async fn guild_member_addition(&self, _ctx: Context, member: Member) {
        let Some(bot) = self.bot() else { return };
        bot.member_log.record_join(&member, &bot.guild_name(member.guild_id).unwrap_or_default(), None);
        if let Err(error) = bot.on_member_add(&member).await {
            logger::error(
                "member join handler failed",
                ctx! {"guildId" => member.guild_id.to_string(), "userId" => member.user.id.to_string(), "error" => error.to_string()},
            );
        }
    }

    async fn guild_member_removal(&self, _ctx: Context, guild_id: GuildId, user: User, member: Option<Member>) {
        let Some(bot) = self.bot() else { return };
        let _ = member;
        bot.member_log.record_leave(&user, guild_id, &bot.guild_name(guild_id).unwrap_or_default(), None);
    }

    async fn message(&self, _ctx: Context, message: Message) {
        let Some(bot) = self.bot() else { return };
        if let Err(error) = bot.verification.honeypot(&message).await {
            log_discord_error(
                "verification.honeypot",
                &error,
                ctx! {"guildId" => message.guild_id.map(|g| g.to_string()), "channelId" => message.channel_id.to_string(), "userId" => message.author.id.to_string()},
            );
        }
    }

    async fn voice_state_update(&self, _ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(bot) = self.bot() else { return };
        if bot.rooms.on_voice_state(old.as_ref(), &new).await.is_err() {
            logger::error("voice state handler failed", ctx! {"guildId" => new.guild_id.map(|g| g.to_string())});
        }
    }

    async fn channel_create(&self, _ctx: Context, channel: GuildChannel) {
        let Some(bot) = self.bot() else { return };
        if let Err(error) = bot.mutes.sync_new_channel(&channel).await {
            logger::error(
                "new channel mute sync failed",
                ctx! {"guildId" => channel.guild_id.to_string(), "channelId" => channel.id.to_string(), "error" => error.to_string()},
            );
        }
    }

    async fn channel_delete(&self, _ctx: Context, channel: GuildChannel, _messages: Option<Vec<Message>>) {
        let Some(bot) = self.bot() else { return };
        if let Err(error) = bot.on_channel_delete(channel.guild_id, channel.id).await {
            logger::error(
                "channel deletion handler failed",
                ctx! {"guildId" => channel.guild_id.to_string(), "channelId" => channel.id.to_string(), "error" => error.to_string()},
            );
        }
    }

    async fn guild_role_delete(&self, _ctx: Context, guild_id: GuildId, role_id: RoleId, _role: Option<Role>) {
        let Some(bot) = self.bot() else { return };
        if let Err(error) = bot.on_role_delete(guild_id, role_id).await {
            logger::error(
                "role deletion handler failed",
                ctx! {"guildId" => guild_id.to_string(), "roleId" => role_id.to_string(), "error" => error.to_string()},
            );
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Some(bot) = self.bot() else { return };
        bot.on_interaction(&ctx, &interaction).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_json<T: serde::Serialize>(value: &T) -> Value {
        serde_json::to_value(value).unwrap()
    }

    #[test]
    fn subscribes_to_guild_messages_without_message_content() {
        assert!(intents().contains(GatewayIntents::GUILD_MESSAGES));
        assert!(!intents().contains(GatewayIntents::MESSAGE_CONTENT));
    }

    #[test]
    fn keeps_the_embed_manager_button_rows_within_five_components() {
        let (_, rows) = embed_manager_panel("a");
        let counts: Vec<usize> = rows.iter().map(|r| to_json(r)["components"].as_array().unwrap().len()).collect();
        assert_eq!(counts, vec![5, 1]);
    }

    #[test]
    fn mention_settings_offers_role_picker_and_mass_toggles() {
        let mention = DeliveryMention { roles: vec!["r1".into(), "r2".into()], everyone: true, here: false };
        let (embed, rows) = mention_settings("manual", "token", &mention, false);
        let embed = to_json(&embed);
        assert_eq!(embed["title"], "Mention Settings");
        assert_eq!(embed["fields"][1]["value"], "<@&r1>, <@&r2>");
        assert_eq!(embed["fields"][2]["value"], "@everyone");
        assert!(embed["fields"][3]["value"].as_str().unwrap().contains("Gaia currently cannot"));
        let rows: Vec<Value> = rows.iter().map(to_json).collect();
        assert_eq!(rows[0]["components"][0]["custom_id"], "embedui:mentionroles:manual:token");
        assert_eq!(rows[0]["components"][0]["max_values"], 25);
        assert_eq!(rows[1]["components"][0]["style"], 4); // Danger while @everyone is on
        assert_eq!(rows[1]["components"][3]["custom_id"], "embedui:mentioncontinue:manual:token");
    }

    #[test]
    fn mass_mentions_are_exclusive_toggles() {
        let none = DeliveryMention::default();
        let everyone = toggle_mass(&none, "everyone");
        assert!(everyone.everyone && !everyone.here);
        let here = toggle_mass(&everyone, "here");
        assert!(!here.everyone && here.here);
        assert!(!toggle_mass(&here, "here").here);
    }

    #[test]
    fn uri_component_round_trips_names() {
        let name = "Weekly News: ä/ü 100%";
        let encoded = encode_uri_component(name);
        assert!(!encoded.contains(':'));
        assert_eq!(decode_uri_component(&encoded).unwrap(), name);
        assert!(decode_uri_component("%E0%A4%A").is_err());
    }

    #[test]
    fn session_tokens_are_base36_and_unique() {
        let (a, b) = (session_token(5), session_token(5));
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
        assert_eq!(session_token(6).len(), a.len() + 1);
    }

    #[test]
    fn compact_uptime_matches_ts() {
        assert_eq!(compact_uptime(59_999), "59s");
        assert_eq!(compact_uptime(3_660_000), "1h 1m");
        assert_eq!(compact_uptime(90_000_000), "1d 1h");
    }

    #[test]
    fn parses_label_wrapped_file_upload_submissions() {
        let raw = json!({
            "data": {
                "custom_id": "embedui:attachmentupload:t",
                "components": [{ "type": 18, "id": 1, "component": { "type": 19, "id": 2, "custom_id": "attachment", "values": ["99"] } }],
                "resolved": { "attachments": { "99": { "id": "99", "filename": "a.png", "size": 3, "url": "https://cdn/a.png", "content_type": "image/png" } } }
            }
        });
        assert_eq!(
            parse_file_upload(&raw, "attachment"),
            Some(FileUpload { name: "a.png".into(), url: "https://cdn/a.png".into(), size: 3, content_type: Some("image/png".into()) })
        );
        assert_eq!(parse_file_upload(&json!({"data": {"components": []}}), "attachment"), None);
    }

    /// The real path: Discord's modal-submit JSON goes through (patched) serenity and back out intact.
    #[test]
    fn file_upload_survives_serenity_modal_deserialization() {
        let raw = json!({
            "id": "1", "application_id": "901", "type": 5, "token": "t", "version": 1, "guild_id": "1", "channel_id": "2",
            "locale": "en-US", "entitlements": [], "authorizing_integration_owners": {}, "app_permissions": "0", "attachment_size_limit": 8388608,
            "member": { "user": { "id": "5", "username": "u", "discriminator": "0", "global_name": null, "avatar": null },
                        "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "permissions": "8" },
            "data": {
                "custom_id": "embedui:attachmentupload:t",
                "components": [{ "type": 18, "id": 1, "component": { "type": 19, "id": 2, "custom_id": "attachment", "values": ["99"] } }],
                "resolved": { "attachments": { "99": { "id": "99", "filename": "a.png", "size": 3, "url": "https://cdn.discordapp.com/a.png",
                                                     "proxy_url": "https://media.discordapp.net/a.png", "content_type": "image/png" } } }
            }
        });
        let Interaction::Modal(modal) = serde_json::from_value::<Interaction>(raw).expect("serenity parses the modal") else {
            panic!("not a modal interaction")
        };
        let upload = serde_json::to_value(&modal).ok().and_then(|raw| parse_file_upload(&raw, "attachment"));
        assert_eq!(upload.map(|u| (u.name, u.size)), Some(("a.png".into(), 3)));
    }

    // ---- Embed Manager behaviour (fake Discord) ----
    use crate::test_support::{GuildSeed, Harness};

    const A: u64 = 1;
    const B: u64 = 2;
    const ADMIN: u64 = 5;
    const OTHER_ADMIN: u64 = 8;
    const MEMBER: u64 = 6;
    const B_ADMIN: u64 = 7;

    /// Guild A: admins 5 and 8, plain member 6, text channel 100, roles 30/31. Guild B: admin 7, channel 200.
    async fn embed_harness() -> (Harness, GaiaBot) {
        let h = Harness::start().await;
        h.seed(
            GuildSeed::new(A, "A")
                .role(10, "Admin", 5, Permissions::ADMINISTRATOR)
                .role(20, "Gaia", 10, Permissions::SEND_MESSAGES | Permissions::MENTION_EVERYONE)
                .role(30, "News", 1, Permissions::empty())
                .role(31, "Events", 1, Permissions::empty())
                .channel(100, ChannelType::Text, None)
                .bot(&[20])
                .member(ADMIN, &[10])
                .member(OTHER_ADMIN, &[10])
                .member(MEMBER, &[]),
        );
        h.seed(
            GuildSeed::new(B, "B")
                .role(11, "Admin", 5, Permissions::ADMINISTRATOR)
                .channel(200, ChannelType::Text, None)
                .bot(&[])
                .member(B_ADMIN, &[11]),
        );
        let bot = h.bot();
        (h, bot)
    }

    async fn click(h: &Harness, bot: &GaiaBot, guild: u64, user: u64, id: &str) {
        bot.on_interaction(&h.ctx(), &Interaction::Component(h.button(guild, user, id))).await;
    }

    async fn submit(h: &Harness, bot: &GaiaBot, guild: u64, user: u64, id: &str, fields: &[(&str, &str)]) {
        bot.on_interaction(&h.ctx(), &Interaction::Modal(h.modal(guild, user, id, fields))).await;
    }

    async fn pick(h: &Harness, bot: &GaiaBot, guild: u64, user: u64, id: &str, kind: u8, values: &[&str]) {
        bot.on_interaction(&h.ctx(), &Interaction::Component(h.select(guild, user, id, kind, values))).await;
    }

    fn last_reply(h: &Harness) -> Value {
        h.replies().last().cloned().expect("no interaction response")
    }

    fn session(guild: u64, user: u64) -> Session {
        Session { guild_id: guild.to_string(), user_id: user.to_string(), ..Default::default() }
    }

    #[tokio::test]
    async fn rejects_embed_actions_after_administrator_permission_is_removed() {
        let (h, bot) = embed_harness().await;
        // TS removedAdministratorInteraction: the member's Administrator role is gone by the time they click.
        h.seed(GuildSeed::new(A, "A").role(10, "Admin", 5, Permissions::ADMINISTRATOR).bot(&[]).member(ADMIN, &[]));
        click(&h, &bot, A, ADMIN, "embedui:create:1").await;
        let reply = last_reply(&h);
        assert_eq!(reply["type"], 4, "no modal opened");
        assert!(reply["data"]["content"].as_str().unwrap().contains("Administrator"));
        click(&h, &bot, A, ADMIN, "embedflow:input:create:Name").await;
        assert!(last_reply(&h)["data"]["content"].as_str().unwrap().contains("permission"));
    }

    #[tokio::test]
    async fn forged_embed_modal_from_a_non_admin_is_rejected() {
        let (h, bot) = embed_harness().await;
        submit(&h, &bot, A, MEMBER, "embedmodal:create:Forged", &[("json", r#"{"content":"x"}"#)]).await;
        assert!(last_reply(&h)["data"]["content"].as_str().unwrap().contains("Administrator"));
        assert_eq!(bot.pending_embeds.len(), 0);
        submit(&h, &bot, A, ADMIN, "embedmodal:create:Real", &[("json", r#"{"content":"x"}"#)]).await;
        assert_eq!(last_reply(&h)["data"]["components"][0]["components"][0]["label"], "Save Embed");
        assert_eq!(bot.pending_embeds.len(), 1);
    }

    #[tokio::test]
    async fn validates_template_names_from_custom_ids_and_modals() {
        let (h, bot) = embed_harness().await;
        let long = "x".repeat(65);
        for id in ["embedmodal:create:bad%01name".to_string(), format!("embedmodal:create:{long}"), "embedmodal:create:%20%20".into()] {
            submit(&h, &bot, A, ADMIN, &id, &[("json", r#"{"content":"x"}"#)]).await;
            assert_eq!(last_reply(&h)["data"]["content"], INVALID_NAME, "{id}");
        }
        click(&h, &bot, A, ADMIN, "embedflow:input:create:a%0Ab").await;
        assert_eq!(last_reply(&h)["data"]["content"], INVALID_NAME);
        submit(&h, &bot, A, ADMIN, "embedui:name:create:1", &[("name", "tab\there")]).await;
        assert_eq!(last_reply(&h)["data"]["content"], INVALID_NAME);
        assert_eq!(bot.pending_embeds.len(), 0);
        // A forged pending entry with a bad name is refused at save time too.
        let pending = PendingEmbed { name: "a\u{7}".into(), json: "{}".into(), replace: false, user_id: ADMIN.to_string(), guild_id: A.to_string() };
        bot.pending_embeds.insert("tok".into(), &ADMIN.to_string(), pending);
        click(&h, &bot, A, ADMIN, "embedflow:save:tok").await;
        assert_eq!(last_reply(&h)["data"]["content"], INVALID_NAME);
        assert!(h.repo.embeds("1", None, None).unwrap().is_empty());
        assert_eq!(template_name("  Weekly News  ").as_deref(), Some("Weekly News"));
        assert!(template_name(&"é".repeat(64)).is_some());
    }

    #[tokio::test]
    async fn saves_templates_per_guild_and_cannot_reach_another_guilds_templates() {
        let (h, bot) = embed_harness().await;
        // Guild B owns "Secret" with an attachment and a pending schedule.
        h.repo.save_embed("2", "Secret", r#"{"content":"b only"}"#, "b", None).unwrap();
        let tb = h.repo.embed("2", "Secret").unwrap().unwrap().template_id;
        let png = crate::services::embed_renderer::attachment_record(&tb, "x.png", "image/png", b"\x89PNG\r\n\x1a\n".to_vec(), "b").unwrap();
        h.repo.save_attachment("2", &png, 10, 1 << 20).unwrap();
        let stamp = crate::db::now();
        h.repo
            .create_schedule(&ScheduledEmbedSend {
                id: "sb".into(),
                template_id: tb.clone(),
                guild_id: "2".into(),
                channel_id: "200".into(),
                scheduled_for_utc: "2099-01-01T00:00:00.000Z".into(),
                timezone: "Europe/Ljubljana".into(),
                created_by_user_id: "b".into(),
                created_at: stamp.clone(),
                updated_by_user_id: "b".into(),
                updated_at: stamp,
                status: "scheduled".into(),
                template_name_snapshot: "Secret".into(),
                ..Default::default()
            })
            .unwrap();

        // See: list/pickers only show guild A's (empty) library.
        click(&h, &bot, A, ADMIN, "embedui:list:1").await;
        assert_eq!(last_reply(&h)["data"]["content"], "No embeds have been saved in this server.");
        click(&h, &bot, A, ADMIN, "embedui:send:1").await;
        assert_eq!(last_reply(&h)["data"]["content"], "No embeds have been saved in this server.");
        pick(&h, &bot, A, ADMIN, "embedui:attachmenttemplate:1", 3, &["Secret"]).await;
        assert_eq!(last_reply(&h)["data"]["content"], "That embed no longer exists.");
        // Delete: guild B's attachment survives a forged remove from guild A.
        pick(&h, &bot, A, ADMIN, &format!("embedui:attachmentremove:{tb}"), 3, &["x.png"]).await;
        assert_eq!(last_reply(&h)["data"]["content"], "That template no longer exists.");
        assert_eq!(h.repo.attachments("2", &tb).unwrap().len(), 1);
        // Send: a forged send session naming guild B's template finds nothing in guild A.
        bot.embed_sessions.insert(
            "send".into(),
            "5",
            Session { name: Some("Secret".into()), channel_id: Some("100".into()), ..session(A, ADMIN) },
        );
        click(&h, &bot, A, ADMIN, "embedui:manualconfirm:send").await;
        assert_eq!(last_reply(&h)["data"]["content"], "Embed or destination channel is unavailable.");
        assert!(h.calls_to("POST", "/channels/*/messages").is_empty());
        // Schedules: guild B's schedule cannot be viewed or cancelled from guild A.
        click(&h, &bot, A, ADMIN, "embedui:cancelschedule:sb").await;
        assert_eq!(last_reply(&h)["data"]["content"], "That pending schedule is unavailable.");
        click(&h, &bot, A, ADMIN, "embedui:cancelconfirm:sb").await;
        assert_eq!(last_reply(&h)["data"]["content"], "Schedule was already changed or is unavailable.");
        assert_eq!(h.repo.schedule("2", "sb").unwrap().unwrap().status, "scheduled");
        // Edit: "editing" Secret from guild A creates guild A's own template and leaves B's untouched.
        submit(&h, &bot, A, ADMIN, "embedmodal:edit:Secret", &[("json", r#"{"content":"a"}"#)]).await;
        let token = last_reply(&h)["data"]["components"][0]["components"][0]["custom_id"].as_str().unwrap().replace("embedflow:save:", "");
        click(&h, &bot, A, ADMIN, &format!("embedflow:save:{token}")).await;
        assert_eq!(last_reply(&h)["data"]["content"], "Embed **Secret** saved.");
        assert_eq!(h.repo.embed("1", "Secret").unwrap().unwrap().payload_json, r#"{"content":"a"}"#);
        assert_eq!(h.repo.embed("2", "Secret").unwrap().unwrap().payload_json, r#"{"content":"b only"}"#);
        click(&h, &bot, B, B_ADMIN, "embedui:list:2").await;
        assert_eq!(last_reply(&h)["data"]["content"], "**Embeds**\n• Secret");
    }

    #[tokio::test]
    async fn mention_back_cancel_and_schedule_cancel_require_session_ownership() {
        let (h, bot) = embed_harness().await;
        bot.embed_sessions.insert("m".into(), "5", Session { name: Some("N".into()), channel_id: Some("100".into()), ..session(A, ADMIN) });
        bot.schedule_sessions.insert("s".into(), "5", session(A, ADMIN));
        for id in ["embedui:mentionback:manual:m", "embedui:mentioncancel:manual:m", "embedui:schedulecancel:s"] {
            click(&h, &bot, A, OTHER_ADMIN, id).await;
            assert_eq!(last_reply(&h)["type"], 4, "{id}: refused with a reply, not an update");
        }
        assert!(bot.embed_sessions.get("m").is_some(), "another admin cannot cancel the session");
        assert!(bot.schedule_sessions.get("s").is_some());
        click(&h, &bot, A, ADMIN, "embedui:mentionback:manual:m").await;
        assert_eq!(last_reply(&h)["type"], 7);
        click(&h, &bot, A, ADMIN, "embedui:mentioncancel:manual:m").await;
        assert!(bot.embed_sessions.get("m").is_none());
        click(&h, &bot, A, ADMIN, "embedui:schedulecancel:s").await;
        assert!(bot.schedule_sessions.get("s").is_none());
    }

    #[tokio::test]
    async fn embedflow_cancel_drops_the_pending_preview() {
        let (h, bot) = embed_harness().await;
        submit(&h, &bot, A, ADMIN, "embedmodal:create:Draft", &[("json", r#"{"content":"x"}"#)]).await;
        let cancel = last_reply(&h)["data"]["components"][0]["components"][1]["custom_id"].as_str().unwrap().to_string();
        click(&h, &bot, A, OTHER_ADMIN, &cancel).await;
        assert_eq!(bot.pending_embeds.len(), 1, "only the owner's cancel drops it");
        click(&h, &bot, A, ADMIN, &cancel).await;
        assert_eq!(bot.pending_embeds.len(), 0);
        assert_eq!(last_reply(&h)["data"]["content"], "Embed changes cancelled.");
    }

    #[test]
    fn sessions_expire_after_ttl_and_are_capped_per_user() {
        let store = SessionStore::<u32>::default();
        store.insert("old".into(), "u", 1);
        store.backdate("old", SESSION_TTL);
        assert_eq!(store.get("old"), None, "expired entries are invisible");
        assert_eq!(store.update("old", |v| *v), None);
        store.insert("new".into(), "u", 2);
        store.backdate("new", Duration::from_secs(1));
        assert_eq!(store.len(), 1, "insert sweeps expired entries");
        store.insert("other".into(), "v", 9);
        for n in 0..SESSIONS_PER_USER {
            store.insert(format!("t{n}"), "u", n as u32);
        }
        assert_eq!(store.get("new"), None, "the user's oldest session is evicted at the cap");
        assert_eq!(store.get("other"), Some(9), "other users are unaffected");
        assert_eq!(store.len(), SESSIONS_PER_USER + 1);
    }

    #[tokio::test]
    async fn routes_native_role_selection_into_a_manual_delivery_mention_session() {
        let (h, bot) = embed_harness().await;
        let s = Session { name: Some("Notice".into()), channel_id: Some("100".into()), mention: Some(DeliveryMention::default()), ..session(A, ADMIN) };
        bot.embed_sessions.insert("token".into(), "5", s);
        pick(&h, &bot, A, ADMIN, "embedui:mentionroles:manual:token", 6, &["30", "31"]).await;
        assert_eq!(
            bot.embed_sessions.get("token").unwrap().mention,
            Some(DeliveryMention { roles: vec!["30".into(), "31".into()], everyone: false, here: false })
        );
        let reply = last_reply(&h);
        assert_eq!(reply["type"], 7);
        assert_eq!(reply["data"]["embeds"][0]["title"], "Mention Settings");
        // Another admin cannot mutate the session.
        pick(&h, &bot, A, OTHER_ADMIN, "embedui:mentionroles:manual:token", 6, &["30"]).await;
        assert_eq!(bot.embed_sessions.get("token").unwrap().mention.unwrap().roles.len(), 2);
    }

    #[tokio::test]
    async fn scheduled_sends_persist_and_deliver_the_chosen_mention() {
        let (h, bot) = embed_harness().await;
        h.repo.save_embed("1", "Notice", r#"{"content":"hello"}"#, "admin", None).unwrap();
        let t = h.repo.embed("1", "Notice").unwrap().unwrap().template_id;
        let due = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mention = DeliveryMention { roles: vec!["30".into()], everyone: false, here: false };
        let s = Session {
            name: Some("Notice".into()),
            template_id: Some(t),
            channel_id: Some("100".into()),
            utc: Some(due),
            display: Some("soon".into()),
            mention: Some(mention.clone()),
            ..session(A, ADMIN)
        };
        bot.schedule_sessions.insert("sched".into(), "5", s);
        click(&h, &bot, A, ADMIN, "embedui:scheduleconfirm:sched").await;
        assert_eq!(last_reply(&h)["data"]["embeds"][0]["title"], "Embed Scheduled");
        let stored = h.repo.upcoming_schedules("1", None).unwrap();
        assert_eq!(parse_mention(stored[0].mention_json.as_deref()), mention, "mention is persisted");

        let scheduler = EmbedScheduler::new(h.repo.clone(), h.discord.clone(), None);
        scheduler.tick(chrono::Utc::now()).await.unwrap();
        let sent = h.calls_to("POST", "/channels/100/messages");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].body["content"], "<@&30>\nhello");
        assert_eq!(sent[0].body["allowed_mentions"]["roles"], json!(["30"]));
        let history = h.repo.schedule_history("1", None).unwrap();
        assert_eq!(history[0].status, "sent");
    }
}
