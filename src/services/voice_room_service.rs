use crate::ctx;
use crate::db::repositories::{ActiveRoom, GaiaRepository, VoicePreference};
use crate::discord::Discord;
use crate::logger;
use crate::services::locks::KeyedLock;
use crate::services::mute_service::{edit_overwrite, overwrite_target};
use regex::Regex;
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateButton, CreateChannel, CreateEmbed, CreateEmbedFooter,
    CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateModal,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, EditChannel, EditMember, EditMessage,
};
use serenity::futures::future::join_all;
use serenity::model::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

const BRAND: u32 = crate::discord::GAIA_PURPLE;
static ROOM_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Room (\d+)$").unwrap());

fn compact_ms(started: Instant) -> String {
    format!("{}ms", started.elapsed().as_millis())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CachedAccess {
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccessPlanEntry {
    pub id: String,
    pub allow: Vec<&'static str>,
    pub deny: Vec<&'static str>,
}

/// Permission overwrites for a new room (TS `roomAccessPlan`); "Connect" is the only flag used.
pub fn room_access_plan(
    everyone_id: &str,
    owner_id: &str,
    is_locked: i64,
    access: &CachedAccess,
    verification_role_id: Option<&str>,
    verification_role_exists: bool,
) -> Vec<AccessPlanEntry> {
    let entry = |id: &str, allow: bool, deny: bool| AccessPlanEntry {
        id: id.to_string(),
        allow: if allow { vec!["Connect"] } else { vec![] },
        deny: if deny { vec!["Connect"] } else { vec![] },
    };
    let restricted = verification_role_id.is_some() && verification_role_exists;
    let locked = is_locked != 0;
    let mut plan = vec![entry(everyone_id, false, locked || restricted), entry(owner_id, true, false)];
    if let (true, false, Some(role)) = (restricted, locked, verification_role_id) {
        plan.push(entry(role, true, false));
    }
    let mut seen = HashSet::new();
    for id in access.allowed.iter().chain(&access.denied) {
        if seen.insert(id) {
            let denied = access.denied.contains(id);
            plan.push(entry(id, !denied, denied));
        }
    }
    plan
}

fn ephemeral(content: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true))
}

fn not_owner() -> CreateInteractionResponse {
    ephemeral("You are not the current owner of this room.")
}

fn parts(custom_id: &str) -> (String, String) {
    let mut split = custom_id.split(':').skip(1);
    (split.next().unwrap_or_default().to_string(), split.next().unwrap_or_default().to_string())
}

fn is_voice(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::Voice | ChannelType::Stage)
}

fn modal_value(i: &ModalInteraction, id: &str) -> String {
    i.data
        .components
        .iter()
        .flat_map(|row| &row.components)
        .find_map(|c| match c {
            ActionRowComponent::InputText(t) if t.custom_id == id => Some(t.value.clone().unwrap_or_default()),
            _ => None,
        })
        .unwrap_or_default()
}

#[derive(Clone)]
struct GuildConfig {
    enabled: bool,
    join_channel_id: Option<String>,
}

pub struct VoiceRoomService {
    repo: Arc<GaiaRepository>,
    discord: Discord,
    locks: KeyedLock,
    prefs: Mutex<HashMap<String, VoicePreference>>,
    accesses: Mutex<HashMap<String, CachedAccess>>,
    configs: Mutex<HashMap<String, GuildConfig>>,
    rename_cooldowns: Mutex<HashMap<String, Instant>>,
}

impl VoiceRoomService {
    pub fn new(repo: Arc<GaiaRepository>, discord: Discord) -> Self {
        Self {
            repo,
            discord,
            locks: KeyedLock::default(),
            prefs: Mutex::default(),
            accesses: Mutex::default(),
            configs: Mutex::default(),
            rename_cooldowns: Mutex::default(),
        }
    }

    fn key(guild_id: &str, owner_id: &str) -> String {
        format!("{guild_id}:{owner_id}")
    }

    fn clear_owner_cache(&self, guild_id: &str, owner_id: &str) {
        let key = Self::key(guild_id, owner_id);
        self.prefs.lock().unwrap().remove(&key);
        self.accesses.lock().unwrap().remove(&key);
    }

    pub fn forget_guild(&self, guild_id: &str) {
        self.configs.lock().unwrap().remove(guild_id);
        let prefix = format!("{guild_id}:");
        self.prefs.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        self.accesses.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
        self.rename_cooldowns.lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
    }

    fn voice_channel_of(&self, guild: GuildId, user: UserId) -> Option<ChannelId> {
        self.discord.cache.guild(guild)?.voice_states.get(&user)?.channel_id
    }

    fn live_channel(&self, guild: GuildId, channel: ChannelId) -> Option<GuildChannel> {
        self.discord.cache.guild(guild)?.channels.get(&channel).cloned()
    }

    fn live_voice(&self, guild: GuildId, channel_id: &str) -> Option<GuildChannel> {
        self.live_channel(guild, channel_id.parse().ok()?).filter(|c| is_voice(c.kind))
    }

