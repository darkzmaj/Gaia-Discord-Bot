export const MAX_MUTE_MS = 30 * 24 * 60 * 60 * 1000;
const unitMs: Record<string, number> = { d: 86_400_000, h: 3_600_000, m: 60_000, s: 1_000 };

export function parseDuration(input: string): number | null {
  const normalized = input.trim().toLowerCase();
  if (!normalized) return null;
  const parts = [...normalized.matchAll(/(\d+)\s*([dhms])/g)];
  if (!parts.length || parts.map((part) => part[0]).join('').replace(/\s/g, '') !== normalized.replace(/\s/g, '')) return null;
  let total = 0;
  for (const [, amount, unit] of parts) {
    const value = Number(amount);
    if (!Number.isSafeInteger(value)) return null;
    total += value * unitMs[unit];
    if (total > MAX_MUTE_MS) return null;
  }
  return total > 0 ? total : null;
}

export function humanDuration(ms: number): string {
  const parts: string[] = [];
  for (const [name, divisor] of [['day', 86_400_000], ['hour', 3_600_000], ['minute', 60_000], ['second', 1_000]] as const) {
    const n = Math.floor(ms / divisor); ms %= divisor;
    if (n) parts.push(`${n} ${name}${n === 1 ? '' : 's'}`);
  }
  return parts.slice(0, 3).join(' ') || '0 seconds';
}

export const discordTimestampSeconds = (date: Date) => Math.floor(date.getTime() / 1000);
export function ljubljanaDate(date: Date): string { return new Intl.DateTimeFormat('en-GB', { timeZone: 'Europe/Ljubljana', dateStyle: 'medium', timeStyle: 'short' }).format(date); }
