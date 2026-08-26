import type { Client } from 'discord.js';
import type { CookieRepository, ScheduledEmbedSend } from '../db/repositories.js';
import { logger } from '../logger.js';
import { parseMention, renderTemplate, resolveMention, withDeliveryMention } from './embed-renderer.js';

export const SCHEDULE_POLL_MS = 30_000;
export const MISSED_SCHEDULE_GRACE_MS = 24 * 60 * 60_000;
const reason = (error: unknown) => String((error as any)?.message ?? 'unknown delivery failure').replace(/[\r\n]/g, ' ').slice(0, 300);

/** Durable, DB-claimed delivery loop. Ambiguous processing work is never auto-retried. */
export class EmbedScheduler {
  private timer?: NodeJS.Timeout;
  private stopping = false;
  constructor(private readonly repo: CookieRepository, private readonly client: Client) {}
  async start(): Promise<void> { this.stopping = false; const stale = await this.repo.reconcileStaleScheduleProcessing(new Date().toISOString()); logger.info('[scheduler] started', { stale }); await this.tick(); this.timer = setInterval(() => void this.tick().catch((error) => logger.error('[scheduler] tick failed', { error: error instanceof Error ? error.name : 'unknown' })), SCHEDULE_POLL_MS); }
  stop(): void { this.stopping = true; if (this.timer) clearInterval(this.timer); this.timer = undefined; }
  async tick(now = new Date()): Promise<void> { if (this.stopping) return; for (;;) { const schedule = await this.repo.claimDueSchedule(now.toISOString()); if (!schedule || this.stopping) return; await this.deliver(schedule, now); } }
  private async deliver(schedule: ScheduledEmbedSend, now: Date): Promise<void> {
    logger.info('[scheduler] schedule claimed', { id: schedule.id });
    if (now.getTime() - new Date(schedule.scheduled_for_utc).getTime() > MISSED_SCHEDULE_GRACE_MS) { await this.repo.updateSchedule(schedule.id, 'scheduler', { status: 'missed', failure_code: 'MISSED_GRACE_EXCEEDED', failure_reason: 'Cookie was offline for more than 24 hours after the scheduled time.' }); return; }
    try {
      const template = await this.repo.embedById(schedule.template_id); if (!template) throw new Error('The referenced global embed template no longer exists.');
      const guild = this.client.guilds.cache.get(schedule.guild_id); if (!guild) throw new Error('Destination guild is unavailable.');
      const channel: any = guild.channels.cache.get(schedule.channel_id); if (!channel || channel.guildId !== guild.id || !channel.isTextBased?.() || !('send' in channel)) throw new Error('Destination channel is unavailable or cannot receive messages.');
      const resolved = resolveMention(guild, parseMention(schedule.mention_json));
      const message = await channel.send(withDeliveryMention(await renderTemplate(this.repo, template), resolved.mention));
      await this.repo.trackSentEmbed(template.template_id, guild.id, channel.id, message.id);
      await this.repo.updateSchedule(schedule.id, 'scheduler', { status: 'sent', sent_at: new Date().toISOString(), sent_message_id: message.id, failure_code: null, failure_reason: null, mention_outcome: resolved.warning ?? `Delivered: ${JSON.stringify(resolved.mention)}` });
      logger.info('[scheduler] sent', { id: schedule.id, guild: guild.id, channel: channel.id });
    } catch (error) { await this.repo.updateSchedule(schedule.id, 'scheduler', { status: 'failed', failure_code: String((error as any)?.code ?? 'DELIVERY_FAILED').slice(0, 60), failure_reason: reason(error) }); logger.warn('[scheduler] failed', { id: schedule.id, code: String((error as any)?.code ?? 'DELIVERY_FAILED') }); }
  }
}