    fn guild_name(&self, guild: GuildId) -> String {
        self.discord.cache.guild(guild).map(|g| g.name.clone()).unwrap_or_default()
    }

    pub async fn on_voice_state(self: &Arc<Self>, old: Option<&VoiceState>, state: &VoiceState) -> anyhow::Result<()> {
        let (Some(member), Some(guild)) = (state.member.clone(), state.guild_id) else { return Ok(()) };
        let old_channel = old.and_then(|o| o.channel_id);
        let new_channel = state.channel_id;
        if let Some(old_channel) = old_channel.filter(|c| Some(*c) != new_channel) {
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.reconcile_room(guild, old_channel).await {
                    logger::error(
                        "voice room reconciliation failed",
                        ctx! {"guildId" => guild.to_string(), "channelId" => old_channel.to_string(), "error" => e.to_string()},
                    );
                }
            });
        }
        let Some(new_channel) = new_channel else { return Ok(()) };
        if old_channel == Some(new_channel) || member.user.bot {
            return Ok(());
        }
        let gid = guild.to_string();
        let config = self.config(&gid)?;
        if !config.enabled || Some(new_channel.to_string()) != config.join_channel_id {
            return Ok(());
        }
        self.locks
            .run(&format!("{gid}:{}", member.user.id), async {
                if self.voice_channel_of(guild, member.user.id).map(|c| c.to_string()) != config.join_channel_id
                    || self.repo.active_room_for_owner(&gid, &member.user.id.to_string())?.is_some()
                {
                    return Ok(());
                }
                self.create_room(guild, &member, new_channel).await
            })
            .await
    }

    pub async fn refresh_guild_config(&self, guild_id: &str) -> anyhow::Result<()> {
        self.configs.lock().unwrap().remove(guild_id);
        self.config(guild_id)?;
        Ok(())
    }

    pub async fn create_room(self: &Arc<Self>, guild: GuildId, owner: &Member, source: ChannelId) -> anyhow::Result<()> {
        self.locks.run(&format!("{guild}:room-create"), self.create_room_locked(guild, owner, source)).await
    }

    async fn create_room_locked(self: &Arc<Self>, guild: GuildId, owner: &Member, source: ChannelId) -> anyhow::Result<()> {
        let started = Instant::now();
        let gid = guild.to_string();
        let owner_id = owner.user.id.to_string();
        let preference = self.preference(&gid, &owner_id)?;
        let access = self.access(&gid, &owner_id)?;
        let verification = self.repo.verification(&gid)?;
        let reservation_ms = compact_ms(started);
        let parent = self.live_channel(guild, source).and_then(|c| c.parent_id);
        let name = preference.custom_name.clone().unwrap_or_else(|| self.available_name(guild));
        let overwrites = self.overwrites(
            guild,
            &owner_id,
            &preference,
            &access,
            if verification.enabled != 0 { verification.role_id.as_deref() } else { None },
        );
        let reason = format!("Gaia room for {}", owner.user.tag());
        let mut builder = CreateChannel::new(name)
            .kind(ChannelType::Voice)
            .user_limit(preference.user_limit as u32)
            .permissions(overwrites)
            .audit_log_reason(&reason);
        if let Some(parent) = parent {
            builder = builder.category(parent);
        }
        let Ok(room) = guild.create_channel(&self.discord.http, builder).await else {
            logger::error("room creation failed", ctx! {"guildId" => gid, "userId" => owner_id});
            return Ok(());
        };
        let creation_ms = compact_ms(started);
        let http = &self.discord.http;
        let created = self.repo.create_room(&ActiveRoom {
            guild_id: gid.clone(),
            channel_id: room.id.to_string(),
            owner_id: owner_id.clone(),
            control_channel_id: Some(room.id.to_string()),
            control_message_id: None,
        })?;
        if !created {
            let _ = http.delete_channel(room.id, Some("Duplicate Gaia room prevented")).await;
            return Ok(());
        }
        if self.voice_channel_of(guild, owner.user.id) != Some(source) {
            self.repo.delete_room(&gid, &room.id.to_string())?;
            let _ = http.delete_channel(room.id, Some("Owner left join channel")).await;
            return Ok(());
        }
        let moved = guild
            .edit_member(http, owner.user.id, EditMember::new().voice_channel(room.id).audit_log_reason("Gaia join-to-create"))
            .await;
        if moved.is_err() {
            self.repo.delete_room(&gid, &room.id.to_string())?;
            let _ = http.delete_channel(room.id, Some("Gaia could not move owner")).await;
            return Ok(());
        }
        let move_ms = compact_ms(started);
        logger::info(
            "voice room critical path",
            ctx! {"guildId" => gid, "userId" => owner_id, "reservation" => reservation_ms, "channelCreation" => creation_ms, "memberMove" => move_ms.clone(), "total" => move_ms},
        );
        let this = self.clone();
        let owner = owner.user.id;
        tokio::spawn(async move { this.post_create(guild, room.id, owner, started).await });
        Ok(())
    }

    async fn post_create(&self, guild: GuildId, room: ChannelId, owner: UserId, started: Instant) {
        let result: anyhow::Result<()> = async {
            let embed = self.panel_embed(guild, &owner.to_string())?;
            let message = room
                .send_message(
                    &self.discord.http,
                    CreateMessage::new()
                        .content(format!("<@{owner}>"))
                        .allowed_mentions(CreateAllowedMentions::new().users([owner]))
                        .embed(embed)
                        .components(Self::controls(&room.to_string())),
                )
                .await?;
            self.repo.set_room_control_message(&guild.to_string(), &room.to_string(), Some(&message.id.to_string()))?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => logger::info(
                "voice room post-create setup",
                ctx! {"guildId" => guild.to_string(), "channelId" => room.to_string(), "duration" => compact_ms(started)},
            ),
            Err(_) => logger::warn("voice room panel setup failed", ctx! {"guildId" => guild.to_string(), "channelId" => room.to_string()}),
        }
    }

    pub async fn reconcile_room(&self, guild: GuildId, channel: ChannelId) -> anyhow::Result<()> {
        let (gid, cid) = (guild.to_string(), channel.to_string());
        if self.repo.active_room(&gid, &cid)?.is_none() {
            return Ok(());
        }
        self.locks
            .run(&format!("{gid}:channel:{cid}"), async {
                let room = self.repo.active_room(&gid, &cid)?;
                let live = self.live_voice(guild, &cid);
                let (Some(room), Some(live)) = (room.clone(), live) else {
                    if room.is_some() {
                        self.repo.delete_room(&gid, &cid)?;
                    }
                    return Ok(());
                };
                let (humans, owner_present) = {
                    let Some(g) = self.discord.cache.guild(guild) else { return Ok(()) };
                    let inside: Vec<&VoiceState> = g.voice_states.values().filter(|v| v.channel_id == Some(channel)).collect();
                    let humans: Vec<UserId> = inside
                        .iter()
                        .filter(|v| g.members.get(&v.user_id).or(v.member.as_ref()).is_some_and(|m| !m.user.bot))
                        .map(|v| v.user_id)
                        .collect();
                    (humans, inside.iter().any(|v| v.user_id.to_string() == room.owner_id))
                };
                if humans.is_empty() {
                    self.repo.delete_room(&gid, &cid)?;
                    let _ = self.discord.http.delete_channel(channel, Some("Gaia temporary room is empty")).await;
                    return Ok(());
                }
                if owner_present {
                    return Ok(());
                }
                // ponytail: clock-nanos pick instead of a rand dependency; uniform enough for choosing a room owner.
                let pick = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as usize);
                let next = humans[pick % humans.len()];
                if !self.repo.transfer_room(&gid, &cid, &room.owner_id, &next.to_string())? {
                    return Ok(());
                }
                self.apply_preferences(guild, &live, &next.to_string(), None).await?;
                self.refresh_panel(guild, &cid).await?;
                let embed = CreateEmbed::new().color(BRAND).title("Gaia room ownership transferred").description(format!(
                    "You now own **{}** in **{}**. Your saved room preferences were applied.",
                    live.name,
                    self.guild_name(guild)
                ));
                let _ = next.direct_message(&self.discord.http, CreateMessage::new().embed(embed)).await;
                logger::info("voice ownership transferred", ctx! {"guildId" => gid, "channelId" => cid, "userId" => next.to_string()});
                Ok(())
            })
            .await
    }

    pub async fn cleanup_guild(&self, guild: GuildId) -> anyhow::Result<()> {
        let gid = guild.to_string();
        for room in self.repo.active_rooms(Some(&gid))? {
            let channel = self.live_voice(guild, &room.channel_id);
            self.repo.delete_room(&gid, &room.channel_id)?;
            self.clear_owner_cache(&gid, &room.owner_id);
            if let Some(channel) = channel {
                let _ = self.discord.http.delete_channel(channel.id, Some("Gaia Voice Rooms disabled")).await;
            }
        }
        Ok(())
    }

    pub async fn recover(&self, guild: GuildId) -> anyhow::Result<()> {
        let gid = guild.to_string();
        self.refresh_guild_config(&gid).await?;
        for room in self.repo.active_rooms(Some(&gid))? {
            match self.live_voice(guild, &room.channel_id) {
                None => self.repo.delete_room(&gid, &room.channel_id)?,
                Some(channel) => self.reconcile_room(guild, channel.id).await?,
            }
        }
        Ok(())
    }

    pub async fn reconcile_guild_access(&self, guild: GuildId, previous_verification_role_id: Option<&str>) -> anyhow::Result<()> {
        let gid = guild.to_string();
        for room in self.repo.active_rooms(Some(&gid))? {
            let Some(channel) = self.live_voice(guild, &room.channel_id) else {
                self.repo.delete_room(&gid, &room.channel_id)?;
                continue;
            };
            self.locks
                .run(
                    &format!("{gid}:channel:{}", channel.id),
                    self.apply_preferences(guild, &channel, &room.owner_id, previous_verification_role_id),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn handle_button(&self, i: &ComponentInteraction) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let (action, channel_id) = parts(&i.data.custom_id);
        let Some(guild) = self.authorized_guild(i.guild_id, &channel_id, i.user.id)? else {
            i.create_response(http, not_owner()).await?;
            return Ok(());
        };
        let (gid, uid) = (guild.to_string(), i.user.id.to_string());
        match action.as_str() {
            "toggle" => {
                let p = self.preference(&gid, &uid)?;
                self.set_preference(&gid, &uid, "is_locked", if p.is_locked != 0 { 0.into() } else { 1.into() })?;
                self.apply_and_refresh(guild, &channel_id, &uid).await?;
                i.create_response(http, ephemeral(if p.is_locked != 0 { "Room unlocked." } else { "Room locked." })).await?;
            }
            "limit" => i.create_response(http, Self::limit_modal(&channel_id)).await?,
            "rename" => i.create_response(http, Self::rename_modal(&channel_id)).await?,
            "allow" | "deny" => {
                let menu = CreateSelectMenu::new(format!("roompick:{action}:{channel_id}"), CreateSelectMenuKind::User { default_users: None })
                    .placeholder(format!("Select a member to {action}"))
                    .min_values(1)
                    .max_values(1);
                let message = CreateInteractionResponseMessage::new()
                    .content(format!("Select a member to {action}."))
                    .components(vec![CreateActionRow::SelectMenu(menu)])
                    .ephemeral(true);
                i.create_response(http, CreateInteractionResponse::Message(message)).await?;
            }
            "manage" => self.show_access(i, guild, &channel_id, false).await?,
            "reset" => {
                let embed = CreateEmbed::new().color(0xed4245).title("Reset Voice Room Settings?").description(
                    "This resets your saved settings for this server: user limit, lock state, allowed users, and denied users. Your current room will update too.",
                );
                let row = CreateActionRow::Buttons(vec![
                    CreateButton::new(format!("roomreset:cancel:{channel_id}")).label("Cancel").style(ButtonStyle::Secondary),
                    CreateButton::new(format!("roomreset:confirm:{channel_id}")).label("Reset Settings").style(ButtonStyle::Danger),
                ]);
                let message = CreateInteractionResponseMessage::new().embed(embed).components(vec![row]).ephemeral(true);
                i.create_response(http, CreateInteractionResponse::Message(message)).await?;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn handle_modal(&self, i: &ModalInteraction) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let (action, channel_id) = parts(&i.data.custom_id);
        let Some(guild) = self.authorized_guild(i.guild_id, &channel_id, i.user.id)? else {
            i.create_response(http, not_owner()).await?;
            return Ok(());
        };
        let (gid, uid) = (guild.to_string(), i.user.id.to_string());
        if action == "rename" {
            let name = modal_value(i, "name").trim().to_string();
            if name.is_empty() || name.encode_utf16().count() > 20 {
                i.create_response(http, ephemeral("Room names must be between 1 and 20 characters.")).await?;
                return Ok(());
            }
            let key = format!("{gid}:{channel_id}");
            let recent = self.rename_cooldowns.lock().unwrap().get(&key).is_some_and(|last| last.elapsed().as_millis() < 5_000);
            if recent {
                i.create_response(http, ephemeral("Please wait a few seconds before renaming again.")).await?;
                return Ok(());
            }
            let Some(channel) = self.live_voice(guild, &channel_id) else {
                i.create_response(http, ephemeral("That room no longer exists.")).await?;
                return Ok(());
            };
            if channel.id.edit(http, EditChannel::new().name(&name).audit_log_reason("Gaia room rename")).await.is_err() {
                i.create_response(http, ephemeral("Gaia could not rename that room.")).await?;
                return Ok(());
            }
            self.rename_cooldowns.lock().unwrap().insert(key, Instant::now());
            self.set_preference(&gid, &uid, "custom_name", name.clone().into())?;
            self.refresh_panel(guild, &channel_id).await?;
            i.create_response(http, ephemeral(format!("Room renamed to **{name}**."))).await?;
            return Ok(());
        }
        if action != "limit" {
            return Ok(());
        }
        let value = modal_value(i, "limit").trim().to_string();
        let limit = value.parse::<i64>().ok().filter(|n| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) && *n <= 25);
        let Some(limit) = limit else {
            i.create_response(http, ephemeral("Enter a whole number from 1 to 25, or 0 for unlimited.")).await?;
            return Ok(());
        };
        self.set_preference(&gid, &uid, "user_limit", limit.into())?;
        self.apply_and_refresh(guild, &channel_id, &uid).await?;
        i.create_response(http, ephemeral("Room user limit updated.")).await?;
        Ok(())
    }

    pub async fn handle_user_select(&self, i: &ComponentInteraction) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let (action, channel_id) = parts(&i.data.custom_id);
        let guild = if action == "allow" || action == "deny" { self.authorized_guild(i.guild_id, &channel_id, i.user.id)? } else { None };
        let Some(guild) = guild else {
            i.create_response(http, not_owner()).await?;
            return Ok(());
        };
        let user = match &i.data.kind {
            ComponentInteractionDataKind::UserSelect { values } => values.first().copied(),
            _ => None,
        };
        let cached = user.filter(|u| self.discord.cache.guild(guild).is_some_and(|g| g.members.contains_key(u)));
        let member = match (cached, user) {
            (Some(u), _) => Some(u),
            (None, Some(u)) => guild.member(http, u).await.ok().map(|m| m.user.id),
            _ => None,
        };
        let Some(member) = member else {
            i.create_response(http, ephemeral("That user is no longer in this server.")).await?;
            return Ok(());
        };
        let (gid, uid) = (guild.to_string(), i.user.id.to_string());
        self.set_access(&gid, &uid, &member.to_string(), &action, true)?;
        self.apply_and_refresh(guild, &channel_id, &uid).await?;
        let verb = if action == "allow" { "allowed" } else { "denied" };
        let message = CreateInteractionResponseMessage::new().content(format!("<@{member}> was {verb}.")).components(vec![]);
        i.create_response(http, CreateInteractionResponse::UpdateMessage(message)).await?;
        Ok(())
    }

    pub async fn handle_string_select(&self, i: &ComponentInteraction) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let (list, channel_id) = parts(&i.data.custom_id);
        let guild = if list == "allowed" || list == "denied" { self.authorized_guild(i.guild_id, &channel_id, i.user.id)? } else { None };
        let Some(guild) = guild else {
            i.create_response(http, not_owner()).await?;
            return Ok(());
        };
        let user = match &i.data.kind {
            ComponentInteractionDataKind::StringSelect { values } => values.first().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        let (gid, uid) = (guild.to_string(), i.user.id.to_string());
        self.set_access(&gid, &uid, &user, if list == "allowed" { "allow" } else { "deny" }, false)?;
        self.apply_and_refresh(guild, &channel_id, &uid).await?;
        self.show_access(i, guild, &channel_id, true).await
    }

    pub async fn handle_reset(&self, i: &ComponentInteraction) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let (action, channel_id) = parts(&i.data.custom_id);
        let Some(guild) = self.authorized_guild(i.guild_id, &channel_id, i.user.id)? else {
            i.create_response(http, not_owner()).await?;
            return Ok(());
        };
        let cleared = |content: &str| {
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new().content(content).embeds(vec![]).components(vec![]),
            )
        };
        if action == "cancel" {
            i.create_response(http, cleared("Reset cancelled.")).await?;
            return Ok(());
        }
        let (gid, uid) = (guild.to_string(), i.user.id.to_string());
        self.repo.reset_preference(&gid, &uid)?;
        self.clear_owner_cache(&gid, &uid);
        if let Some(channel) = self.live_voice(guild, &channel_id) {
            let name = self.available_name(guild);
            let _ = channel.id.edit(http, EditChannel::new().name(name).audit_log_reason("Gaia room settings reset")).await;
        }
        self.apply_and_refresh(guild, &channel_id, &uid).await?;
        i.create_response(http, cleared("Your room settings were reset for this server.")).await?;
        Ok(())
    }

    async fn show_access(&self, i: &ComponentInteraction, guild: GuildId, channel_id: &str, update: bool) -> anyhow::Result<()> {
        let access = self.access(&guild.to_string(), &i.user.id.to_string())?;
        let format = |ids: &[String]| if ids.is_empty() { "None".to_string() } else { ids.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>().join("\n") };
        let label = |id: &String| {
            let name = id.parse::<UserId>().ok().and_then(|u| Some(self.discord.cache.guild(guild)?.members.get(&u)?.display_name().to_string()));
            name.unwrap_or_else(|| format!("User {id}"))
        };
        let menu = |list: &str, placeholder: &str, ids: &[String]| {
            let options = ids.iter().take(25).map(|id| CreateSelectMenuOption::new(label(id), id)).collect();
            CreateActionRow::SelectMenu(
                CreateSelectMenu::new(format!("roomaccess:{list}:{channel_id}"), CreateSelectMenuKind::String { options }).placeholder(placeholder),
            )
        };
        let mut rows = Vec::new();
        if !access.allowed.is_empty() {
            rows.push(menu("allowed", "Remove an allowed user", &access.allowed));
        }
        if !access.denied.is_empty() {
            rows.push(menu("denied", "Remove a denied user", &access.denied));
        }
        let embed = CreateEmbed::new()
            .color(BRAND)
            .title("Manage Room Access")
            .field("Allowed", format(&access.allowed), true)
            .field("Denied", format(&access.denied), true);
        let message = CreateInteractionResponseMessage::new().embed(embed).components(rows);
        let is_string_select = matches!(i.data.kind, ComponentInteractionDataKind::StringSelect { .. });
        let response = if update && is_string_select {
            CreateInteractionResponse::UpdateMessage(message)
        } else {
            CreateInteractionResponse::Message(message.ephemeral(true))
        };
        i.create_response(&self.discord.http, response).await?;
        Ok(())
    }

    fn limit_modal(channel_id: &str) -> CreateInteractionResponse {
        let input = CreateInputText::new(InputTextStyle::Short, "User limit (0–25)", "limit").required(true).max_length(2);
        CreateInteractionResponse::Modal(
            CreateModal::new(format!("roommodal:limit:{channel_id}"), "Set Room User Limit").components(vec![CreateActionRow::InputText(input)]),
        )
    }

    fn rename_modal(channel_id: &str) -> CreateInteractionResponse {
        let input = CreateInputText::new(InputTextStyle::Short, "Room name", "name").required(true).max_length(20);
        CreateInteractionResponse::Modal(
            CreateModal::new(format!("roommodal:rename:{channel_id}"), "Room Name").components(vec![CreateActionRow::InputText(input)]),
        )
    }

    fn config(&self, guild_id: &str) -> anyhow::Result<GuildConfig> {
        if let Some(cached) = self.configs.lock().unwrap().get(guild_id) {
            return Ok(cached.clone());
        }
        let value = GuildConfig {
            enabled: self.repo.settings(guild_id)?.voice_rooms_enabled != 0,
            join_channel_id: self.repo.join_channel(guild_id)?,
        };
        self.configs.lock().unwrap().insert(guild_id.to_string(), value.clone());
        Ok(value)
    }

    fn preference(&self, guild_id: &str, user_id: &str) -> anyhow::Result<VoicePreference> {
        let key = Self::key(guild_id, user_id);
        if let Some(cached) = self.prefs.lock().unwrap().get(&key) {
            return Ok(cached.clone());
        }
        let value = self.repo.preference(guild_id, user_id)?;
        self.prefs.lock().unwrap().insert(key, value.clone());
        Ok(value)
    }

    fn access(&self, guild_id: &str, user_id: &str) -> anyhow::Result<CachedAccess> {
        let key = Self::key(guild_id, user_id);
        if let Some(cached) = self.accesses.lock().unwrap().get(&key) {
            return Ok(cached.clone());
        }
        let raw = self.repo.access(guild_id, user_id)?;
        let value = CachedAccess { allowed: raw.allowed, denied: raw.denied };
        self.accesses.lock().unwrap().insert(key, value.clone());
        Ok(value)
    }

    fn set_preference(&self, guild_id: &str, user_id: &str, field: &str, value: rusqlite::types::Value) -> anyhow::Result<()> {
        self.repo.update_preference(guild_id, user_id, field, value.clone())?;
        let mut p = self.preference(guild_id, user_id)?;
        match (field, value) {
            ("is_locked", rusqlite::types::Value::Integer(v)) => p.is_locked = v,
            ("user_limit", rusqlite::types::Value::Integer(v)) => p.user_limit = v,
            ("custom_name", rusqlite::types::Value::Text(v)) => p.custom_name = Some(v),
            ("custom_name", _) => p.custom_name = None,
            _ => {}
        }
        self.prefs.lock().unwrap().insert(Self::key(guild_id, user_id), p);
        Ok(())
    }

    fn set_access(&self, guild_id: &str, owner_id: &str, user_id: &str, action: &str, present: bool) -> anyhow::Result<()> {
        let (kind, opposite) = if action == "allow" { ("allowed", "denied") } else { ("denied", "allowed") };
        self.repo.set_access(guild_id, owner_id, user_id, kind, present)?;
        if present {
            self.repo.set_access(guild_id, owner_id, user_id, opposite, false)?;
        }
        let mut access = self.access(guild_id, owner_id)?;
        let (list, other) = if kind == "allowed" { (&mut access.allowed, &mut access.denied) } else { (&mut access.denied, &mut access.allowed) };
        if present {
            if !list.iter().any(|id| id == user_id) {
                list.push(user_id.to_string());
            }
            other.retain(|id| id != user_id);
        } else {
            list.retain(|id| id != user_id);
        }
        self.accesses.lock().unwrap().insert(Self::key(guild_id, owner_id), access);
        Ok(())
    }

    async fn apply_and_refresh(&self, guild: GuildId, channel_id: &str, owner_id: &str) -> anyhow::Result<()> {
        if let Some(channel) = self.live_voice(guild, channel_id) {
            self.apply_preferences(guild, &channel, owner_id, None).await?;
            self.refresh_panel(guild, channel_id).await?;
        }
        Ok(())
    }

    async fn apply_preferences(
        &self,
        guild: GuildId,
        channel: &GuildChannel,
        owner_id: &str,
        previous_verification_role_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let gid = guild.to_string();
        let http = &self.discord.http;
        let p = self.preference(&gid, owner_id)?;
        let access = self.access(&gid, owner_id)?;
        let verification = self.repo.verification(&gid)?;
        let muted: HashSet<u64> = self.repo.active_mutes(&gid)?.iter().filter_map(|m| m.user_id.parse().ok()).collect();
        channel.id.edit(http, EditChannel::new().user_limit(p.user_limit as u32)).await?;
        let mut channel = channel.clone();
        let stale: Vec<u64> = channel
            .permission_overwrites
            .iter()
            .map(overwrite_target)
            .filter(|(id, is_role)| !is_role && !muted.contains(id))
            .map(|(id, _)| id)
            .collect();
        for chunk in stale.chunks(10) {
            join_all(chunk.iter().map(|id| http.delete_permission(channel.id, TargetId::new(*id), Some("Gaia room access sync")))).await;
        }
        channel.permission_overwrites.retain(|o| !stale.contains(&overwrite_target(o).0));
        let role_exists = |id: &str| {
            id.parse::<RoleId>().ok().is_some_and(|r| self.discord.cache.guild(guild).is_some_and(|g| g.roles.contains_key(&r)))
        };
        let restricted = verification.enabled != 0 && verification.role_id.as_deref().is_some_and(role_exists);
        let locked = p.is_locked != 0;
        let connect = Permissions::CONNECT;
        let everyone = guild.everyone_role().get();
        edit_overwrite(&self.discord, &channel, everyone, true, connect, if locked || restricted { Some(false) } else { None }, None).await?;
        if let Some(previous) = previous_verification_role_id.filter(|p| Some(*p) != verification.role_id.as_deref()) {
            if let Ok(id) = previous.parse::<u64>() {
                let _ = edit_overwrite(&self.discord, &channel, id, true, connect, None, None).await;
            }
        }
        if let Some(Ok(id)) = verification.role_id.as_deref().map(str::parse::<u64>) {
            let value = if restricted && !locked { Some(true) } else { None };
            let _ = edit_overwrite(&self.discord, &channel, id, true, connect, value, None).await;
        }
        edit_overwrite(&self.discord, &channel, owner_id.parse()?, false, connect, Some(true), None).await?;
        let mut seen = HashSet::new();
        for id in access.allowed.iter().chain(&access.denied) {
            let Ok(target) = id.parse::<u64>() else { continue };
            if seen.insert(target) && !muted.contains(&target) {
                edit_overwrite(&self.discord, &channel, target, false, connect, Some(!access.denied.contains(id)), None).await?;
            }
        }
        Ok(())
    }

    fn overwrites(
        &self,
        guild: GuildId,
        owner_id: &str,
        p: &VoicePreference,
        access: &CachedAccess,
        verification_role_id: Option<&str>,
    ) -> Vec<PermissionOverwrite> {
        let roles: HashSet<String> =
            self.discord.cache.guild(guild).map(|g| g.roles.keys().map(|r| r.to_string()).collect()).unwrap_or_default();
        let everyone = guild.everyone_role().to_string();
        let exists = verification_role_id.is_some_and(|r| roles.contains(r));
        let flags = |list: &[&str]| if list.is_empty() { Permissions::empty() } else { Permissions::CONNECT };
        room_access_plan(&everyone, owner_id, p.is_locked, access, verification_role_id, exists)
            .into_iter()
            .filter_map(|entry| {
                let id: u64 = entry.id.parse().ok()?;
                let kind = if entry.id == everyone || roles.contains(&entry.id) {
                    PermissionOverwriteType::Role(RoleId::new(id))
                } else {
                    PermissionOverwriteType::Member(UserId::new(id))
                };
                Some(PermissionOverwrite { allow: flags(&entry.allow), deny: flags(&entry.deny), kind })
            })
            .collect()
    }

    async fn refresh_panel(&self, guild: GuildId, channel_id: &str) -> anyhow::Result<()> {
        let gid = guild.to_string();
        let active = self.repo.active_room(&gid, channel_id)?;
        let (Some(active), Some(channel)) = (active, self.live_voice(guild, channel_id)) else { return Ok(()) };
        let Some(message_id) = active.control_message_id.clone().and_then(|m| m.parse::<MessageId>().ok()) else { return Ok(()) };
        let context = || ctx! {"service" => "voice-room", "guildId" => gid, "channelId" => channel_id, "messageId" => message_id.to_string()};
        if let Err(e) = self.discord.http.get_message(channel.id, message_id).await {
            logger::log_discord_error("voice-room.panel.fetch", &e.into(), context());
            self.repo.set_room_control_message(&gid, channel_id, None)?;
            return Ok(());
        }
        let edit = EditMessage::new()
            .content(format!("<@{}>", active.owner_id))
            .allowed_mentions(CreateAllowedMentions::new())
            .embed(self.panel_embed(guild, &active.owner_id)?)
            .components(Self::controls(channel_id));
        if let Err(e) = channel.id.edit_message(&self.discord.http, message_id, edit).await {
            logger::log_discord_error("voice-room.panel.edit", &e.into(), context());
            self.repo.set_room_control_message(&gid, channel_id, None)?;
        }
        Ok(())
    }

    fn panel_embed(&self, guild: GuildId, owner_id: &str) -> anyhow::Result<CreateEmbed> {
        let gid = guild.to_string();
        let p = self.preference(&gid, owner_id)?;
        let room = self.repo.active_room_for_owner(&gid, owner_id)?;
        let fallback = p.custom_name.clone().unwrap_or_else(|| "Room".into());
        let room_name = room
            .and_then(|r| self.live_channel(guild, r.channel_id.parse().ok()?).map(|c| c.name))
            .unwrap_or(fallback);
        Ok(CreateEmbed::new()
            .color(BRAND)
            .title("🌿 Gaia Voice Room")
            .field("Owner", format!("<@{owner_id}>"), false)
            .field("Room", room_name, false)
            .field("Access", if p.is_locked != 0 { "🔴 Locked" } else { "🟢 Unlocked" }, true)
            .field("User Limit", if p.user_limit != 0 { format!("🟢 {} users", p.user_limit) } else { "🟢 Unlimited".into() }, true)
            .footer(CreateEmbedFooter::new("Your room settings are saved for the next room you create.")))
    }

    fn controls(channel_id: &str) -> Vec<CreateActionRow> {
        let button = |action: &str, label: &str, style| CreateButton::new(format!("room:{action}:{channel_id}")).label(label).style(style);
        vec![
            CreateActionRow::Buttons(vec![
                button("toggle", "Lock / Unlock", ButtonStyle::Secondary),
                button("limit", "User Limit", ButtonStyle::Primary),
                button("rename", "Rename", ButtonStyle::Primary),
                button("allow", "Allow User", ButtonStyle::Success),
                button("deny", "Deny User", ButtonStyle::Danger),
            ]),
            CreateActionRow::Buttons(vec![
                button("manage", "Manage Access", ButtonStyle::Secondary),
                button("reset", "Reset Settings", ButtonStyle::Danger),
            ]),
        ]
    }

    /// TS `authorizedRoom` plus the `!i.guild || !channelId` guard: the guild when `user` owns the room.
    fn authorized_guild(&self, guild: Option<GuildId>, channel_id: &str, user: UserId) -> anyhow::Result<Option<GuildId>> {
        let Some(guild) = guild.filter(|_| !channel_id.is_empty()) else { return Ok(None) };
        Ok(self.authorized_room(guild, channel_id, user)?.map(|_| guild))
    }

    fn authorized_room(&self, guild: GuildId, channel_id: &str, user: UserId) -> anyhow::Result<Option<ActiveRoom>> {
        let room = self.repo.active_room(&guild.to_string(), channel_id)?;
        Ok(room.filter(|r| r.owner_id == user.to_string()))
    }

    fn available_name(&self, guild: GuildId) -> String {
        let used: HashSet<u64> = self
            .discord
            .cache
            .guild(guild)
            .map(|g| g.channels.values().filter_map(|c| ROOM_NAME.captures(&c.name)?[1].parse().ok()).collect())
            .unwrap_or_default();
        let mut n = 1;
        while used.contains(&n) {
            n += 1;
        }
        format!("Room {n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access() -> CachedAccess {
        CachedAccess { allowed: vec!["allowed".into()], denied: vec!["denied".into()] }
    }
    fn e(id: &str, allow: bool, deny: bool) -> AccessPlanEntry {
        AccessPlanEntry {
            id: id.into(),
            allow: if allow { vec!["Connect"] } else { vec![] },
            deny: if deny { vec!["Connect"] } else { vec![] },
        }
    }

    #[test]
    fn keeps_normal_unlocked_baseline_access_when_verification_is_off() {
        assert_eq!(
            room_access_plan("everyone", "owner", 0, &access(), None, false),
            vec![e("everyone", false, false), e("owner", true, false), e("allowed", true, false), e("denied", false, true)]
        );
    }

    #[test]
    fn keeps_an_unlocked_room_verification_restricted_while_preserving_explicit_access() {
        assert_eq!(
            room_access_plan("everyone", "owner", 0, &access(), Some("verified"), true),
            vec![
                e("everyone", false, true),
                e("owner", true, false),
                e("verified", true, false),
                e("allowed", true, false),
                e("denied", false, true)
            ]
        );
    }

    #[test]
    fn does_not_grant_the_verification_role_access_to_a_locked_room() {
        let plan = room_access_plan("everyone", "owner", 1, &access(), Some("verified"), true);
        assert!(plan.contains(&e("everyone", false, true)));
        assert!(!plan.iter().any(|p| p.id == "verified"));
        assert!(plan.contains(&e("allowed", true, false)));
        assert!(plan.contains(&e("denied", false, true)));
    }

    #[test]
    fn falls_back_safely_when_the_configured_verification_role_is_missing() {
        let plan = room_access_plan("everyone", "owner", 0, &access(), Some("missing-role"), false);
        assert!(plan.contains(&e("everyone", false, false)));
        assert!(!plan.iter().any(|p| p.id == "missing-role"));
    }
}
