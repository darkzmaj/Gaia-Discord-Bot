import 'dotenv/config';
import { validateAuditLogConfig } from "./services/audit-log-service.js";

const optionalEnv = (value: string | undefined): string | undefined => value?.trim();
const auditLogging = {
  webhookUrl: optionalEnv(process.env.LOG_WEBHOOK_URL),
  guildId: optionalEnv(process.env.LOG_GUILD),
  channelId: optionalEnv(process.env.LOG_GUILD_CHANNEL),
};

export const config = {
  token: process.env.DISCORD_TOKEN ?? '',
  databasePath: process.env.DATABASE_PATH ?? './data/cookie.sqlite',
  applicationId: process.env.APPLICATION_ID ?? process.env.CLIENT_ID ?? process.env.DISCORD_CLIENT_ID,
  logLevel: process.env.LOG_LEVEL ?? 'info',
  auditLogging,
  auditLoggingValidation: validateAuditLogConfig(auditLogging),
};

export function requireConfig(): void {
  if (!config.token.trim()) throw new Error('DISCORD_TOKEN is required. See .env.example.');
  if (!config.databasePath.trim()) throw new Error('DATABASE_PATH cannot be empty. See .env.example.');
}
