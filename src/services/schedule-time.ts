const parts = (date: Date, zone = 'Europe/Ljubljana') => Object.fromEntries(new Intl.DateTimeFormat('en-GB', { timeZone: zone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }).formatToParts(date).filter((part) => part.type !== 'literal').map((part) => [part.type, Number(part.value)]));
export function parseLjubljanaSchedule(input: string, now = new Date()): { utc: Date; display: string } {
  const match = input.trim().match(/^(?:(\d{4})-(\d{2})-(\d{2})|(\d{2})\.(\d{2})\.(\d{4}))\s+(\d{2}):(\d{2})$/);
  if (!match) throw new Error('Use `YYYY-MM-DD HH:mm` or `DD.MM.YYYY HH:mm` in Europe/Ljubljana.');
  const year = Number(match[1] ?? match[6]), month = Number(match[2] ?? match[5]), day = Number(match[3] ?? match[4]), hour = Number(match[7]), minute = Number(match[8]);
  if (month < 1 || month > 12 || day < 1 || day > 31 || hour > 23 || minute > 59) throw new Error('The date or time is invalid.');
  const wall = Date.UTC(year, month - 1, day, hour, minute); if (new Date(wall).getUTCDate() !== day || new Date(wall).getUTCMonth() !== month - 1) throw new Error('The date is invalid.');
  const candidates = [-3, -2, -1, 0, 1].map((offset) => new Date(wall + offset * 3_600_000)).filter((candidate) => { const p = parts(candidate); return p.year === year && p.month === month && p.day === day && p.hour === hour && p.minute === minute; });
  if (!candidates.length) throw new Error('This local time does not exist because of the DST change.');
  if (candidates.length !== 1) throw new Error('This local time is ambiguous because of the DST change; choose another time.');
  if (candidates[0].getTime() <= now.getTime()) throw new Error('Scheduled time must be in the future.');
  if (candidates[0].getTime() - now.getTime() > 366 * 86400_000) throw new Error('Scheduled time must be within one year.');
  return { utc: candidates[0], display: new Intl.DateTimeFormat('en-GB', { timeZone: 'Europe/Ljubljana', dateStyle: 'long', timeStyle: 'short' }).format(candidates[0]) };
}
