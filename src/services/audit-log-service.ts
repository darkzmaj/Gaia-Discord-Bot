import { EmbedBuilder } from "discord.js";
import { logger } from "../logger.js";

export type AuditPriority = "low" | "normal" | "critical";
export type AuditColor = "success" | "warning" | "danger" | "info";
export type AuditSubject = { id?: string; name?: string };
export type AuditEvent = {
  title: string;
  category: string;
  color?: AuditColor;
  priority?: AuditPriority;
  guild?: AuditSubject;
  actor?: AuditSubject;
  target?: AuditSubject;
  channel?: AuditSubject;
  reason?: string;
  details?: Record<string, string | number | boolean | undefined>;
};
export type AuditLogConfig = {
  webhookUrl?: string;
  guildId?: string;
  channelId?: string;
};
export type AuditFetch = (input: string, init?: RequestInit) => Promise<Response>;
export type AuditLogConfigValidation = ReturnType<typeof validateAuditLogConfig>;

const MAX_QUEUE = 100;
const RETRIES = 3;
const SNOWFLAKE = /^\d{17,20}$/;
const WEBHOOK = /^https:\/\/(?:canary\.|ptb\.)?discord(?:app)?\.com\/api\/webhooks\/\d{17,20}\/[A-Za-z0-9_\-.]+\/?(?:\?.*)?$/i;
const colors: Record<AuditColor, number> = {
  success: 0x57f287,
  warning: 0xfee75c,
  danger: 0xed4245,
  info: 0x5865f2,
};

/** Removes credentials and control characters before anything leaves Cookie. */
export const sanitizeAuditText = (value: unknown, limit = 900): string =>
  String(value ?? "Unknown")
    .replace(/(?:https?:\/\/[^\s/]+)?\/api\/webhooks\/\d{17,20}\/[A-Za-z0-9_\-.]+/gi, "[redacted webhook]")
    .replace(/[A-Za-z\d_-]{20,}\.[A-Za-z\d_-]{6,}\.[A-Za-z\d_-]{20,}/g, "[redacted token]")
    .replace(/[\r\n]+/g, " ")
    .slice(0, limit);

export function validateAuditLogConfig(config: AuditLogConfig):
  | { enabled: false; reason: string }
  | { enabled: true; config: Required<AuditLogConfig> } {
  const webhookUrl = config.webhookUrl?.trim();
  const guildId = config.guildId?.trim();
  const channelId = config.channelId?.trim();
  if (!webhookUrl && !guildId && !channelId)
    return { enabled: false, reason: "not configured" };
  if (!webhookUrl || !guildId || !channelId)
    return { enabled: false, reason: "incomplete configuration" };
  if (!SNOWFLAKE.test(guildId) || !SNOWFLAKE.test(channelId))
    return { enabled: false, reason: "invalid guild or channel ID" };
  if (!WEBHOOK.test(webhookUrl))
    return { enabled: false, reason: "invalid webhook URL" };
  return { enabled: true, config: { webhookUrl, guildId, channelId } };
}

/**
 * Best-effort remote audit delivery. `record` is deliberately synchronous and
 * never throws: an unavailable audit destination cannot affect Discord work.
 */
export class AuditLogService {
  private config?: Required<AuditLogConfig>;
  private readonly queue: AuditEvent[] = [];
  private working?: Promise<void>;
  private stopping = false;
  private destinationValidated = false;
  private validationTimer?: NodeJS.Timeout;
  private validationAttempts = 0;

  constructor(
    configured: AuditLogConfig,
    private readonly request: AuditFetch = fetch,
    checked: AuditLogConfigValidation = validateAuditLogConfig(configured),
  ) {
    if (checked.enabled) this.config = checked.config;
    else if (checked.reason !== "not configured")
      logger.warn("[logging] remote audit logging disabled", { reason: checked.reason });
  }

  get enabled(): boolean {
    return !!this.config && this.destinationValidated && !this.stopping;
  }

  /** Called after clientReady. A tokened webhook is self-contained transport/authentication. */
  async initialize(): Promise<void> {
    if (!this.config || this.stopping || this.destinationValidated) return;
    const configured = this.config;
    logger.info("[logging] validating webhook destination", {
      configuredGuild: configured.guildId,
      configuredChannel: configured.channelId,
    });
    try {
      const response = await this.request(configured.webhookUrl, {
        signal: AbortSignal.timeout(5_000),
      });
      if (!response.ok) {
        if (response.status === 429 || response.status >= 500)
          this.validationFailed("temporary webhook API failure", true);
        else this.permanentlyDisable("webhook unavailable");
        return;
      }
      const webhook = (await response.json()) as { guild_id?: unknown; channel_id?: unknown };
      if (typeof webhook.guild_id !== "string" || typeof webhook.channel_id !== "string") {
        this.permanentlyDisable("webhook unavailable");
        return;
      }
      logger.info("[logging] webhook resolved");
      if (webhook.guild_id !== configured.guildId) {
        this.destinationMismatch("[logging] webhook guild mismatch", {
          configuredGuild: configured.guildId,
          webhookGuild: webhook.guild_id,
        });
        return;
      }
      if (webhook.channel_id !== configured.channelId) {
        this.destinationMismatch("[logging] webhook channel mismatch", {
          configuredChannel: configured.channelId,
          webhookChannel: webhook.channel_id,
        });
        return;
      }
      logger.info("[logging] destination verified", {
        guildId: configured.guildId,
        channelId: configured.channelId,
      });
      this.destinationValidated = true;
      this.validationAttempts = 0;
      logger.info("[logging] remote audit logging enabled", {
        guildId: configured.guildId,
        channelId: configured.channelId,
      });
      this.startWorker();
    } catch {
      this.validationFailed("temporary webhook API failure", true);
    }
  }

