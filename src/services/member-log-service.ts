import { EmbedBuilder } from "discord.js";
import type { GuildMember, PartialGuildMember } from "discord.js";
import { logger } from "../logger.js";
import { sanitizeAuditText } from "./audit-log-service.js";

export type MemberLogConfig = {
  webhookUrl?: string;
  channelId?: string;
};
export type MemberLogFetch = (input: string, init?: RequestInit) => Promise<Response>;
export type MemberLogConfigValidation = ReturnType<typeof validateMemberLogConfig>;

const MAX_QUEUE = 100;
const RETRIES = 3;
const SNOWFLAKE = /^\d{17,20}$/;
const WEBHOOK = /^https:\/\/(?:canary\.|ptb\.)?discord(?:app)?\.com\/api\/webhooks\/\d{17,20}\/[A-Za-z0-9_\-.]+\/?(?:\?.*)?$/i;
const colors = { joined: 0x57f287, left: 0xed4245 } as const;

export function validateMemberLogConfig(config: MemberLogConfig):
  | { enabled: false; reason: string }
  | { enabled: true; config: Required<MemberLogConfig> } {
  const webhookUrl = config.webhookUrl?.trim();
  const channelId = config.channelId?.trim();
  if (!webhookUrl && !channelId) return { enabled: false, reason: "not configured" };
  if (!webhookUrl || !channelId) return { enabled: false, reason: "incomplete configuration" };
  if (!SNOWFLAKE.test(channelId)) return { enabled: false, reason: "invalid channel ID" };
  if (!WEBHOOK.test(webhookUrl)) return { enabled: false, reason: "invalid webhook URL" };
  return { enabled: true, config: { webhookUrl, channelId } };
}

type MemberLogEvent = {
  kind: "joined" | "left";
  member: GuildMember | PartialGuildMember;
  occurredAt: Date;
};

/**
 * Best-effort centralized member-event delivery. This deliberately uses only
 * webhook credentials, so Cookie never needs access to the logging guild.
 */
export class MemberLogService {
  private config?: Required<MemberLogConfig>;
  private readonly queue: MemberLogEvent[] = [];
  private working?: Promise<void>;
  private stopping = false;
  private destinationValidated = false;
  private validationTimer?: NodeJS.Timeout;
  private validationAttempts = 0;

  constructor(
    configured: MemberLogConfig,
    private readonly request: MemberLogFetch = fetch,
    checked: MemberLogConfigValidation = validateMemberLogConfig(configured),
  ) {
    if (checked.enabled) this.config = checked.config;
    else if (checked.reason !== "not configured")
      logger.warn("[member-logging] disabled", { reason: checked.reason });
  }

  get enabled(): boolean {
    return !!this.config && this.destinationValidated && !this.stopping;
  }

  /** Validates the configured target through the webhook itself, never Discord guild membership. */
  async initialize(): Promise<void> {
    if (!this.config || this.stopping || this.destinationValidated) return;
    const configured = this.config;
    try {
      const response = await this.request(configured.webhookUrl, {
        signal: AbortSignal.timeout(5_000),
      });
      if (!response.ok) {
        if (response.status === 429 || response.status >= 500) this.validationFailed(true);
        else this.permanentlyDisable("webhook unavailable");
        return;
      }
      const webhook = (await response.json()) as { channel_id?: unknown };
      if (webhook.channel_id !== configured.channelId) {
        this.permanentlyDisable("webhook channel mismatch");
        return;
      }
      this.destinationValidated = true;
      this.validationAttempts = 0;
      logger.info("[member-logging] destination verified", { channelId: configured.channelId });
      this.startWorker();
    } catch {
      this.validationFailed(true);
    }
  }

  recordJoin(member: GuildMember, occurredAt = new Date()): void {
    this.record("joined", member, occurredAt);
  }

  recordLeave(member: GuildMember | PartialGuildMember, occurredAt = new Date()): void {
    this.record("left", member, occurredAt);
  }

