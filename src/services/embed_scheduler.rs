use super::audit_log_service::{AuditEvent, AuditLogService, AuditSubject};
use super::embed_renderer::{parse_mention, render_template, resolve_mention, with_delivery_mention};
use crate::db::repositories::{GaiaRepository, ScheduleUpdate, ScheduledEmbedSend};
use crate::discord::Discord;
use crate::logger;
use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serenity::model::channel::ChannelType;
use serenity::model::id::{ChannelId, GuildId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

pub const SCHEDULE_POLL_MS: u64 = 30_000;
pub const MISSED_SCHEDULE_GRACE_MS: i64 = 24 * 60 * 60_000;

fn reason(error: &anyhow::Error) -> String {
    error.to_string().replace(['\r', '\n'], " ").chars().take(300).collect()
}

fn error_code(error: &anyhow::Error) -> String {
    error.downcast_ref::<serenity::Error>().and_then(logger::discord_code).map(|c| c.to_string()).unwrap_or_else(|| "DELIVERY_FAILED".into())
}

fn snowflake<T: From<u64>>(id: &str) -> Option<T> {
    id.parse::<u64>().ok().filter(|&n| n != 0).map(T::from)
}

/// Durable, DB-claimed delivery loop. Ambiguous processing work is never auto-retried.
pub struct EmbedScheduler {
    repo: Arc<GaiaRepository>,
    discord: Discord,
    audit: Option<Arc<AuditLogService>>,
    timer: Mutex<Option<JoinHandle<()>>>,
    stopping: AtomicBool,
}

impl EmbedScheduler {
    pub fn new(repo: Arc<GaiaRepository>, discord: Discord, audit: Option<Arc<AuditLogService>>) -> Self {
        Self { repo, discord, audit, timer: Mutex::new(None), stopping: AtomicBool::new(false) }
    }

    pub async fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        self.stopping.store(false, Ordering::SeqCst);
        let stale = self.repo.reconcile_stale_schedule_processing(&crate::db::now())?;
        logger::info("[scheduler] started", crate::ctx! {"stale" => stale});
        self.tick(Utc::now()).await?;
        let scheduler = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(SCHEDULE_POLL_MS)).await;
                if let Err(error) = scheduler.tick(Utc::now()).await {
                    let name = if error.downcast_ref::<serenity::Error>().is_some() { "DiscordAPIError" } else { "Error" };
                    logger::error("[scheduler] tick failed", crate::ctx! {"error" => name});
                }
            }
        });
        if let Some(old) = self.timer.lock().unwrap().replace(handle) {
            old.abort();
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(timer) = self.timer.lock().unwrap().take() {
            timer.abort();
        }
    }

    pub async fn tick(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        if self.stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        loop {
            let Some(schedule) = self.repo.claim_due_schedule(&stamp)? else { return Ok(()) };
            if self.stopping.load(Ordering::SeqCst) {
                return Ok(());
            }
            self.deliver(schedule, now).await?;
        }
    }

    async fn deliver(&self, schedule: ScheduledEmbedSend, now: DateTime<Utc>) -> anyhow::Result<()> {
        logger::info("[scheduler] schedule claimed", crate::ctx! {"id" => schedule.id});
        let missed = DateTime::parse_from_rfc3339(&schedule.scheduled_for_utc)
            .is_ok_and(|at| (now - at.with_timezone(&Utc)).num_milliseconds() > MISSED_SCHEDULE_GRACE_MS);
        if missed {
            self.repo.update_schedule(&schedule.id, "scheduler", &ScheduleUpdate {
                status: Some("missed".into()),
                failure_code: Some(Some("MISSED_GRACE_EXCEEDED".into())),
                failure_reason: Some(Some("Gaia was offline for more than 24 hours after the scheduled time.".into())),
                ..Default::default()
            })?;
            return Ok(());
        }
        match self.send(&schedule).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let code = error_code(&error);
                self.repo.update_schedule(&schedule.id, "scheduler", &ScheduleUpdate {
                    status: Some("failed".into()),
                    failure_code: Some(Some(code.chars().take(60).collect())),
                    failure_reason: Some(Some(reason(&error))),
                    ..Default::default()
                })?;
                logger::warn("[scheduler] failed", crate::ctx! {"id" => schedule.id, "code" => code});
                if let Some(audit) = &self.audit {
                    audit.record(AuditEvent {
                        title: "Scheduled Embed Failed".into(),
                        category: "Embeds".into(),
                        color: "danger".into(),
                        priority: "critical".into(),
                        guild: Some(AuditSubject::id(&schedule.guild_id)),
                        channel: Some(AuditSubject::id(&schedule.channel_id)),
                        reason: Some(reason(&error)),
                        details: vec![("Template".into(), schedule.template_name_snapshot.clone().into()), ("Schedule".into(), schedule.id.clone().into())],
                        ..Default::default()
                    });
                }
                Ok(())
            }
        }
    }

    async fn send(&self, schedule: &ScheduledEmbedSend) -> anyhow::Result<()> {
        let template = self.repo.embed_by_id(&schedule.guild_id, &schedule.template_id)?.ok_or_else(|| anyhow!("The referenced embed template no longer exists."))?;
        let bot_id = self.discord.cache.current_user().id;
        // Resolve everything from the cache before any await (cache guards are not Send).
        let (guild_id, guild_name, channel_id, channel_name, resolved) = {
            let guild = snowflake::<GuildId>(&schedule.guild_id)
                .and_then(|id| self.discord.cache.guild(id))
                .ok_or_else(|| anyhow!("Destination guild is unavailable."))?;
            let channel_id = snowflake::<ChannelId>(&schedule.channel_id);
            let channel = channel_id.and_then(|id| guild.channels.get(&id).or_else(|| guild.threads.iter().find(|t| t.id == id)));
            let channel = channel
                .filter(|c| c.guild_id == guild.id)
                .filter(|c| {
                    matches!(
                        c.kind,
                        ChannelType::Text | ChannelType::News | ChannelType::Voice | ChannelType::Stage | ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
                    )
                })
                .ok_or_else(|| anyhow!("Destination channel is unavailable or cannot receive messages."))?;
            let resolved = resolve_mention(&guild, bot_id, &parse_mention(schedule.mention_json.as_deref()));
            (guild.id, guild.name.clone(), channel.id, channel.name.clone(), resolved)
        };
        let rendered = render_template(&self.repo, &schedule.guild_id, &template.template_id, &template.payload_json).await?;
        let message = channel_id.send_message(&self.discord.http, with_delivery_mention(rendered, &resolved.mention).create_message()).await?;
        let (guild, channel, message_id) = (guild_id.to_string(), channel_id.to_string(), message.id.to_string());
        self.repo.track_sent_embed(&template.template_id, &guild, &channel, &message_id)?;
        let outcome = resolved.warning.clone().unwrap_or_else(|| format!("Delivered: {}", serde_json::to_string(&resolved.mention).unwrap_or_default()));
        self.repo.update_schedule(&schedule.id, "scheduler", &ScheduleUpdate {
            status: Some("sent".into()),
            sent_at: Some(Some(crate::db::now())),
            sent_message_id: Some(Some(message_id)),
            failure_code: Some(None),
            failure_reason: Some(None),
            mention_outcome: Some(Some(outcome)),
            ..Default::default()
        })?;
        logger::info("[scheduler] sent", crate::ctx! {"id" => schedule.id, "guild" => guild, "channel" => channel});
        if let Some(audit) = &self.audit {
            audit.record(AuditEvent {
                title: "Scheduled Embed Delivered".into(),
                category: "Embeds".into(),
                color: "success".into(),
                guild: Some(AuditSubject::named(guild, guild_name)),
                channel: Some(AuditSubject::named(channel, channel_name)),
                details: vec![("Template".into(), schedule.template_name_snapshot.clone().into()), ("Schedule".into(), schedule.id.clone().into())],
                ..Default::default()
            });
        }
        Ok(())
    }
}
