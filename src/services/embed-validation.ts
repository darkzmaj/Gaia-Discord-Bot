import { EmbedBuilder } from 'discord.js';

export interface StoredMessage { content?: string | null; embeds?: unknown[]; allowed_mentions?: unknown; [key: string]: unknown }
const max = (value: unknown, length: number, name: string, errors: string[]) => { if (typeof value === 'string' && value.length > length) errors.push(`${name} must be at most ${length} characters.`); };
const isUrl = (value: unknown) => typeof value === 'string' && (() => { try { const u = new URL(value); return u.protocol === 'https:' || u.protocol === 'http:'; } catch { return false; } })();

/** Validates supported Discord message/embed JSON without rewriting its design. */
export function validateEmbedPayload(raw: string): { ok: true; value: StoredMessage } | { ok: false; errors: string[] } {
  let value: unknown;
  try { value = JSON.parse(raw); } catch { return { ok: false, errors: ['JSON is malformed.'] }; }
  if (!value || typeof value !== 'object' || Array.isArray(value)) return { ok: false, errors: ['JSON must be a message object.'] };
  const message = { ...(value as StoredMessage) }, errors: string[] = [];
  // Discord exports these harmless defaults. Keep meaningful unsupported data explicit.
  if (Array.isArray(message.attachments) && message.attachments.length === 0) delete message.attachments;
  if (message.tts === false) delete message.tts;
  if (message.flags === 0) delete message.flags;
  const supportedMessageKeys = new Set(['content', 'embeds', 'allowed_mentions']);
  for (const key of Object.keys(message)) if (!supportedMessageKeys.has(key)) errors.push(key === 'attachments' ? 'message.attachments must be uploaded through Cookie attachment management.' : `message.${key} is not supported.`);
  max(message.content, 2000, 'content', errors);
  if (message.embeds !== undefined && !Array.isArray(message.embeds)) errors.push('embeds must be an array.');
  const embeds = message.embeds ?? [];
  if (!message.content && !embeds.length) errors.push('Provide content, at least one embed, or both.');
  if (embeds.length > 10) errors.push('Discord allows at most 10 embeds.');
  for (const [index, embed] of embeds.entries()) {
    if (!embed || typeof embed !== 'object' || Array.isArray(embed)) { errors.push(`embeds[${index}] must be an object.`); continue; }
    const e = embed as Record<string, any>; max(e.title, 256, `embeds[${index}].title`, errors); max(e.description, 4096, `embeds[${index}].description`, errors);
    if (e.url && !isUrl(e.url)) errors.push(`embeds[${index}].url must be an http(s) URL.`);
    for (const key of ['image', 'thumbnail', 'video']) if (e[key]?.url && !isUrl(e[key].url) && !(typeof e[key].url === 'string' && e[key].url.startsWith('attachment://'))) errors.push(`embeds[${index}].${key}.url must be an http(s) URL or attachment:// filename.`);
    if (e.footer) max(e.footer.text, 2048, `embeds[${index}].footer.text`, errors);
    if (e.author) { max(e.author.name, 256, `embeds[${index}].author.name`, errors); if (e.author.url && !isUrl(e.author.url)) errors.push(`embeds[${index}].author.url must be an http(s) URL.`); }
    if (e.fields !== undefined && !Array.isArray(e.fields)) errors.push(`embeds[${index}].fields must be an array.`);
    if ((e.fields?.length ?? 0) > 25) errors.push(`embeds[${index}] has more than 25 fields.`);
    let total = (e.title?.length ?? 0) + (e.description?.length ?? 0) + (e.footer?.text?.length ?? 0) + (e.author?.name?.length ?? 0);
    for (const [fieldIndex, field] of (e.fields ?? []).entries()) { max(field?.name, 256, `embeds[${index}].fields[${fieldIndex}].name`, errors); max(field?.value, 1024, `embeds[${index}].fields[${fieldIndex}].value`, errors); total += (field?.name?.length ?? 0) + (field?.value?.length ?? 0); }
    if (total > 6000) errors.push(`embeds[${index}] totals more than 6000 characters.`);
    try { EmbedBuilder.from(e); } catch { errors.push(`embeds[${index}] is not Discord-compatible.`); }
  }
  return errors.length ? { ok: false, errors } : { ok: true, value: message };
}