  record(event: AuditEvent): void {
    if (!this.config || this.stopping) return;
    const safe = this.safeEvent(event);
    if (this.queue.length >= MAX_QUEUE) {
      const low = this.queue.findIndex((item) => (item.priority ?? "normal") === "low");
      if (low >= 0) this.queue.splice(low, 1);
      else if ((safe.priority ?? "normal") === "low") return;
      else {
        logger.warn("[logging] audit queue full; dropped event", { category: safe.category });
        return;
      }
    }
    this.queue.push(safe);
    this.startWorker();
  }

  private startWorker(): void {
    if (!this.enabled || this.working || !this.queue.length) return;
    this.working = this.drain()
      .catch((error) =>
        logger.warn("[logging] audit worker stopped unexpectedly", {
          error: sanitizeAuditText((error as Error)?.name, 80),
        }),
      )
      .finally(() => (this.working = undefined));
  }

  async shutdown(timeoutMs = 2_000): Promise<void> {
    this.stopping = true;
    if (this.validationTimer) clearTimeout(this.validationTimer);
    if (!this.working) return;
    await Promise.race([
      this.working.catch(() => undefined),
      new Promise<void>((resolve) => setTimeout(resolve, timeoutMs)),
    ]);
    if (this.queue.length)
      logger.warn("[logging] audit queue flush timed out", { dropped: this.queue.length });
    this.queue.length = 0;
  }

  private permanentlyDisable(reason: string): void {
    this.config = undefined;
    this.destinationValidated = false;
    this.queue.length = 0;
    logger.warn("[logging] remote audit logging disabled", { reason: sanitizeAuditText(reason, 160) });
  }

  private destinationMismatch(message: string, context: Record<string, string>): void {
    this.config = undefined;
    this.destinationValidated = false;
    this.queue.length = 0;
    logger.warn(message, context);
  }

  private validationFailed(reason: string, retry: boolean): void {
    const willRetry =
      retry && !this.stopping && this.validationAttempts < RETRIES;
    logger.warn("[logging] remote audit logging unavailable", {
      check: reason,
      retrying: willRetry,
    });
    if (!willRetry) return;
    this.validationAttempts++;
    this.validationTimer = setTimeout(() => {
      this.validationTimer = undefined;
      void this.initialize().catch((error) =>
        logger.warn("[logging] audit validation retry failed safely", {
          error: sanitizeAuditText((error as Error)?.name, 80),
        }),
      );
    }, 2_000 * this.validationAttempts);
  }

  private async drain(): Promise<void> {
    while (this.queue.length) {
      const event = this.queue.shift();
      if (event) await this.send(event);
    }
  }

  private async send(event: AuditEvent): Promise<void> {
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
          logger.warn("[logging] audit delivery dropped", { status: response.status, category: event.category });
          return;
        }
        if (attempt === RETRIES) {
          logger.warn("[logging] audit delivery retries exhausted", { status: response.status, category: event.category });
          return;
        }
        const retryAfter = Number(response.headers.get("retry-after"));
        await this.delay(Number.isFinite(retryAfter) ? Math.min(retryAfter * 1000, 5_000) : 150 * 2 ** attempt);
      } catch (error) {
        if (attempt === RETRIES) {
          logger.warn("[logging] audit delivery retries exhausted", {
            category: event.category,
            error: sanitizeAuditText((error as Error)?.name, 80),
          });
          return;
        }
        await this.delay(150 * 2 ** attempt);
      }
    }
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  private safeEvent(event: AuditEvent): AuditEvent {
    const subject = (value?: AuditSubject): AuditSubject | undefined =>
      value ? { id: value.id ? sanitizeAuditText(value.id, 40) : undefined, name: value.name ? sanitizeAuditText(value.name, 160) : undefined } : undefined;
    return {
      ...event,
      title: sanitizeAuditText(event.title, 256),
      category: sanitizeAuditText(event.category, 80),
      guild: subject(event.guild), actor: subject(event.actor), target: subject(event.target), channel: subject(event.channel),
      reason: event.reason ? sanitizeAuditText(event.reason, 900) : undefined,
      details: Object.fromEntries(Object.entries(event.details ?? {}).map(([key, value]) => [sanitizeAuditText(key, 80), typeof value === "string" ? sanitizeAuditText(value, 500) : value])),
    };
  }

  private embed(event: AuditEvent): ReturnType<EmbedBuilder["toJSON"]> {
    const subject = (item?: AuditSubject) => item ? [item.name, item.id ? `ID: ${item.id}` : undefined].filter(Boolean).join("\n") : undefined;
    const fields = [
      ["Server", subject(event.guild)], ["Member", subject(event.target)], ["Executor", subject(event.actor)], ["Channel", subject(event.channel)], ["Reason", event.reason],
      ...Object.entries(event.details ?? {}).filter(([, value]) => value !== undefined).map(([name, value]) => [name, String(value)] as const),
    ].filter((entry): entry is [string, string] => !!entry[1]).slice(0, 25)
      .map(([name, value]) => ({ name: name.slice(0, 256), value: value.slice(0, 1024), inline: name !== "Reason" }));
    return new EmbedBuilder().setColor(colors[event.color ?? "info"]).setTitle(event.title).addFields(fields).setTimestamp().setFooter({ text: `Cookie Audit Log • ${event.category}` }).toJSON();
  }
}
