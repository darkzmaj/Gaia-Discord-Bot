import { PermissionsBitField, type GuildMember } from 'discord.js';
import type { CookieRepository } from '../db/repositories.js';

export const ADMIN_LABEL = '𝗔𝗱𝗺𝗶𝗻';
export const STAFF_LABEL = '𝗦𝘁𝗮𝗳𝗳';
export type PermissionLevel = 'global' | 'staff' | 'administrator';

/** Cookie Administrator means a current effective Discord Administrator permission, never guild ownership. */
export function isDiscordAdministrator(member: GuildMember): boolean {
  return member.permissions.has(PermissionsBitField.Flags.Administrator);
}

export const isAdministrator = isDiscordAdministrator;

export async function hasPermission(repo: CookieRepository, member: GuildMember, level: PermissionLevel): Promise<boolean> {
  if (level === 'global') return true;
  if (isDiscordAdministrator(member)) return true;
  return level === 'staff' && repo.isStaff(member.guild.id, member.id);
}