  private record(kind: MemberLogEvent["kind"], member: GuildMember | PartialGuildMember, occurredAt: Date): void {
    if (!this.config || this.stopping || member.user.bot) return;
    if (this.queue.length >= MAX_QUEUE) {
      logger.warn("[member-logging] queue full; dropped event", { event: kind });
      return;
    }
    this.queue.push({ kind, member, occurredAt });
    this.startWorker();
  }

  private startWorker(): void {
    if (!this.enabled || this.working || !this.queue.length) return;
    this.working = this.drain()
      .catch((error) => logger.warn("[member-logging] worker stopped unexpectedly", { error: sanitizeAuditText((error as Error)?.name, 80) }))
      .finally(() => (this.working = undefined));
  }

  async shutdown(timeoutMs = 2_000): Promise<void> {
    this.stopping = true;
    if (this.validationTimer) clearTimeout(this.validationTimer);
    if (this.working)
      await Promise.race([
        this.working.catch(() => undefined),
        new Promise<void>((resolve) => setTimeout(resolve, timeoutMs)),
      ]);
    this.queue.length = 0;
  }

  private permanentlyDisable(reason: string): void {
    this.config = undefined;
    this.destinationValidated = false;
    this.queue.length = 0;
    logger.warn("[member-logging] disabled", { reason });
  }

  private validationFailed(retry: boolean): void {
    const willRetry = retry && !this.stopping && this.validationAttempts < RETRIES;
    logger.warn("[member-logging] webhook unavailable", { retrying: willRetry });
    if (!willRetry) return;
    this.validationAttempts++;
    this.validationTimer = setTimeout(() => {
      this.validationTimer = undefined;
      void this.initialize().catch(() => undefined);
    }, 2_000 * this.validationAttempts);
  }

  private async drain(): Promise<void> {
    while (this.queue.length) {
      const event = this.queue.shift();
      if (event) await this.send(event);
    }
  }

  private async send(event: MemberLogEvent): Promise<void> {
    const url = this.config?.webhookUrl;
    if (!url) return;
    const payload = JSON.stringify({
      embeds: [this.embed(event)],
      allowed_mentions: { parse: [], users: [], roles: [], replied_user: false },
    });
    for (let attempt = 0; attempt <= RETRIES; attempt++) {
      try {
        const response = await this.request(url, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: payload,
          signal: AbortSignal.timeout(5_000),
        });
        if (response.ok) return;
        if (response.status !== 429 && response.status < 500) {
          logger.warn("[member-logging] delivery dropped", { status: response.status, event: event.kind });
          return;
        }
        if (attempt === RETRIES) {
          logger.warn("[member-logging] delivery retries exhausted", { status: response.status, event: event.kind });
          return;
        }
        const retryAfter = Number(response.headers.get("retry-after"));
        await this.delay(Number.isFinite(retryAfter) ? Math.min(retryAfter * 1_000, 5_000) : 150 * 2 ** attempt);
      } catch (error) {
        if (attempt === RETRIES) {
          logger.warn("[member-logging] delivery retries exhausted", { event: event.kind, error: sanitizeAuditText((error as Error)?.name, 80) });
          return;
        }
        await this.delay(150 * 2 ** attempt);
      }
    }
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  private embed({ kind, member, occurredAt }: MemberLogEvent): ReturnType<EmbedBuilder["toJSON"]> {
    const user = member.user;
    const username = sanitizeAuditText(user.username, 160);
    const guildName = sanitizeAuditText(member.guild.name, 160);
    const timestamp = Math.floor(occurredAt.getTime() / 1_000);
    return new EmbedBuilder()
      .setColor(colors[kind])
      .setTitle(kind === "joined" ? "Member Joined" : "Member Left")
      .addFields(
        { name: "User", value: `<@${user.id}> (${username})`, inline: false },
        { name: "Guild", value: guildName, inline: false },
        { name: kind === "joined" ? "Joined" : "Left", value: `<t:${timestamp}:F>`, inline: false },
        { name: "User ID", value: user.id, inline: true },
        { name: "Guild ID", value: member.guild.id, inline: true },
      )
      .setThumbnail(user.displayAvatarURL({ extension: "png", size: 4096, forceStatic: false }))
      .setTimestamp(occurredAt)
      .setFooter({ text: "Cookie Member Log" })
      .toJSON();
  }
}
