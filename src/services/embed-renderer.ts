import { createHash, randomUUID } from 'node:crypto';
import { AttachmentBuilder, EmbedBuilder, PermissionFlagsBits, type Guild } from 'discord.js';
import type { CookieRepository, EmbedAttachment } from '../db/repositories.js';
import { validateEmbedPayload, type StoredMessage } from './embed-validation.js';

export const MAX_ATTACHMENT_BYTES = 8 * 1024 * 1024;
export const MAX_TEMPLATE_ATTACHMENT_BYTES = 20 * 1024 * 1024;
export type DeliveryMention = { roles: string[]; everyone: boolean; here: boolean };
export const noMention: DeliveryMention = { roles: [], everyone: false, here: false };
export function parseMention(value?: string): DeliveryMention { try { const parsed = JSON.parse(value ?? '') as Partial<DeliveryMention>; return { roles: Array.isArray(parsed.roles) ? parsed.roles.filter((id): id is string => typeof id === 'string').slice(0, 25) : [], everyone: !!parsed.everyone, here: !!parsed.here }; } catch { return noMention; } }
export function mentionLabel(mention: DeliveryMention): string { const parts = [...mention.roles.map((id) => `<@&${id}>`), ...(mention.everyone ? ['@everyone'] : []), ...(mention.here ? ['@here'] : [])]; return parts.join(', ') || 'None'; }
export function resolveMention(guild: Guild, requested: DeliveryMention): { mention: DeliveryMention; warning?: string } { const roles = requested.roles.filter((id) => { const role = guild.roles.cache.get(id); return !!role && !role.managed; }); const massAllowed = !!guild.members.me?.permissions.has(PermissionFlagsBits.MentionEveryone); const mention = { roles, everyone: requested.everyone && massAllowed, here: requested.here && massAllowed }; const dropped = requested.roles.length - roles.length + (requested.everyone && !mention.everyone ? 1 : 0) + (requested.here && !mention.here ? 1 : 0); return { mention, warning: dropped ? 'One or more requested mentions were unavailable or Cookie no longer has permission to make a mass mention.' : undefined }; }
export function withDeliveryMention(message: { content?: string; embeds?: EmbedBuilder[]; allowedMentions?: unknown; files?: AttachmentBuilder[] }, mention: DeliveryMention) { const prefix = [mention.everyone ? '@everyone' : '', mention.here ? '@here' : '', ...mention.roles.map((id) => `<@&${id}>`)].filter(Boolean).join(' '); return { ...message, content: prefix ? `${prefix}${message.content ? `\n${message.content}` : ''}` : message.content, allowedMentions: { parse: mention.everyone || mention.here ? ['everyone'] : [], roles: mention.roles, users: [], repliedUser: false } }; }
const safeMime = new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'application/pdf', 'text/plain']);

export function sanitizeAttachmentFilename(value: string): string {
  if (value.includes('/') || value.includes('\\') || value.includes('..')) throw new Error('Attachment filename is invalid.');
  const name = value.normalize('NFKC').replace(/[\\/:*?"<>|\u0000-\u001f]/g, '_').replace(/^\.+/, '').trim();
  if (!name || name.length > 100 || name === '.' || name === '..') throw new Error('Attachment filename is invalid.');
  return name;
}
export function attachmentRecord(templateId: string, filename: string, contentType: string, data: Buffer, actor: string): EmbedAttachment {
  const clean = sanitizeAttachmentFilename(filename);
  if (!safeMime.has(contentType)) throw new Error('Attachment type is not allowed. Use PNG, JPEG, GIF, WEBP, PDF, or plain text.');
  if (!data.length || data.length > MAX_ATTACHMENT_BYTES) throw new Error(`Attachment must be between 1 byte and ${MAX_ATTACHMENT_BYTES / 1024 / 1024} MB.`);
  return { id: randomUUID(), template_id: templateId, filename: clean, content_type: contentType, size_bytes: data.length, sha256: createHash('sha256').update(data).digest('hex'), data, created_by_id: actor, created_at: new Date().toISOString() };
}
export async function renderTemplate(repo: CookieRepository, template: { template_id: string; payload_json: string }): Promise<{ content?: string; embeds?: EmbedBuilder[]; allowedMentions?: unknown; files?: AttachmentBuilder[] }> {
  const valid = validateEmbedPayload(template.payload_json);
  if (!valid.ok) throw new Error(`Template is invalid: ${valid.errors.join(' ')}`);
  const message = valid.value as StoredMessage, attachments = await repo.attachments(template.template_id);
  if (attachments.reduce((total, item) => total + item.size_bytes, 0) > MAX_TEMPLATE_ATTACHMENT_BYTES) throw new Error('Template attachment total exceeds the 20 MB limit.');
  const referenced = new Set<string>();
  for (const embed of message.embeds ?? []) for (const key of ['image', 'thumbnail', 'video']) { const url = (embed as any)?.[key]?.url; if (typeof url === 'string' && url.startsWith('attachment://')) referenced.add(url.slice('attachment://'.length)); }
  const files = attachments.map((attachment) => {
    if (createHash('sha256').update(attachment.data).digest('hex') !== attachment.sha256 || attachment.data.length !== attachment.size_bytes) throw new Error(`Attachment ${attachment.filename} failed its integrity check.`);
    return new AttachmentBuilder(attachment.data, { name: attachment.filename, description: attachment.filename });
  });
  for (const filename of referenced) if (!attachments.some((attachment) => attachment.filename === filename)) throw new Error(`Template references missing attachment ${filename}.`);
  return { content: message.content ?? undefined, embeds: (message.embeds ?? []).map((embed) => EmbedBuilder.from(embed as any)), allowedMentions: message.allowed_mentions, files: files.length ? files : undefined };
}
