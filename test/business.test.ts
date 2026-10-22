import { afterEach, describe, expect, it } from "vitest";
import { createHash } from "node:crypto";
import { GatewayIntentBits, PermissionFlagsBits, SystemChannelFlagsBitField } from "discord.js";
import { createDatabase, type SqliteDatabase } from "../src/db/database.js";
import { CookieRepository, type GuildBackup } from "../src/db/repositories.js";
import {
  discordTimestampSeconds,
  ljubljanaDate,
  MAX_MUTE_MS,
  parseDuration,
} from "../src/services/duration.js";
import { validateEmbedPayload } from "../src/services/embed-validation.js";
import {
  attachmentRecord,
  noMention,
  renderTemplate,
  resolveMention,
  sanitizeAttachmentFilename,
  withDeliveryMention,
} from "../src/services/embed-renderer.js";
import { parseLjubljanaSchedule } from "../src/services/schedule-time.js";
import {
  hasPermission,
  isDiscordAdministrator,
} from "../src/services/authorization.js";
import { commands } from "../src/commands.js";
import { ADMIN_LABEL, STAFF_LABEL } from "../src/services/authorization.js";
import {
  HONEYPOT_REJOIN_URL,
  normalizeVerificationColor,
  VerificationService,
} from "../src/services/verification-service.js";
import { roomAccessPlan } from "../src/services/voice-room-service.js";
import { MuteService } from "../src/services/mute-service.js";
import {
  BACKUP_SCHEMA_VERSION,
  BackupService,
  groupRestoreWarnings,
  mapPermissionOverwrites,
  MAX_BACKUP_BYTES,
  roleColorsFromSnapshot,
  roleCreateOptions,
  systemChannelFlagsFromSnapshot,
} from "../src/services/backup-service.js";
import {
  CommandSynchronizer,
  commandSchemaFingerprint,
} from "../src/services/command-sync.js";
import { AdminUi } from "../src/services/admin-ui.js";
import { CookieBot } from "../src/bot.js";
import { config } from "../src/config.js";
import {
  AuditLogService,
  sanitizeAuditText,
  validateAuditLogConfig,
} from "../src/services/audit-log-service.js";
import {
  MemberLogService,
  validateMemberLogConfig,
} from "../src/services/member-log-service.js";

const databases: SqliteDatabase[] = [];
async function repo(): Promise<CookieRepository> {
  const db = await createDatabase(":memory:");
  databases.push(db);
  return new CookieRepository(db);
}
afterEach(async () => {
  await Promise.all(databases.splice(0).map((db) => db.close()));
});

describe("production audit logging", () => {
  const logging = {
    webhookUrl: "https://discord.com/api/webhooks/123456789012345678/abcdefghijklmnopqrstuvwxyz",
    guildId: "123456789012345678",
    channelId: "223456789012345678",
  };
  const ok = (body: object = {}) => new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
  it("keeps logging disabled for absent, incomplete, and invalid configuration", () => {
    expect(validateAuditLogConfig({})).toMatchObject({ enabled: false, reason: "not configured" });
    expect(validateAuditLogConfig({ guildId: logging.guildId })).toMatchObject({ enabled: false, reason: "incomplete configuration" });
    expect(validateAuditLogConfig({ ...logging, webhookUrl: "https://example.com/not-a-webhook" })).toMatchObject({ enabled: false, reason: "invalid webhook URL" });
    expect(new AuditLogService({}).enabled).toBe(false);
  });
  it("rejects a webhook whose configured guild does not match", async () => {
    const service = new AuditLogService(logging, async () => ok({ guild_id: "999999999999999999", channel_id: logging.channelId }));
    await service.initialize();
    expect(service.enabled).toBe(false);
  });
  it("rejects a webhook whose configured channel does not match", async () => {
    const service = new AuditLogService(logging, async () => ok({ guild_id: logging.guildId, channel_id: "999999999999999999" }));
    await service.initialize();
    expect(service.enabled).toBe(false);
  });
  it("rejects an invalid or deleted webhook without affecting Cookie", async () => {
    const service = new AuditLogService(logging, async () => new Response(null, { status: 404 }));
    await expect(service.initialize()).resolves.toBeUndefined();
    expect(service.enabled).toBe(false);
  });
  it("handles a transient webhook API failure safely and leaves validation retryable", async () => {
    const service = new AuditLogService(logging, async () => { throw new Error("temporary network failure"); });
    await expect(service.initialize()).resolves.toBeUndefined();
    expect(service.enabled).toBe(false);
    await service.shutdown();
  });
  it("validates and sends through guild B's webhook when Cookie is only in guild A", async () => {
    const cookieGuildIds = ["333333333333333333"];
    expect(cookieGuildIds).not.toContain(logging.guildId);
    let sends = 0;
    const service = new AuditLogService(logging, async (_url, init) => {
      if (!init?.method) return ok({ guild_id: logging.guildId, channel_id: logging.channelId });
      sends++; return new Response(null, { status: 204 });
    });
    expect(service.enabled).toBe(false);
    await service.initialize();
    service.record({ title: "Cross-guild Audit", category: "Bot" });
    await (service as any).working;
    expect(service.enabled).toBe(true);
    expect(sends).toBe(1);
  });
  it("delivers compact no-ping embeds and sanitizes secrets", async () => {
    const calls: Array<{ input: string; init?: RequestInit }> = [];
    const service = new AuditLogService(logging, async (input, init) => {
      calls.push({ input, init });
      return calls.length === 1 ? ok({ guild_id: logging.guildId, channel_id: logging.channelId }) : new Response(null, { status: 204 });
    });
    await service.initialize();
    service.record({ title: "Member Muted", category: "Moderation", guild: { id: "g", name: "@everyone" }, reason: "https://discord.com/api/webhooks/123456789012345678/secret" });
    await (service as any).working;
    const payload = JSON.parse(String(calls[1].init?.body));
    expect(payload.allowed_mentions).toEqual({ parse: [], users: [], roles: [], replied_user: false });
    expect(JSON.stringify(payload)).not.toContain("/secret");
    expect(sanitizeAuditText("https://discord.com/api/webhooks/123456789012345678/secret")).toContain("redacted");
  });
  it("retries transient failures, drops permanent failures, and bounds the queue", async () => {
    let post = 0;
    const service = new AuditLogService(logging, async (_input, init) => {
      if (!init?.method) return ok({ guild_id: logging.guildId, channel_id: logging.channelId });
      post++;
      return post < 3 ? new Response(null, { status: 500 }) : new Response(null, { status: 204 });
    });
    await service.initialize();
    service.record({ title: "Restore Failed", category: "Backups", priority: "critical" });
    await (service as any).working;
    expect(post).toBe(3);
    for (let index = 0; index < 100; index++) (service as any).queue.push({ title: "noise", category: "Bot", priority: "low" });
    service.record({ title: "Mute Automatically Expired", category: "Moderation", priority: "critical" });
    expect((service as any).queue.length).toBeLessThanOrEqual(100);
  });
  it("does not let delivery failure or shutdown flush failure escape the caller", async () => {
    const service = new AuditLogService(logging, async (_input, init) => {
      if (!init?.method) return ok({ guild_id: logging.guildId, channel_id: logging.channelId });
      throw new Error("network unavailable");
    });
    await service.initialize();
    service.record({ title: "Verification Enabled", category: "Verification" });
    await expect(service.shutdown(10)).resolves.toBeUndefined();
  });
  it("does not retry permanent webhook rejections", async () => {
    let posts = 0;
    const service = new AuditLogService(logging, async (_input, init) => {
      if (!init?.method) return ok({ guild_id: logging.guildId, channel_id: logging.channelId });
      posts++; return new Response(null, { status: 400 });
    });
    await service.initialize();
    service.record({ title: "Backup Created", category: "Backups" });
    await (service as any).working;
    expect(posts).toBe(1);
  });
  it("accepts the important event categories without treating them as Discord mentions", async () => {
    const sent: unknown[] = [];
    const service = new AuditLogService(logging, async (_input, init) => {
      if (!init?.method) return ok({ guild_id: logging.guildId, channel_id: logging.channelId });
      sent.push(JSON.parse(String(init.body))); return new Response(null, { status: 204 });
    });
    await service.initialize();
    for (const [title, category] of [["Admin Configuration Changed", "Admin Configuration"], ["Embed Scheduled", "Embeds"], ["Manual Backup Created", "Backups"], ["Restore Completed With Warnings", "Backups"], ["Restore Failed", "Backups"]]) service.record({ title, category, guild: { id: "g", name: "Guild" } });
    await (service as any).working;
    expect(sent).toHaveLength(5);
  });
});

describe("centralized member logging", () => {
  const logging = {
    webhookUrl: "https://discord.com/api/webhooks/123456789012345678/abcdefghijklmnopqrstuvwxyz",
    channelId: "1481921183464030358",
  };
  const ok = (body: object = {}) => new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
  const member = (overrides: Partial<any> = {}) => {
    const value: any = {
      id: "123456789012345678",
      guild: { id: "987654321098765432", name: "Example Server" },
      user: { id: "123456789012345678", username: "dark", bot: false },
      ...overrides,
    };
    value.user.displayAvatarURL = (options: unknown) => {
      value.avatarOptions = options;
      return "https://cdn.discordapp.com/embed/avatars/0.png";
    };
    return value;
  };
  const payloadFor = async (kind: "join" | "leave", value = member()) => {
    const calls: Array<{ input: string; init?: RequestInit }> = [];
    const service = new MemberLogService(logging, async (input, init) => {
      calls.push({ input, init });
      return init?.method ? new Response(null, { status: 204 }) : ok({ channel_id: logging.channelId });
    });
    await service.initialize();
    const time = new Date("2026-08-30T12:34:56.000Z");
    if (kind === "join") service.recordJoin(value as any, time);
    else service.recordLeave(value as any, time);
    await (service as any).working;
    return { payload: JSON.parse(String(calls[1].init?.body)), service, time };
  };
  it("safely disables absent, invalid, and mismatched member webhook configuration", async () => {
    expect(validateMemberLogConfig({})).toMatchObject({ enabled: false, reason: "not configured" });
    expect(validateMemberLogConfig({ channelId: logging.channelId })).toMatchObject({ enabled: false, reason: "incomplete configuration" });
    expect(validateMemberLogConfig({ ...logging, webhookUrl: "https://example.com/not-a-webhook" })).toMatchObject({ enabled: false, reason: "invalid webhook URL" });
    const service = new MemberLogService(logging, async () => ok({ channel_id: "999999999999999999" }));
    await expect(service.initialize()).resolves.toBeUndefined();
    expect(service.enabled).toBe(false);
  });
  it("sends a green no-ping join embed with the actual event timestamp and avatar", async () => {
    const value = member();
    const { payload, time } = await payloadFor("join", value);
    const embed = payload.embeds[0];
    expect(embed.title).toBe("Member Joined");
    expect(embed.color).toBe(0x57f287);
    expect(embed.fields).toEqual(expect.arrayContaining([
      { name: "User", value: "<@123456789012345678> (dark)", inline: false },
      { name: "Guild", value: "Example Server", inline: false },
      { name: "Joined", value: `<t:${Math.floor(time.getTime() / 1000)}:F>`, inline: false },
      { name: "User ID", value: "123456789012345678", inline: true },
      { name: "Guild ID", value: "987654321098765432", inline: true },
    ]));
    expect(embed.thumbnail.url).toContain("embed/avatars/0.png");
    expect(value.avatarOptions).toEqual({ extension: "png", size: 4096, forceStatic: false });
    expect(payload.allowed_mentions).toEqual({ parse: [], users: [], roles: [], replied_user: false });
  });
  it("sends a red no-ping leave embed with the member's normal username", async () => {
    const { payload, time } = await payloadFor("leave");
    const embed = payload.embeds[0];
    expect(embed.title).toBe("Member Left");
    expect(embed.color).toBe(0xed4245);
    expect(embed.fields).toEqual(expect.arrayContaining([
      { name: "User", value: "<@123456789012345678> (dark)", inline: false },
      { name: "Left", value: `<t:${Math.floor(time.getTime() / 1000)}:F>`, inline: false },
    ]));
  });
  it("ignores bot joins and leaves, including before a webhook request", async () => {
    let calls = 0;
    const service = new MemberLogService(logging, async () => { calls++; return ok({ channel_id: logging.channelId }); });
    await service.initialize();
    service.recordJoin(member({ user: { ...member().user, bot: true } }) as any);
    service.recordLeave(member({ user: { ...member().user, bot: true } }) as any);
    expect(calls).toBe(1);
  });
  it("contains transient delivery failures and retries through webhook-only access", async () => {
    let posts = 0;
    const service = new MemberLogService(logging, async (_input, init) => {
      if (!init?.method) return ok({ channel_id: logging.channelId });
      posts++;
      if (posts === 1) throw new Error("network unavailable");
      return new Response(null, { status: 204 });
    });
    await service.initialize();
    expect(() => service.recordJoin(member() as any)).not.toThrow();
    await (service as any).working;
    expect(posts).toBe(2);
  });
  it("registers member logging beside the existing join handler", async () => {
    const r = await repo();
    const bot = new CookieBot(r);
    let existingJoin = 0;
    let recordedJoin = 0;
    (bot as any).onMemberAdd = async () => { existingJoin++; };
    (bot as any).memberLog = { recordJoin: () => { recordedJoin++; }, recordLeave: () => undefined };
    bot.client.emit("guildMemberAdd", member() as any);
    await new Promise((resolve) => setImmediate(resolve));
    expect(existingJoin).toBe(1);
    expect(recordedJoin).toBe(1);
    bot.client.destroy();
  });
});

describe("duration parsing", () => {
  it("accepts supported combined durations", () =>
    expect(parseDuration("1d 4h 30m")).toBe(102_600_000));
  it("rejects malformed, zero, and over-30-day durations", () => {
    expect(parseDuration("0s")).toBeNull();
    expect(parseDuration("5x")).toBeNull();
    expect(parseDuration("30d 1s")).toBeNull();
    expect(parseDuration("30d")).toBe(MAX_MUTE_MS);
  });
  it("uses absolute millisecond dates and second-based Discord timestamps", () => {
    const started = new Date("2026-03-29T00:30:00.000Z");
    const expires = new Date(started.getTime() + (parseDuration("1h") ?? 0));
    expect(expires.getTime()).toBeGreaterThan(started.getTime());
    expect(discordTimestampSeconds(expires)).toBe(
      Math.floor(expires.getTime() / 1000),
    );
    expect(ljubljanaDate(started)).toContain("29 Mar 2026");
  });
});

describe("guild isolation and constraints", () => {
  it("keeps feature and autorole settings isolated", async () => {
    const r = await repo();
    await r.setAutorole("guild-a", "user_role_id", "role-a");
    await r.toggle("guild-a", "voice_rooms_enabled");
    expect(await r.autoroles("guild-b")).toEqual({
      user_role_id: null,
      bot_role_id: null,
    });
    expect((await r.settings("guild-b")).voice_rooms_enabled).toBe(1);
  });
  it("enforces one active room per owner per guild but not across guilds", async () => {
    const r = await repo();
    expect(
      await r.createRoom({
        guild_id: "a",
        channel_id: "c1",
        owner_id: "u",
        control_channel_id: null,
      }),
    ).toBe(true);
    expect(
      await r.createRoom({
        guild_id: "a",
        channel_id: "c2",
        owner_id: "u",
        control_channel_id: null,
      }),
    ).toBe(false);
    expect(
      await r.createRoom({
        guild_id: "b",
        channel_id: "c3",
        owner_id: "u",
        control_channel_id: null,
      }),
    ).toBe(true);
  });
  it("persists preferences and lets deny override allow at the service layer", async () => {
    const r = await repo();
    await r.updatePreference("a", "owner", "user_limit", 25);
    await r.setAccess("a", "owner", "guest", "allowed", true);
    await r.setAccess("a", "owner", "guest", "denied", true);
    const access = await r.access("a", "owner");
    expect((await r.preference("a", "owner")).user_limit).toBe(25);
    expect(access.denied).toContain("guest");
  });
  it("stores staff per guild and removes it cleanly", async () => {
    const r = await repo();
    await r.addStaff("a", "u", "admin");
    expect(await r.isStaff("a", "u")).toBe(true);
    expect(await r.isStaff("b", "u")).toBe(false);
    expect(await r.removeStaff("a", "u")).toBe(true);
    expect(await r.isStaff("a", "u")).toBe(false);
  });
  it("treats Discord administrators as staff without a staff record", async () => {
    const r = await repo();
    const admin = {
      id: "admin",
      guild: { id: "a" },
      permissions: { has: () => true },
    };
    const ordinary = {
      id: "ordinary",
      guild: { id: "a" },
      permissions: { has: () => false },
    };
    expect(await hasPermission(r, admin as any, "staff")).toBe(true);
    expect(await hasPermission(r, ordinary as any, "staff")).toBe(false);
  });
  it("defines Cookie Administrator by effective Discord permission, not ownership or role name", async () => {
    const r = await repo();
    const member = (id: string, administrator: boolean, ownerId = "owner") => ({
      id,
      guild: { id: "a", ownerId },
      permissions: {
        has: (permission: bigint) =>
          administrator && permission === PermissionFlagsBits.Administrator,
      },
    });
    const ownerWithoutAdministrator = member("owner", false),
      nonOwnerManagementRole = member("management", true),
      namedAdminWithoutPermission = member("named-admin", false);
    await r.addStaff("staff", "cookie-staff", "owner");
    const staff = member("cookie-staff", false);
    for (const level of ["administrator", "staff"] as const)
      expect(await hasPermission(r, nonOwnerManagementRole as any, level)).toBe(
        true,
      );
    expect(isDiscordAdministrator(nonOwnerManagementRole as any)).toBe(true);
    expect(
      await hasPermission(r, ownerWithoutAdministrator as any, "administrator"),
    ).toBe(false);
    expect(
      await hasPermission(
        r,
        namedAdminWithoutPermission as any,
        "administrator",
      ),
    ).toBe(false);
    expect(await hasPermission(r, staff as any, "administrator")).toBe(false);
    expect(await hasPermission(r, staff as any, "staff")).toBe(false);
    await r.addStaff("a", "cookie-staff", "owner");
    expect(await hasPermission(r, staff as any, "staff")).toBe(true);
  });
  it("only transfers ownership when the recorded current owner matches", async () => {
    const r = await repo();
    await r.createRoom({
      guild_id: "a",
      channel_id: "room",
      owner_id: "old",
      control_channel_id: null,
    });
    expect(await r.transferRoom("a", "room", "not-old", "new")).toBe(false);
    expect(await r.transferRoom("a", "room", "old", "new")).toBe(true);
    expect((await r.activeRoom("a", "room"))?.owner_id).toBe("new");
  });
  it("resets only the current guild’s persistent room preferences and access lists", async () => {
    const r = await repo();
    await r.updatePreference("a", "owner", "is_locked", 1);
    await r.updatePreference("a", "owner", "user_limit", 10);
    await r.setAccess("a", "owner", "guest", "allowed", true);
    await r.updatePreference("b", "owner", "user_limit", 7);
    await r.resetPreference("a", "owner");
    expect(await r.preference("a", "owner")).toMatchObject({
      is_locked: 0,
      user_limit: 0,
    });
    expect(await r.access("a", "owner")).toEqual({ allowed: [], denied: [] });
    expect((await r.preference("b", "owner")).user_limit).toBe(7);
  });
  it("keeps custom room names guild-specific and clears them on reset", async () => {
    const r = await repo();
    await r.updatePreference("a", "owner", "custom_name", "Gaming");
    expect((await r.preference("a", "owner")).custom_name).toBe("Gaming");
    expect((await r.preference("b", "owner")).custom_name).toBeNull();
    await r.resetPreference("a", "owner");
    expect((await r.preference("a", "owner")).custom_name).toBeNull();
  });
  it("defaults verification to disabled and isolates configuration per guild", async () => {
    const r = await repo();
    expect((await r.verification("a")).enabled).toBe(0);
    await r.updateVerification("a", { enabled: 1, role_id: "verified" });
    expect(await r.verification("a")).toMatchObject({
      enabled: 1,
      role_id: "verified",
    });
    expect(await r.verification("b")).toMatchObject({
      enabled: 0,
      role_id: null,
    });
  });
  it("persists a deployment while message edits retain its message identity", async () => {
    const r = await repo();
    await r.updateVerification("a", {
      channel_id: "verify-channel",
      message_id: "verify-message",
      title: "Welcome",
      description: "Read the rules",
      button_text: "Verify now",
      color: 0x57f287,
    });
    await r.updateVerification("a", {
      title: "Updated title",
      button_text: "Join",
    });
    expect(await r.verification("a")).toMatchObject({
      channel_id: "verify-channel",
      message_id: "verify-message",
      title: "Updated title",
      button_text: "Join",
    });
  });
  it("clears a stale verification deployment without affecting message configuration", async () => {
    const r = await repo();
    await r.updateVerification("a", {
      channel_id: "missing-channel",
      message_id: "missing-message",
      title: "Configured",
    });
    await r.updateVerification("a", { channel_id: null, message_id: null });
    expect(await r.verification("a")).toMatchObject({
      channel_id: null,
      message_id: null,
      title: "Configured",
    });
  });
  it("purges only the removed guild operational data while retaining global templates", async () => {
    const r = await repo();
    await r.setAutorole("a", "user_role_id", "role-a");
    await r.setAutorole("b", "user_role_id", "role-b");
    await r.saveEmbed("Global", '{"content":"shared"}', "admin");
    await r.purgeGuild("a");
    expect(await r.autoroles("b")).toEqual({
      user_role_id: "role-b",
      bot_role_id: null,
    });
    expect(await r.embed("Global")).toBeDefined();
    expect(await r.autoroles("a")).toEqual({
      user_role_id: null,
      bot_role_id: null,
    });
  });
});

describe("verification configuration and AutoRole exclusion", () => {
  const fakeGuild = (id: string) =>
    ({
      id,
      roles: { cache: new Map() },
      members: {
        me: {
          permissions: { has: () => true },
          roles: { highest: { comparePositionTo: () => 1 } },
        },
      },
    }) as any;
  it("normalizes custom hexadecimal colors and rejects malformed values", () => {
    expect(normalizeVerificationColor("#57F287")).toBe(0x57f287);
    expect(normalizeVerificationColor("57f287")).toBe(0x57f287);
    expect(normalizeVerificationColor("#GG0000")).toBeNull();
    expect(normalizeVerificationColor("#12345")).toBeNull();
  });
  it("refuses enabling verification while effective human AutoRole is active", async () => {
    const r = await repo();
    const service = new VerificationService(r);
    const reconciler = { reconcileGuildAccess: async () => undefined } as any;
    await r.setAutorole("a", "user_role_id", "player");
    const result = await service.setEnabled(fakeGuild("a"), true, reconciler);
    expect(result.error).toContain("Verification Cannot Be Enabled");
    expect((await r.verification("a")).enabled).toBe(0);
  });
  it("does not treat bot AutoRole or an unconfigured user AutoRole as a verification conflict", async () => {
    const r = await repo();
    const service = new VerificationService(r);
    const reconciler = {
      calls: 0,
      reconcileGuildAccess: async () => {
        reconciler.calls++;
      },
    } as any;
    await r.setAutorole("a", "bot_role_id", "bots");
    expect(
      (await service.setEnabled(fakeGuild("a"), true, reconciler)).error,
    ).toBeUndefined();
    expect((await r.verification("a")).enabled).toBe(1);
    expect(reconciler.calls).toBe(1);
  });
  it("automatically disables persisted verification when user AutoRole becomes active", async () => {
    const r = await repo();
    const service = new VerificationService(r);
    const reconciler = {
      calls: 0,
      reconcileGuildAccess: async () => {
        reconciler.calls++;
      },
    } as any;
    await r.updateVerification("a", { enabled: 1, role_id: "verified" });
    expect(
      await service.disableForUserAutorole(fakeGuild("a"), reconciler),
    ).toBe(true);
    expect((await r.verification("a")).enabled).toBe(0);
    expect(reconciler.calls).toBe(1);
  });
  it("serializes rapid verification clicks so the role is assigned once", async () => {
    const r = await repo();
    await r.updateVerification("a", { enabled: 1, role_id: "verified" });
    const role = { id: "verified", managed: false };
    const roles = new Map<string, unknown>();
    let adds = 0;
    const guild = {
      id: "a",
      roles: { cache: new Map([["verified", role]]) },
      members: {
        me: {
          permissions: { has: () => true },
          roles: { highest: { comparePositionTo: () => 1 } },
        },
        fetch: async () => ({
          id: "member",
          roles: {
            cache: roles,
            add: async () => {
              adds++;
              roles.set("verified", role);
            },
          },
        }),
      },
    } as any;
    const service = new VerificationService(r);
    const interaction = () =>
      ({ guild, user: { id: "member" }, reply: async () => undefined }) as any;
    await Promise.all([
      service.button(interaction()),
      service.button(interaction()),
    ]);
    expect(adds).toBe(1);
  });
  it("unverifies only a current-guild member holding the configured role", async () => {
    const r = await repo();
    await r.updateVerification("a", { role_id: "verified" });
    const role = { id: "verified", managed: false };
    const roles = new Map([["verified", role]]);
    let removals = 0;
    const guild = {
      id: "a",
      roles: { cache: new Map([["verified", role]]) },
      members: {
        me: {
          permissions: { has: () => true },
          roles: { highest: { comparePositionTo: () => 1 } },
        },
        fetch: async () => ({
          manageable: true,
          roles: {
            cache: roles,
            remove: async () => {
              removals++;
              roles.delete("verified");
            },
          },
        }),
      },
    } as any;
    const service = new VerificationService(r);
    expect(await service.unverify(guild, "member")).toEqual({ changed: true });
    expect(await service.unverify(guild, "member")).toEqual({ changed: false });
    expect(removals).toBe(1);
  });
  it("keeps verification and its deployment isolated between guilds", async () => {
    const r = await repo();
    await r.updateVerification("a", {
      enabled: 1,
      channel_id: "channel-a",
      message_id: "message-a",
    });
    await r.updateVerification("b", {
      enabled: 1,
      channel_id: "channel-b",
      message_id: "message-b",
    });
    expect(await r.verification("a")).toMatchObject({
      channel_id: "channel-a",
      message_id: "message-a",
    });
    expect(await r.verification("b")).toMatchObject({
      channel_id: "channel-b",
      message_id: "message-b",
    });
  });
});

describe("verification voice-room permission plans", () => {
  const access = { allowed: ["allowed"], denied: ["denied"] };
  it("keeps normal unlocked baseline access when verification is off", () => {
    expect(
      roomAccessPlan(
        "everyone",
        "owner",
        { is_locked: 0 },
        access,
        null,
        false,
      ),
    ).toEqual([
      { id: "everyone", allow: [], deny: [] },
      { id: "owner", allow: ["Connect"], deny: [] },
      { id: "allowed", allow: ["Connect"], deny: [] },
      { id: "denied", allow: [], deny: ["Connect"] },
    ]);
  });
  it("keeps an unlocked room verification-restricted while preserving explicit access", () => {
    expect(
      roomAccessPlan(
        "everyone",
        "owner",
        { is_locked: 0 },
        access,
        "verified",
        true,
      ),
    ).toEqual([
      { id: "everyone", allow: [], deny: ["Connect"] },
      { id: "owner", allow: ["Connect"], deny: [] },
      { id: "verified", allow: ["Connect"], deny: [] },
      { id: "allowed", allow: ["Connect"], deny: [] },
      { id: "denied", allow: [], deny: ["Connect"] },
    ]);
  });
  it("does not grant the verification role access to a locked room", () => {
    const plan = roomAccessPlan(
      "everyone",
      "owner",
      { is_locked: 1 },
      access,
      "verified",
      true,
    );
    expect(plan).toContainEqual({
      id: "everyone",
      allow: [],
      deny: ["Connect"],
    });
    expect(plan.find((entry) => entry.id === "verified")).toBeUndefined();
    expect(plan).toContainEqual({
      id: "allowed",
      allow: ["Connect"],
      deny: [],
    });
    expect(plan).toContainEqual({ id: "denied", allow: [], deny: ["Connect"] });
  });
  it("falls back safely when the configured verification role is missing", () => {
    const plan = roomAccessPlan(
      "everyone",
      "owner",
      { is_locked: 0 },
      access,
      "missing-role",
      false,
    );
    expect(plan).toContainEqual({ id: "everyone", allow: [], deny: [] });
    expect(plan.find((entry) => entry.id === "missing-role")).toBeUndefined();
  });
});

describe("moderation notification privacy", () => {
  it("does not include moderator attribution in mute or unmute DMs", async () => {
    const r = await repo();
    const service = new MuteService(r, () => []);
    const sent: any[] = [];
    const member = {
      id: "member",
      send: async (payload: any) => {
        sent.push(payload);
      },
    };
    await (service as any).dmMuted(
      member,
      { id: "a", name: "Guild" },
      60_000,
      new Date("2026-01-01T00:01:00Z"),
      "Reason",
    );
    await (service as any).dmUnmuted(
      member,
      { id: "a", name: "Guild" },
      "Reason",
      false,
    );
    const serialized = JSON.stringify(sent);
    expect(serialized).not.toContain("Moderator");
    expect(serialized).not.toContain("moderator");
  });
});

describe("guild backup persistence", () => {
  const snapshot = (guildId: string) =>
    JSON.stringify({
      schemaVersion: BACKUP_SCHEMA_VERSION,
      guild: { id: guildId, name: "Source" },
      roles: [],
      channels: [],
      members: [],
      cookie: {},
    });
  const backup = (
    id: string,
    guildId: string,
    createdAt: string,
  ): GuildBackup => {
    const value = snapshot(guildId);
    return {
      backup_id: id,
      source_guild_id: guildId,
      source_guild_name: "Source",
      created_at: createdAt,
      created_by: "admin",
      creation_type: "manual",
      snapshot_schema_version: BACKUP_SCHEMA_VERSION,
      snapshot_json: value,
      snapshot_hash: "",
      snapshot_size: Buffer.byteLength(value),
      role_count: 0,
      channel_count: 0,
      member_count: 0,
      overwrite_count: 0,
      source_removed_at: null,
    };
  };
  it("rotates only after inserting a successful third backup", async () => {
    const r = await repo();
    await r.storeBackup(backup("one", "a", "2026-01-01T00:00:00.000Z"));
    await r.storeBackup(backup("two", "a", "2026-01-02T00:00:00.000Z"));
    await r.storeBackup(backup("three", "a", "2026-01-03T00:00:00.000Z"));
    expect((await r.backups("a")).map((entry) => entry.backup_id)).toEqual([
      "three",
      "two",
    ]);
  });
  it("retains restore-referenced history instead of failing a new backup transaction", async () => {
    const r = await repo();
    await r.storeBackup(backup("one", "a", "2026-01-01T00:00:00.000Z"));
    await r.storeBackup(backup("two", "a", "2026-01-02T00:00:00.000Z"));
    await r.createRestoreOperation({
      restore_id: "restore-one",
      backup_id: "one",
      destination_guild_id: "b",
      requested_by: "admin",
      status: "completed",
      current_phase: "complete",
      progress_json: "{}",
      errors_json: "[]",
    });
    await expect(
      r.storeBackup(backup("three", "a", "2026-01-03T00:00:00.000Z")),
    ).resolves.toBeUndefined();
    expect((await r.backups("a")).map((entry) => entry.backup_id)).toEqual([
      "three",
      "two",
      "one",
    ]);
  });
  it("retains a selected source backup while making a same-guild safety backup", async () => {
    const r = await repo();
    await r.storeBackup(backup("source", "a", "2026-01-01T00:00:00.000Z"));
    await r.storeBackup(backup("other", "a", "2026-01-02T00:00:00.000Z"));
    await r.storeBackup(
      backup("safety", "a", "2026-01-03T00:00:00.000Z"),
      "source",
    );
    expect((await r.backups("a")).map((entry) => entry.backup_id)).toEqual([
      "safety",
      "source",
    ]);
  });
  it("marks running restore operations interrupted for safe startup recovery", async () => {
    const r = await repo();
    await r.storeBackup(backup("source", "a", "2026-01-01T00:00:00.000Z"));
    await r.createRestoreOperation({
      restore_id: "restore",
      backup_id: "source",
      destination_guild_id: "b",
      requested_by: "admin",
      status: "running",
      current_phase: "roles",
      progress_json: "{}",
      errors_json: "[]",
    });
    await r.interruptRunningRestores();
    const db = (r as any).db;
    expect(
      (
        await db.get(
          "SELECT status FROM restore_operations WHERE restore_id=?",
          "restore",
        )
      ).status,
    ).toBe("interrupted");
  });
  it("persists destructive restore partial failures as an explicit terminal state", async () => {
    const r = await repo();
    await r.storeBackup(backup("source", "a", "2026-01-01T00:00:00.000Z"));
    await r.createRestoreOperation({
      restore_id: "partial",
      backup_id: "source",
      destination_guild_id: "b",
      requested_by: "admin",
      status: "planned",
      current_phase: "safety backup",
      progress_json: "{}",
      errors_json: "[]",
    });
    await r.updateRestoreOperation("partial", {
      status: "failed_partial",
      current_phase: "channels",
      completed: true,
    });
    const row = await (r as any).db.get(
      "SELECT status,completed_at FROM restore_operations WHERE restore_id=?",
      "partial",
    );
    expect(row.status).toBe("failed_partial");
    expect(row.completed_at).toBeTruthy();
  });
  it("rejects an integrity-mismatched snapshot before restore planning", async () => {
    const r = await repo();
    const value = snapshot("a");
    const bad = {
      ...backup("bad", "a", "2026-01-01T00:00:00.000Z"),
      snapshot_json: value,
      snapshot_hash: "not-a-hash",
    };
    await r.storeBackup(bad);
    const service = new BackupService(r, {
      guilds: { cache: new Map() },
    } as any);
    await expect(
      service.preview(
        "bad",
        {
          id: "b",
          roles: { cache: new Map() },
          channels: { cache: new Map() },
        } as any,
        "admin",
      ),
    ).rejects.toThrow("integrity");
  });
  it("creates one missed Ljubljana daily automatic backup without duplicate retries", async () => {
    const r = await repo();
    await r.updateBackupSettings("a", { automatic_enabled: 1 });
    const guild = { id: "a", name: "Source" };
    const service = new BackupService(r, {
      guilds: { cache: new Map([["a", guild]]) },
    } as any);
    let creates = 0;
    (service as any).create = async () => {
      creates++;
      await r.updateBackupSettings("a", {
        last_success_at: "2026-08-26T01:00:00.000Z",
      });
    };
    await service.runAutomatic(new Date("2026-08-26T03:30:00.000Z"));
    await service.runAutomatic(new Date("2026-08-26T04:00:00.000Z"));
    expect(creates).toBe(1);
  });
  it("does not run a daily backup before 03:00 Europe/Ljubljana", async () => {
    const r = await repo();
    await r.updateBackupSettings("a", { automatic_enabled: 1 });
    const service = new BackupService(r, {
      guilds: { cache: new Map([["a", { id: "a", name: "Source" }]]) },
    } as any);
    let creates = 0;
    (service as any).create = async () => {
      creates++;
    };
    await service.runAutomatic(new Date("2026-08-26T00:59:00.000Z"));
    expect(creates).toBe(0);
  });
  it("uses a complete member cache and shares exactly one guild synchronization when incomplete", async () => {
    const service = new BackupService({} as any, {} as any);
    const complete = {
      memberCount: 2,
      id: "g",
      members: {
        cache: new Map([
          ["a", {}],
          ["b", {}],
        ]),
        fetch: async () => {
          throw new Error("must not fetch");
        },
      },
    };
    await expect(
      (service as any).acquireMembers(complete),
    ).resolves.toMatchObject({ source: "cache" });
    let fetches = 0;
    const incomplete = {
      memberCount: 2,
      id: "other",
      members: {
        cache: new Map([["a", {}]]),
        fetch: async () => {
          fetches++;
          await new Promise((resolve) => setTimeout(resolve, 5));
          return new Map([
            ["a", {}],
            ["b", {}],
          ]);
        },
      },
    };
    const [first, second] = await Promise.all([
      (service as any).acquireMembers(incomplete),
      (service as any).acquireMembers(incomplete),
    ]);
    expect(fetches).toBe(1);
    expect(first.source).toBe("synchronized");
    expect(second.source).toBe("synchronized");
  });
  it("retries one gateway member rate limit and fails without accepting incomplete members", async () => {
    const service = new BackupService({} as any, {} as any);
    let fetches = 0;
    const guild = {
      id: "g",
      memberCount: 2,
      members: {
        cache: new Map([["a", {}]]),
        fetch: async () => {
          fetches++;
          if (fetches === 1)
            throw Object.assign(new Error("opcode 8 rate limited"), {
              name: "GatewayRateLimitError",
              retry_after: 0,
            });
          throw Object.assign(new Error("still rate limited"), {
            name: "GatewayRateLimitError",
            retry_after: 0,
          });
        },
      },
    };
    await expect((service as any).acquireMembers(guild)).rejects.toMatchObject({
      rateLimited: true,
    });
    expect(fetches).toBe(2);
  });
  it("releases the per-guild backup lock after member synchronization failure", async () => {
    const service = new BackupService(
      { activeRooms: async () => [] } as any,
      {} as any,
    );
    const guild = {
      id: "g",
      memberCount: 1,
      members: {
        cache: new Map(),
        fetch: async () => {
          throw new Error("member sync failed");
        },
      },
    };
    await expect(
      service.create(guild as any, "admin", "manual"),
    ).rejects.toThrow("member sync failed");
    expect((service as any).activeBackups.has("g")).toBe(false);
  });
  it("rejects a duplicate manual backup instead of starting a second member request", async () => {
    const service = new BackupService({} as any, {} as any);
    (service as any).activeBackups.add("g");
    await expect(
      service.create({ id: "g" } as any, "admin", "manual"),
    ).rejects.toThrow("currently being created");
  });
  it("does not start an automatic backup while a manual backup is active", async () => {
    const service = new BackupService(
      {
        backupSettings: async () => ({
          automatic_enabled: 1,
          last_success_at: null,
        }),
        updateBackupSettings: async () => undefined,
        purgeExpiredBackups: async () => undefined,
      } as any,
      { guilds: { cache: new Map([["g", { id: "g" }]]) } } as any,
    );
    (service as any).activeBackups.add("g");
    let creates = 0;
    (service as any).create = async () => {
      creates++;
    };
    await service.runAutomatic(new Date("2026-08-26T03:30:00.000Z"));
    expect(creates).toBe(0);
  });
  it("maps every overwrite through destination IDs and preserves bigint precision", () => {
    const large = "900719925474099312345";
    const mapped = mapPermissionOverwrites(
      [
        { id: "source-everyone", type: 0, allow: large, deny: "7" },
        { id: "source-role", type: 0, allow: "3", deny: large },
        { id: "present-member", type: 1, allow: "5", deny: "6" },
        { id: "missing-member", type: 1, allow: "1", deny: "2" },
      ],
      new Map([
        ["source-everyone", "destination-everyone"],
        ["source-role", "destination-role"],
      ]),
      new Set(["present-member"]),
    );
    expect(mapped.skipped).toBe(1);
    expect(mapped.mapped).toEqual([
      { id: "destination-everyone", type: 0, allow: BigInt(large), deny: 7n },
      { id: "destination-role", type: 0, allow: 3n, deny: BigInt(large) },
      { id: "present-member", type: 1, allow: 5n, deny: 6n },
    ]);
    expect(mapped.mapped.some((entry) => entry.id.startsWith("source-"))).toBe(
      false,
    );
  });
  it("groups restore warnings into compact operator-facing categories", () => {
    expect(
      groupRestoreWarnings([
        "Managed integration roles were excluded.",
        "Protected channel could not be deleted.",
        "Role hierarchy exceeds Cookie's highest role.",
        "Community feature is controlled by Discord.",
        "An unsupported property was not captured.",
        "A miscellaneous warning.",
      ]).map(({ category, warnings }) => [category, warnings.length]),
    ).toEqual([
      ["Managed/integration roles", 1],
      ["Protected channels", 1],
      ["Hierarchy limitations", 1],
      ["Discord-controlled guild features", 1],
      ["Unsupported properties", 1],
      ["Other", 1],
    ]);
  });
  it("restores zero system channel flags as an empty discord.js bitfield", () => {
    const resolved = systemChannelFlagsFromSnapshot("0");
    expect(resolved).toBeInstanceOf(SystemChannelFlagsBitField);
    expect(resolved.bitfield).toBe(0);
    expect(SystemChannelFlagsBitField.resolve(resolved)).toBe(0);
  });
  it("preserves one, multiple, and every supported system channel flag", () => {
    const flags = SystemChannelFlagsBitField.Flags as Record<string, number>;
    const one = flags.SuppressJoinNotifications;
    const multiple =
      flags.SuppressJoinNotifications |
      flags.SuppressGuildReminderNotifications |
      flags.SuppressRoleSubscriptionPurchaseNotificationReplies;
    const all = Object.entries(flags)
      .filter(([, value]) => typeof value === "number")
      .reduce((value, [, flag]) => value | flag, 0);

    expect(systemChannelFlagsFromSnapshot(String(one)).bitfield).toBe(one);
    expect(systemChannelFlagsFromSnapshot(String(multiple)).bitfield).toBe(
      multiple,
    );
    expect(systemChannelFlagsFromSnapshot(String(all)).bitfield).toBe(all);
  });
  it("rejects malformed or unsupported persisted system channel flags", () => {
    expect(() => systemChannelFlagsFromSnapshot("not-a-bitfield")).toThrow(
      "systemChannelFlags",
    );
    expect(() => systemChannelFlagsFromSnapshot("-1")).toThrow(
      "systemChannelFlags",
    );
    expect(() => systemChannelFlagsFromSnapshot("64")).toThrow("unsupported");
  });
  it("supports legacy numeric system channel flags and round-trips captured values", () => {
    const source = new SystemChannelFlagsBitField([
      "SuppressPremiumSubscriptions",
      "SuppressJoinNotificationReplies",
    ]);
    const persisted = source.bitfield.toString();
    expect(systemChannelFlagsFromSnapshot(0).bitfield).toBe(0);
    expect(systemChannelFlagsFromSnapshot(persisted).bitfield.toString()).toBe(
      persisted,
    );
  });
  it("never sends undefined or legacy numeric role colors to RoleManager#create", () => {
    const role = (extra: Record<string, unknown> = {}) => ({
      id: "source-role",
      name: "Staff",
      permissions: "900719925474099312345",
      hoist: false,
      mentionable: true,
      ...extra,
    });
    const assertDiscordRolePayload = (options: any) => {
      if (!("colors" in options)) return;
      if (!options.colors || typeof options.colors !== "object")
        throw new Error("colors must be an object");
      if (typeof options.colors.primaryColor !== "number")
        throw new Error("primaryColor must be numeric");
    };
    const noColor = roleCreateOptions(role({ color: undefined }));
    expect(noColor).not.toHaveProperty("colors");
    expect(() => assertDiscordRolePayload(noColor)).not.toThrow();

    const standard = roleCreateOptions(role({ color: 0x5865f2 }));
    expect(standard.colors).toEqual({ primaryColor: 0x5865f2 });
    expect(() => assertDiscordRolePayload(standard)).not.toThrow();
    expect(standard.permissions).toBe(900719925474099312345n);
  });
  it("preserves supported enhanced role colors and omits malformed cosmetic data", () => {
    expect(
      roleColorsFromSnapshot({
        colors: {
          primaryColor: 11127295,
          secondaryColor: 16759788,
          tertiaryColor: 16761760,
        },
      }),
    ).toEqual({
      primaryColor: 11127295,
      secondaryColor: 16759788,
      tertiaryColor: 16761760,
    });
    expect(roleColorsFromSnapshot({ colors: undefined, color: undefined })).toBeUndefined();
    expect(roleColorsFromSnapshot({ colors: 0x5865f2 })).toBeUndefined();
  });
  it("captures only ordinary roles, excluding @everyone and managed integrations", async () => {
    const service = new BackupService(
      {
        activeRooms: async () => [],
        settings: async () => ({}),
        autoroles: async () => ({}),
        joinChannel: async () => null,
        verification: async () => ({}),
        staffMembers: async () => [],
      } as any,
      {} as any,
    );
    const everyone = { id: "guild", managed: false, permissions: { bitfield: 0n } };
    const ordinary = {
      id: "ordinary",
      name: "Ordinary",
      managed: false,
      colors: { primaryColor: 0x5865f2, secondaryColor: null, tertiaryColor: null },
      hoist: false,
      permissions: { bitfield: 900719925474099312345n },
      mentionable: true,
      position: 1,
      unicodeEmoji: null,
    };
    const managed = { ...ordinary, id: "managed", managed: true };
    const sourceSystemFlags = new SystemChannelFlagsBitField([
      "SuppressJoinNotifications",
      "SuppressRoleSubscriptionPurchaseNotifications",
    ]);
    const captured = await (service as any).capture(
      {
        id: "guild",
        name: "Guild",
        roles: { everyone, cache: new Map([["guild", everyone], ["ordinary", ordinary], ["managed", managed]]) },
        channels: { cache: new Map() },
        features: [],
        systemChannelFlags: sourceSystemFlags,
        members: { filter: () => ({ map: () => [] }) },
      },
      "backup",
      { filter: () => ({ map: () => [] }) },
    );
    expect(captured.snapshot.roles).toMatchObject([{ id: "ordinary" }]);
    expect(captured.snapshot.roles).toHaveLength(1);
    expect(captured.snapshot.guild.systemChannelFlags).toBe(
      sourceSystemFlags.bitfield.toString(),
    );
    expect(
      systemChannelFlagsFromSnapshot(
        captured.snapshot.guild.systemChannelFlags,
      ).bitfield,
    ).toBe(sourceSystemFlags.bitfield);
  });
  it("does not add a downstream hierarchy error when role creation left no mapping", async () => {
    const service = new BackupService({} as any, {} as any);
    const critical: string[] = [];
    await (service as any).applyRoleHierarchy(
      { members: { me: { roles: { highest: { position: 99 } } } }, roles: { cache: new Map() } },
      { roles: [{ id: "failed", name: "Failed", position: 1 }] },
      new Map(),
      [],
      critical,
    );
    expect(critical).toEqual([]);
  });
  it("rejects malformed snapshots before a restore can mutate Discord", () => {
    const service = new BackupService({} as any, {} as any);
    const valid = {
      schemaVersion: BACKUP_SCHEMA_VERSION,
      guild: { id: "source", everyonePermissions: "0" },
      roles: [{ id: "role", name: "Staff", permissions: "0" }],
      channels: [
        {
          id: "category",
          type: 4,
          name: "Private",
          parentId: null,
          overwrites: [],
        },
      ],
      members: [{ userId: "member", roleIds: ["role"] }],
      cookie: {},
    };
    expect(() => (service as any).validate(valid)).not.toThrow();
    expect(() =>
      (service as any).validate({
        ...valid,
        roles: [{ ...valid.roles[0], permissions: "not-a-bitfield" }],
      }),
    ).toThrow("role");
    expect(() =>
      (service as any).validate({
        ...valid,
        channels: [
          {
            ...valid.channels[0],
            overwrites: [{ id: "x", type: 0, allow: "1.5", deny: "0" }],
          },
        ],
      }),
    ).toThrow("overwrite");
    expect(() =>
      (service as any).validate({
        ...valid,
        channels: [{ ...valid.channels[0], parentId: "missing" }],
      }),
    ).toThrow("channel");
    expect(() =>
      (service as any).validate({
        ...valid,
        guild: { ...valid.guild, systemChannelFlags: "invalid" },
      }),
    ).toThrow("system channel flags");
  });
  it("uses UTF-8 byte boundaries for the 25 MB snapshot limit", () => {
    const under = "€".repeat(Math.floor((MAX_BACKUP_BYTES - 3) / 3));
    const over = `${under}€€`;
    expect(Buffer.byteLength(under, "utf8")).toBeLessThanOrEqual(
      MAX_BACKUP_BYTES,
    );
    expect(Buffer.byteLength(over, "utf8")).toBeGreaterThan(MAX_BACKUP_BYTES);
  });
  it("records a failed safety backup before any destination mutation", async () => {
    const r = await repo();
    const value = JSON.stringify({
      schemaVersion: BACKUP_SCHEMA_VERSION,
      guild: { id: "source", everyonePermissions: "0" },
      roles: [],
      channels: [],
      members: [],
      cookie: {},
    });
    const source = {
      ...backup("source-backup", "source", "2026-01-01T00:00:00.000Z"),
      snapshot_json: value,
      snapshot_hash: createHash("sha256").update(value).digest("hex"),
    };
    await r.storeBackup(source);
    let mutations = 0;
    const administrator = { permissions: { has: () => true } };
    const destination = {
      id: "destination",
      name: "Destination",
      members: { fetch: async () => administrator },
      roles: { everyone: { id: "destination" }, cache: new Map() },
      channels: { cache: new Map() },
      edit: async () => {
        mutations++;
      },
    };
    const service = new BackupService(r, {
      guilds: {
        cache: new Map([
          ["source", { members: { fetch: async () => administrator } }],
        ]),
      },
    } as any);
    (service as any).create = async () => {
      throw new Error("safety snapshot failed");
    };
    await expect(
      service.restore("source-backup", destination as any, "admin"),
    ).rejects.toThrow("safety snapshot failed");
    expect(mutations).toBe(0);
    const operation = await (r as any).db.get(
      "SELECT status,current_phase FROM restore_operations WHERE destination_guild_id=?",
      "destination",
    );
    expect(operation).toMatchObject({
      status: "failed",
      current_phase: "safety backup",
    });
  });
  it("deletes restorable destination state children-first while preserving Cookie and managed roles", async () => {
    const deleted: string[] = [];
    const r = { activeRooms: async () => [] } as any;
    const service = new BackupService(r, {} as any);
    const child = {
      id: "child",
      name: "old-channel",
      type: 0,
      deletable: true,
      delete: async () => {
        deleted.push("child");
      },
    };
    const category = {
      id: "category",
      name: "old-category",
      type: 4,
      deletable: true,
      delete: async () => {
        deleted.push("category");
      },
    };
    const ordinary = {
      id: "ordinary",
      name: "old-role",
      managed: false,
      editable: true,
      delete: async () => {
        deleted.push("ordinary");
      },
    };
    const managed = {
      id: "managed",
      name: "integration",
      managed: true,
      editable: true,
      delete: async () => {
        deleted.push("managed");
      },
    };
    const cookie = {
      id: "cookie",
      name: "Cookie",
      managed: false,
      editable: true,
      delete: async () => {
        deleted.push("cookie");
      },
    };
    const guild = {
      id: "g",
      channels: {
        cache: new Map([
          [child.id, child],
          [category.id, category],
        ]),
      },
      roles: {
        cache: new Map([
          [ordinary.id, ordinary],
          [managed.id, managed],
          [cookie.id, cookie],
        ]),
      },
      members: { me: { roles: { botRole: cookie } } },
    };
    const warnings: string[] = [],
      critical: string[] = [];
    await (service as any).deleteDestinationState(guild, warnings, critical);
    expect(deleted).toEqual(["child", "category", "ordinary"]);
    expect(critical).toEqual([]);
  });
  it("records an actual sanitized Discord channel error as critical restore state", async () => {
    const service = new BackupService({} as any, {} as any);
    const critical: string[] = [];
    const guild = {
      channels: {
        create: async () => {
          throw Object.assign(new Error("Missing Permissions"), {
            code: 50013,
          });
        },
      },
    };
    await (service as any).createChannels(
      guild,
      [{ id: "news", name: "news", type: 5, parentId: null }],
      new Map(),
      { channels: 0 },
      critical,
      undefined,
      "Announcement channels",
    );
    expect(critical).toEqual([
      "Channel news could not be created — Discord error 50013: Missing Permissions",
    ]);
  });
  it("aborts an older incomplete announcement-channel backup before destructive cleanup", () => {
    const service = new BackupService({} as any, {} as any);
    expect(() =>
      (service as any).assertReconstructable({
        guild: {},
        channels: [{ type: 5 }],
      }),
    ).toThrow("did not capture Community state");
  });
  it("enables Community with mapped Rules and Updates channels before announcement creation", async () => {
    const service = new BackupService({} as any, {} as any);
    let payload: any;
    const guild = {
      features: [],
      edit: async (value: any) => {
        payload = value;
      },
    };
    const critical: string[] = [];
    await (service as any).restoreCommunity(
      guild,
      {
        guild: {
          communityEnabled: true,
          rulesChannelId: "rules",
          publicUpdatesChannelId: "updates",
        },
      },
      new Map([
        ["rules", "b-rules"],
        ["updates", "b-updates"],
      ]),
      [],
      critical,
    );
    expect(payload).toMatchObject({
      features: ["COMMUNITY"],
      rulesChannel: "b-rules",
      publicUpdatesChannel: "b-updates",
    });
    expect(critical).toEqual([]);
  });
  it("moves Community references before deleting a stale protected rules channel", async () => {
    const repoMock = {
      settings: async () => ({ ticket_category_id: null }),
      autoroles: async () => ({}),
      joinChannel: async () => null,
      verification: async () => ({}),
    } as any;
    const service = new BackupService(repoMock, {} as any);
    const channels = new Map<string, any>();
    const guild: any = {
      id: "destination",
      features: ["COMMUNITY"],
      rulesChannelId: "stale-about-us",
      publicUpdatesChannelId: "stale-updates",
      edit: async (payload: any) => {
        guild.features = payload.features;
        guild.rulesChannelId = payload.rulesChannel;
        guild.publicUpdatesChannelId = payload.publicUpdatesChannel;
      },
      fetch: async () => guild,
      roles: { everyone: { id: "destination" }, cache: new Map() },
      members: { cache: new Map() },
    };
    const stale = {
      id: "stale-about-us",
      name: "📌 || about-us",
      type: 0,
      get deletable() {
        return guild.rulesChannelId !== "stale-about-us";
      },
      delete: async () => channels.delete("stale-about-us"),
      permissionOverwrites: { cache: new Map() },
    };
    const rules = {
      id: "restored-rules",
      name: "rules",
      type: 0,
      parentId: null,
      permissionOverwrites: { cache: new Map() },
    };
    const updates = {
      id: "restored-updates",
      name: "updates",
      type: 0,
      parentId: null,
      permissionOverwrites: { cache: new Map() },
    };
    channels.set(stale.id, stale);
    channels.set(rules.id, rules);
    channels.set(updates.id, updates);
    guild.channels = { cache: channels };
    const snapshot = {
      guild: {
        communityEnabled: true,
        rulesChannelId: "source-rules",
        publicUpdatesChannelId: "source-updates",
      },
      roles: [],
      channels: [
        { id: "source-rules", name: "rules", type: 0, parentId: null, overwrites: [] },
        { id: "source-updates", name: "updates", type: 0, parentId: null, overwrites: [] },
      ],
    };
    const channelMap = new Map([
      ["source-rules", rules.id],
      ["source-updates", updates.id],
    ]);
    const critical: string[] = [];
    expect(stale.deletable).toBe(false);
    await (service as any).restoreCommunity(
      guild,
      snapshot,
      channelMap,
      [],
      critical,
    );
    expect(guild.rulesChannelId).toBe(rules.id);
    expect(stale.deletable).toBe(true);
    const unremovable = new Map<string, string>();
    await (service as any).cleanupStaleChannels(
      guild,
      channelMap,
      unremovable,
      critical,
    );
    expect(channels.has(stale.id)).toBe(false);
    await (service as any).finalValidate(
      guild,
      snapshot,
      new Map(),
      channelMap,
      unremovable,
      [],
      critical,
    );
    expect(critical).toEqual([]);
  });
  it("treats a post-restore overwrite mismatch as a critical final-validation failure", async () => {
    const r = {
      settings: async () => ({ ticket_category_id: null }),
      autoroles: async () => ({ user_role_id: null, bot_role_id: null }),
      joinChannel: async () => null,
      verification: async () => ({ role_id: null }),
    } as any;
    const service = new BackupService(r, {} as any);
    const channel = {
      id: "destination-channel",
      type: 0,
      name: "general",
      parentId: null,
      permissionOverwrites: { cache: new Map() },
    };
    const destination = {
      roles: { cache: new Map() },
      channels: { cache: new Map([[channel.id, channel]]) },
      members: { cache: new Map() },
      features: [],
    };
    const critical: string[] = [];
    await (service as any).finalValidate(
      destination,
      {
        guild: { communityEnabled: undefined },
        roles: [],
        channels: [
          {
            id: "source-channel",
            type: 0,
            name: "general",
            parentId: null,
            overwrites: [
              { id: "source-everyone", type: 0, allow: "1", deny: "0" },
            ],
          },
        ],
        members: [],
      },
      new Map([["source-everyone", "destination-everyone"]]),
      new Map([["source-channel", "destination-channel"]]),
      new Map(),
      [],
      critical,
    );
    expect(critical).toEqual(
      expect.arrayContaining([
        "Final validation: permission overwrite count for general does not match its backup.",
        "Final validation: permission overwrite for general does not match its backup.",
      ]),
    );
  });
  it("flags stale ordinary roles, channels, and categories after reconstruction", async () => {
    const service = new BackupService(
      {
        settings: async () => ({ ticket_category_id: null }),
        autoroles: async () => ({}),
        joinChannel: async () => null,
        verification: async () => ({}),
      } as any,
      {} as any,
    );
    const everyone = { id: "guild", managed: false };
    const critical: string[] = [];
    await (service as any).finalValidate(
      {
        roles: {
          everyone,
          cache: new Map([
            ["guild", everyone],
            ["stale-role", { id: "stale-role", name: "Stale", managed: false }],
          ]),
        },
        channels: {
          cache: new Map([
            ["stale-channel", { id: "stale-channel", name: "Stale channel", type: 0 }],
            ["stale-category", { id: "stale-category", name: "Stale category", type: 4 }],
          ]),
        },
        members: { cache: new Map() },
        features: [],
      },
      { guild: { communityEnabled: undefined }, roles: [], channels: [] },
      new Map(),
      new Map(),
      new Map(),
      [],
      critical,
    );
    expect(critical).toEqual(
      expect.arrayContaining([
        expect.stringContaining("unexpected ordinary role"),
        expect.stringContaining("unexpected channel or categor"),
      ]),
    );
  });
  it("validates a large synthetic snapshot without numeric permission conversion", () => {
    const service = new BackupService({} as any, {} as any);
    const snapshot = {
      schemaVersion: BACKUP_SCHEMA_VERSION,
      guild: { id: "source", everyonePermissions: "0" },
      roles: Array.from({ length: 100 }, (_, index) => ({
        id: `r${index}`,
        name: `Staff ${index % 3}`,
        permissions: "900719925474099312345",
      })),
      channels: Array.from({ length: 500 }, (_, index) => ({
        id: `c${index}`,
        type: 0,
        name: `general-${index % 3}`,
        parentId: null,
        overwrites: [
          { id: "source", type: 0, allow: "900719925474099312345", deny: "0" },
        ],
      })),
      members: Array.from({ length: 5_000 }, (_, index) => ({
        userId: `u${index}`,
        roleIds: index % 2 ? ["r1"] : [],
      })),
      cookie: {},
    };
    const started = performance.now();
    expect(() => (service as any).validate(snapshot)).not.toThrow();
    const json = JSON.stringify(snapshot);
    const hash = createHash("sha256").update(json).digest("hex");
    expect(hash).toHaveLength(64);
    expect(Buffer.byteLength(json, "utf8")).toBeLessThan(MAX_BACKUP_BYTES);
    expect(performance.now() - started).toBeLessThan(5_000);
  });
});

describe("command UX schema", () => {
  it("uses exact labels and no longer registers /role", () => {
    const names = commands.map((command: any) => command.name);
    expect(names).not.toContain("role");
    expect(
      commands
        .find((command: any) => command.name === "admin")
        ?.description.startsWith(ADMIN_LABEL),
    ).toBe(true);
    expect(
      commands
        .find((command: any) => command.name === "mute")
        ?.description.startsWith(STAFF_LABEL),
    ).toBe(true);
    expect(
      commands.find((command: any) => command.name === "unverify")?.description,
    ).toBe(`${ADMIN_LABEL} Unverify a member.`);
  });
  it("keeps /admin and /embed zero-option while attachments stay in Embed Manager", () => {
    const admin: any = commands.find(
        (command: any) => command.name === "admin",
      ),
      embed: any = commands.find((command: any) => command.name === "embed"),
      unverify: any = commands.find(
        (command: any) => command.name === "unverify",
      );
    expect(admin.options).toEqual([]);
    expect(embed.options).toEqual([]);
    expect(unverify.options).toHaveLength(1);
    expect(commands.map((command: any) => command.name)).toEqual([
      "admin",
      "embed",
      "mute",
      "unmute",
      "unverify",
    ]);
  });
  it("uses Discord default permissions to hide restricted commands from unauthorized members", () => {
    const byName = new Map(
      commands.map((command: any) => [command.name, command]),
    );
    for (const name of ["admin", "embed", "unverify"])
      expect(byName.get(name)?.default_member_permissions).toBe(
        PermissionFlagsBits.Administrator.toString(),
      );
    for (const name of ["mute", "unmute"])
      expect(byName.get(name)?.default_member_permissions).toBe(
        PermissionFlagsBits.ModerateMembers.toString(),
      );
    for (const command of commands as any[]) {
      expect(command.integration_types).toEqual([0]);
      expect(command.contexts).toEqual([0]);
    }
  });
  it("keeps the Embed Manager button rows within Discord's five-component limit", async () => {
    const r = await repo();
    const bot = new CookieBot(r);
    const replies: any[] = [];
    await (bot as any).embed({
      guild: {
        id: "a",
        members: {
          fetch: async () => ({ permissions: { has: () => true } }),
        },
      },
      user: { id: "admin" },
      reply: async (payload: any) => replies.push(payload),
    });
    expect(replies[0].components).toHaveLength(2);
    expect(replies[0].components.map((row: any) => row.components.length)).toEqual(
      [5, 1],
    );
    bot.client.destroy();
  });
});

describe("restore completion notifications and guild identity", () => {
  const identitySnapshot = {
    guild: {
      id: "source",
      name: "Deforged Games",
      description: "Restored description",
      communityEnabled: true,
      assets: { icon: { state: "absent" } },
    },
  } as any;
  const result = (status: any, warnings: string[] = []) => ({
    restoreId: "restore",
    status,
    warnings,
    criticalFailures: status === "failed" ? ["safety backup — denied"] : [],
    safetyBackupId: "safety",
    roles: 2,
    channels: 3,
    overwrites: 4,
    members: 5,
  });
  async function completionFixture() {
    const r = await repo();
    const value = JSON.stringify({
      schemaVersion: BACKUP_SCHEMA_VERSION,
      guild: { id: "source", name: "Source", everyonePermissions: "0" },
      roles: [],
      channels: [],
      members: [],
      cookie: {},
    });
    const backup: GuildBackup = {
      backup_id: "backup",
      source_guild_id: "source",
      source_guild_name: "Source",
      created_at: "2026-01-01T00:00:00.000Z",
      created_by: "admin",
      creation_type: "manual",
      snapshot_schema_version: BACKUP_SCHEMA_VERSION,
      snapshot_json: value,
      snapshot_hash: createHash("sha256").update(value).digest("hex"),
      snapshot_size: Buffer.byteLength(value),
      role_count: 0,
      channel_count: 0,
      member_count: 0,
      overwrite_count: 0,
      source_removed_at: null,
    };
    await r.storeBackup(backup);
    await r.createRestoreOperation({
      restore_id: "restore",
      backup_id: "backup",
      destination_guild_id: "destination",
      requested_by: "admin",
      executor_user_id: "admin",
      status: "completed",
      current_phase: "complete",
      progress_json: "{}",
      errors_json: "[]",
    });
    await r.updateRestoreOperation("restore", { completed: true });
    return { r, backup };
  }
  it("sends exactly one green/yellow/red executor completion DM and persists its outcome", async () => {
    for (const [status, warnings, expected] of [
      ["completed", [], "Server Restore Completed"],
      [
        "completed_with_warnings",
        ["banner unavailable"],
        "Restore Completed With Warnings",
      ],
      ["failed", [], "Server Restore Failed"],
    ] as const) {
      const { r, backup } = await completionFixture();
      const sent: any[] = [];
      const service = new BackupService(r, {
        users: {
          fetch: async () => ({
            send: async (payload: any) => sent.push(payload),
          }),
        },
      } as any);
      await (service as any).notifyCompletion({
        restoreId: "restore",
        backup,
        snapshot: identitySnapshot,
        destination: { id: "destination", name: "Destination" },
        userId: "admin",
        startedAt: "2026-01-01T00:00:00.000Z",
        result: result(status, [...warnings]),
        phase: "complete",
        destructive: status === "failed",
      });
      await (service as any).notifyCompletion({
        restoreId: "restore",
        backup,
        snapshot: identitySnapshot,
        destination: { id: "destination", name: "Destination" },
        userId: "admin",
        startedAt: "2026-01-01T00:00:00.000Z",
        result: result(status, [...warnings]),
        phase: "complete",
        destructive: status === "failed",
      });
      expect(sent).toHaveLength(1);
      expect(sent[0].embeds[0].data.title).toBe(expected);
      expect(
        (
          await (r as any).db.get(
            "SELECT completion_dm_status,executor_user_id FROM restore_operations WHERE restore_id='restore'",
          )
        ).completion_dm_status,
      ).toBe("sent");
      expect(
        (
          await (r as any).db.get(
            "SELECT executor_user_id FROM restore_operations WHERE restore_id='restore'",
          )
        ).executor_user_id,
      ).toBe("admin");
    }
  });
  it("records DM delivery failure without changing the restore result", async () => {
    const { r, backup } = await completionFixture();
    const service = new BackupService(r, {
      users: {
        fetch: async () => {
          throw Object.assign(new Error("Cannot send messages to this user"), {
            code: 50007,
          });
        },
      },
    } as any);
    await (service as any).notifyCompletion({
      restoreId: "restore",
      backup,
      snapshot: identitySnapshot,
      destination: { id: "destination", name: "Destination" },
      userId: "admin",
      startedAt: "2026-01-01T00:00:00.000Z",
      result: result("completed"),
      phase: "complete",
      destructive: true,
    });
    const row = await (r as any).db.get(
      "SELECT completion_dm_status,completion_dm_failure_reason FROM restore_operations WHERE restore_id='restore'",
    );
    expect(row.completion_dm_status).toBe("failed");
    expect(row.completion_dm_failure_reason).toContain("50007");
  });
  it("starts one restore progress DM and edits that same message into the final result", async () => {
    const { r, backup } = await completionFixture();
    const sent: any[] = [];
    const edits: any[] = [];
    const service = new BackupService(r, {
      users: {
        fetch: async () => ({
          send: async (payload: any) => {
            sent.push(payload);
            return { edit: async (next: any) => edits.push(next) };
          },
        }),
      },
    } as any);
    const startedAtMs = Date.now();
    const progress = await (service as any).startRestoreProgressDm({
      backup,
      destination: { id: "destination", name: "Destination" },
      userId: "admin",
      restoreId: "restore",
      startedAtMs,
    });
    await (service as any).updateRestoreProgressDm(progress, {
      backup,
      destination: { id: "destination", name: "Destination" },
      phase: "Roles",
      startedAtMs,
      completed: 2,
      total: 4,
      force: true,
      restoreId: "restore",
    });
    await (service as any).notifyCompletion({
      restoreId: "restore",
      backup,
      snapshot: identitySnapshot,
      destination: { id: "destination", name: "Destination" },
      userId: "admin",
      startedAt: new Date(startedAtMs).toISOString(),
      result: result("completed"),
      phase: "complete",
      destructive: true,
      progressDm: progress,
    });
    expect(sent).toHaveLength(1);
    expect(sent[0].embeds[0].data.title).toBe("Server Restore Running");
    expect(edits).toHaveLength(2);
    expect(edits.at(-1).embeds[0].data.title).toBe("Server Restore Completed");
  });
  it("treats a disabled executor DM as observability-only", async () => {
    const service = new BackupService(
      {} as any,
      {
        users: {
          fetch: async () => {
            throw Object.assign(new Error("Cannot send messages to this user"), {
              code: 50007,
            });
          },
        },
      } as any,
    );
    const backup = { backup_id: "backup", source_guild_name: "Source" } as any;
    const progress = await (service as any).startRestoreProgressDm({
      backup,
      destination: { id: "destination", name: "Destination" },
      userId: "admin",
      restoreId: "restore",
      startedAtMs: Date.now(),
    });
    await expect(
      (service as any).updateRestoreProgressDm(progress, {
        backup,
        destination: { id: "destination", name: "Destination" },
        phase: "Roles",
        startedAtMs: Date.now(),
        restoreId: "restore",
      }),
    ).resolves.toBeUndefined();
    expect(progress.message).toBeUndefined();
  });
  it("maps guild identity channels and makes an absent icon authoritative", async () => {
    const edits: any[] = [];
    const service = new BackupService({} as any, {} as any);
    await (service as any).restoreGuildSettings(
      { edit: async (payload: any) => edits.push(payload) },
      {
        guild: {
          name: "Deforged Games",
          description: null,
          verificationLevel: 3,
          explicitContentFilter: 2,
          defaultMessageNotifications: 1,
          afkTimeout: 300,
          afkChannelId: "afk",
          systemChannelId: "system",
          systemChannelFlags: "0",
          rulesChannelId: "rules",
          publicUpdatesChannelId: "updates",
          safetyAlertsChannelId: "alerts",
          preferredLocale: "en-GB",
          assets: { icon: { state: "absent" } },
        },
      },
      new Map([
        ["afk", "d-afk"],
        ["system", "d-system"],
        ["rules", "d-rules"],
        ["updates", "d-updates"],
        ["alerts", "d-alerts"],
      ]),
      [],
      [],
      [],
    );
    expect(edits).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ name: "Deforged Games" }),
        expect.objectContaining({ description: null }),
        expect.objectContaining({ afkChannel: "d-afk" }),
        expect.objectContaining({ systemChannel: "d-system" }),
        expect.objectContaining({
          systemChannelFlags: expect.any(SystemChannelFlagsBitField),
        }),
        expect.objectContaining({ safetyAlertsChannel: "d-alerts" }),
        expect.objectContaining({ icon: null }),
      ]),
    );
    expect(
      edits.find((payload) => "systemChannelFlags" in payload)
        ?.systemChannelFlags.bitfield,
    ).toBe(0);
  });
  it("captures icon bytes with a hash and restores the retained binary rather than a URL", async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = async () =>
      new Response(Buffer.from("icon-bytes"), {
        headers: { "content-type": "image/png" },
      });
    try {
      const service = new BackupService({} as any, {} as any);
      const captured = await (service as any).captureGuildAssets(
        {
          iconURL: () => "https://cdn.example/icon.png",
          bannerURL: () => null,
          splashURL: () => null,
          discoverySplashURL: () => null,
        },
        "backup",
        [],
      );
      expect(captured[0]).toMatchObject({
        kind: "icon",
        data: Buffer.from("icon-bytes"),
        descriptor: { state: "captured", contentType: "image/png" },
      });
      const edits: any[] = [];
      await (service as any).restoreGuildAssets(
        { edit: async (payload: any) => edits.push(payload) },
        { assets: { icon: captured[0].descriptor } },
        captured.map((asset: any) => ({
          backup_id: asset.backup_id,
          kind: asset.kind,
          content_type: asset.content_type,
          size_bytes: asset.size_bytes,
          sha256: asset.sha256,
          data: asset.data,
          created_at: asset.created_at,
        })),
        [],
        [],
      );
      expect(edits[0].icon).toBe(
        `data:image/png;base64,${Buffer.from("icon-bytes").toString("base64")}`,
      );
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
  it("rejects a corrupt stored icon before destination mutation", () => {
    const service = new BackupService({} as any, {} as any);
    expect(() =>
      (service as any).validateAssets(
        {
          schemaVersion: 2,
          guild: {
            assets: {
              icon: { state: "captured", sha256: "bad", sizeBytes: 3 },
            },
          },
        },
        [
          {
            kind: "icon",
            sha256: "bad",
            size_bytes: 3,
            data: Buffer.from("bad"),
          },
        ],
      ),
    ).toThrow("icon asset integrity");
  });
});

describe("administrator component rechecks", () => {
  const removedAdministratorInteraction = (customId: string) => {
    const replies: any[] = [];
    return {
      replies,
      guild: {
        id: "a",
        members: {
          fetch: async () => ({
            id: "admin",
            guild: { id: "a" },
            permissions: { has: () => false },
          }),
        },
      },
      user: { id: "admin" },
      customId,
      isRepliable: () => true,
      reply: async (payload: any) => {
        replies.push(payload);
      },
    };
  };
  it("rejects admin dashboard and backup actions after Administrator permission is removed", async () => {
    const r = await repo();
    let backupCalls = 0;
    const ui = new AdminUi(
      r,
      {} as any,
      {} as any,
      {} as any,
      {
        create: async () => {
          backupCalls++;
        },
      } as any,
      () => "0s",
    );
    const interaction = removedAdministratorInteraction(
      "adminui:backup:create:a",
    );
    await ui.handle(interaction as any);
    expect(backupCalls).toBe(0);
    expect(interaction.replies[0].content).toContain("Administrator");
  });
  it("rejects embed actions after Administrator permission is removed", async () => {
    const r = await repo();
    const bot = new CookieBot(r);
    const interaction = removedAdministratorInteraction("embedui:create:a");
    await (bot as any).handleEmbedUi(interaction);
    expect(interaction.replies[0].content).toContain("Administrator");
    bot.client.destroy();
  });
});

describe("destructive restore progress lifecycle", () => {
  it("acknowledges manual backup creation before member acquisition starts", async () => {
    const r = await repo();
    let deferred = false;
    const ui = new AdminUi(
      r,
      {} as any,
      {} as any,
      {} as any,
      {
        create: async () => {
          expect(deferred).toBe(true);
          return {
            source_guild_name: "Source",
            created_at: "2026-01-01T00:00:00.000Z",
            role_count: 0,
            channel_count: 0,
            member_count: 0,
          };
        },
      } as any,
      () => "0s",
    );
    const interaction = {
      customId: "adminui:backupcreate:confirm:a",
      guild: {
        id: "a",
        members: {
          fetch: async () => ({
            id: "admin",
            guild: { id: "a" },
            permissions: { has: () => true },
          }),
        },
      },
      user: { id: "admin" },
      deferUpdate: async () => {
        deferred = true;
      },
      editReply: async () => undefined,
    };
    await ui.handle(interaction as any);
    expect(deferred).toBe(true);
  });
  it("continues and persists restore work when its progress message is deleted", async () => {
    const r = await repo();
    const phases: string[] = [];
    let deferred = false;
    const ui = new AdminUi(
      r,
      {} as any,
      {} as any,
      {} as any,
      {
        restore: async (
          _backupId: string,
          _guild: any,
          _userId: string,
          report: any,
        ) => {
          for (const phase of [
            "Safety Backup",
            "Destructive cleanup begins",
            "Roles",
          ]) {
            phases.push(phase);
            await report(
              phase,
              phase === "Roles" ? 1 : undefined,
              phase === "Roles" ? 1 : undefined,
            );
          }
          return {
            status: "completed",
            warnings: [],
            criticalFailures: [],
            roles: 1,
            channels: 1,
            overwrites: 1,
            members: 1,
          };
        },
      } as any,
      () => "0s",
    );
    const interaction = {
      id: "interaction-1",
      customId: "adminui:backuprestore:confirm-backup-1:a",
      guild: {
        id: "a",
        members: {
          fetch: async () => ({
            id: "admin",
            guild: { id: "a" },
            permissions: { has: () => true },
          }),
        },
      },
      user: { id: "admin" },
      deferUpdate: async () => {
        deferred = true;
      },
      editReply: async () => {
        throw Object.assign(new Error("Unknown Message"), {
          code: 10008,
          status: 404,
        });
      },
      reply: async () => undefined,
    };

    await expect(ui.handle(interaction as any)).resolves.toBeUndefined();
    expect(deferred).toBe(true);
    expect(phases).toEqual([
      "Safety Backup",
      "Destructive cleanup begins",
      "Roles",
    ]);
  });
});

describe("command synchronization", () => {
  const state = () => {
    let saved: any;
    return {
      commandSyncState: async () => saved,
      saveCommandSyncState: async (...args: any[]) => {
        saved = {
          application_id: args[0],
          schema_fingerprint: args[1],
          guild_cleanup_complete: args[2] ? 1 : 0,
        };
      },
      saved: () => saved,
    };
  };
  const manager = (existing = 0, fail = false, live = false) => {
    const calls: any[][] = [];
    const liveSet = new Map(
      commands.map((command: any) => [
        command.name,
        {
          ...command,
          id: `id-${command.name}`,
          applicationId: "app",
          defaultMemberPermissions: command.default_member_permissions
            ? { bitfield: BigInt(command.default_member_permissions) }
            : null,
        },
      ]),
    );
    return {
      calls,
      set: async (value: any[]) => {
        calls.push(value);
        if (fail) throw new Error("Discord unavailable");
      },
      fetch: async () => (live ? liveSet : { size: existing }),
    };
  };
  it("has no development-guild registration configuration", () => {
    expect(config).not.toHaveProperty("developmentGuildId");
    expect(config).not.toHaveProperty("commandRegistrationMode");
  });
  it("uses the only global registration path, verifies the live result, and clears legacy guild commands", async () => {
    const repo = state(),
      global = manager(0, false, true),
      legacy = manager(2),
      empty = manager(0);
    const client = {
      application: { id: "app", commands: global },
      user: { id: "app" },
      guilds: {
        cache: new Map([
          ["a", { id: "a", commands: legacy }],
          ["b", { id: "b", commands: empty }],
        ]),
      },
    };
    const result = await new CommandSynchronizer(repo as any).synchronize(
      client as any,
    );
    expect(global.calls).toEqual([commands]);
    expect(legacy.calls).toEqual([[]]);
    expect(empty.calls).toEqual([]);
    expect(result).toMatchObject({ cleaned: 1, failed: 0 });
    expect(repo.saved()).toMatchObject({
      schema_fingerprint: commandSchemaFingerprint(),
      guild_cleanup_complete: 1,
    });
  });
  it("fails synchronization when Discord returns mismatched live command visibility metadata", async () => {
    const global = manager(0, false, true);
    global.fetch = async () =>
      new Map(
        commands.map((command: any) => [
          command.name,
          {
            ...command,
            id: command.name,
            defaultMemberPermissions: command.default_member_permissions
              ? { bitfield: BigInt(command.default_member_permissions) }
              : null,
            integrationTypes: command.name === "admin" ? [1] : [0],
          },
        ]),
      );
    const client = {
      application: { id: "app", commands: global },
      user: { id: "app" },
      guilds: { cache: new Map() },
    };
    await expect(
      new CommandSynchronizer(state() as any).synchronize(client as any),
    ).rejects.toThrow("/admin");
  });
  it("continues cleanup after one guild failure and leaves it pending for retry", async () => {
    const repo = state(),
      global = manager(0, false, true),
      failed = manager(1, true),
      succeeds = manager(1);
    const client = {
      application: { id: "app", commands: global },
      user: { id: "app" },
      guilds: {
        cache: new Map([
          ["bad", { id: "bad", commands: failed }],
          ["good", { id: "good", commands: succeeds }],
        ]),
      },
    };
    const result = await new CommandSynchronizer(repo as any).synchronize(
      client as any,
    );
    expect(result).toMatchObject({ cleaned: 1, failed: 1 });
    expect(succeeds.calls).toEqual([[]]);
    expect(repo.saved()).toMatchObject({ guild_cleanup_complete: 0 });
  });
  it("rejects a configured application ID mismatch before any REST write", async () => {
    const global = manager(),
      client = {
        application: { id: "authenticated", commands: global },
        user: { id: "authenticated" },
        guilds: { cache: new Map() },
      };
    await expect(
      new CommandSynchronizer(state() as any, "configured").synchronize(
        client as any,
      ),
    ).rejects.toThrow("does not match");
    expect(global.calls).toEqual([]);
  });
  it("has a deterministic schema fingerprint and only clears legacy commands on guild join", async () => {
    expect(commandSchemaFingerprint()).toBe(commandSchemaFingerprint());
    const legacy = manager(1),
      global = manager();
    const sync = new CommandSynchronizer(state() as any);
    await sync.clearGuildOnJoin({ id: "joined", commands: legacy } as any);
    expect(legacy.calls).toEqual([[]]);
    expect(global.calls).toEqual([]);
  });
  it("reports live global metadata, guild commands, overrides, and target member permissions without mutation", async () => {
    const global = manager(0, false, true),
      guildCommands = manager(0);
    const permissions = {
      fetch: async () =>
        new Map([["id-admin", [{ id: "role", type: 1, permission: false }]]]),
    };
    const client = {
      application: { id: "app", commands: { ...global, permissions } },
      user: { id: "bot", username: "Cookie" },
      guilds: {
        cache: new Map([
          [
            "guild",
            {
              id: "guild",
              name: "Affected",
              ownerId: "owner",
              commands: guildCommands,
              members: {
                fetch: async () => ({
                  id: "founder",
                  permissions: {
                    has: (permission: string) =>
                      permission === "Administrator" ||
                      permission === "UseApplicationCommands",
                    bitfield: 8n,
                  },
                }),
              },
            },
          ],
        ]),
      },
    };
    const report = await new CommandSynchronizer({
      ...state(),
      isStaff: async () => false,
    } as any).inspect(client as any, "guild", "founder");
    expect(report.application).toMatchObject({ id: "app", botId: "bot" });
    expect(report.globalCommands).toHaveLength(5);
    expect(report.guilds[0]).toMatchObject({
      commands: [],
      member: { administrator: true, useApplicationCommands: true },
    });
    expect(report.guilds[0].permissionOverrides).toEqual([
      {
        commandId: "id-admin",
        permissions: [{ id: "role", type: 1, permission: false }],
      },
    ]);
    expect(
      report.comparisons.find((comparison) => comparison.left === "admin")
        ?.differences,
    ).toEqual([]);
    expect(global.calls).toEqual([]);
  });
});

describe("global embeds", () => {
  it("validates Discord limits and uses case-insensitive unique names", async () => {
    const r = await repo();
    expect(validateEmbedPayload('{"embeds":[{"title":"Hello"}]}').ok).toBe(
      true,
    );
    expect(
      validateEmbedPayload(
        '{"embeds":[{"description":"' + "x".repeat(4097) + '"}]}',
      ).ok,
    ).toBe(false);
    expect(await r.saveEmbed("Welcome", '{"content":"hi"}', "admin")).toBe(
      true,
    );
    expect(await r.saveEmbed("welcome", '{"content":"bye"}', "admin")).toBe(
      false,
    );
    expect(await r.embed("WELCOME")).toMatchObject({ name: "Welcome" });
  });
  it("rejects unsupported message payload keys such as interactive components", () =>
    expect(validateEmbedPayload('{"content":"hi","components":[]}').ok).toBe(
      false,
    ));
  it("tracks sent instances by template and guild without cross-guild leakage", async () => {
    const r = await repo();
    await r.saveEmbed("Rules", '{"content":"rules"}', "admin");
    const embed = await r.embed("Rules");
    await r.trackSentEmbed(embed!.template_id, "a", "channel-a", "message-a");
    await r.trackSentEmbed(embed!.template_id, "b", "channel-b", "message-b");
    expect(await r.sentEmbeds(embed!.template_id, "a")).toMatchObject([
      { channel_id: "channel-a", message_id: "message-a" },
    ]);
    expect(await r.sentEmbeds(embed!.template_id, "b")).toMatchObject([
      { channel_id: "channel-b", message_id: "message-b" },
    ]);
  });
  it("normalizes empty exported attachment fields and rejects meaningful unmanaged attachments", () => {
    const empty = validateEmbedPayload(
      '{"content":null,"embeds":[{"title":"Hello"}],"attachments":[]}',
    );
    expect(empty).toMatchObject({ ok: true });
    if (empty.ok) expect(empty.value).not.toHaveProperty("attachments");
    expect(
      validateEmbedPayload(
        '{"content":"x","attachments":[{"filename":"x.png"}]}',
      ).ok,
    ).toBe(false);
  });
  it("stores attachment bytes safely and resolves attachment URLs through the shared renderer", async () => {
    const r = await repo();
    await r.saveEmbed(
      "Image",
      '{"embeds":[{"image":{"url":"attachment://card.png"}}]}',
      "admin",
    );
    const template = (await r.embed("Image"))!;
    const record = attachmentRecord(
      template.template_id,
      "card.png",
      "image/png",
      Buffer.from("image"),
      "admin",
    );
    await r.saveAttachment(record);
    const rendered = await renderTemplate(r, template);
    expect(rendered.files).toHaveLength(1);
    expect(rendered.embeds?.[0].data.image?.url).toBe("attachment://card.png");
  });
  it("rejects unsafe attachment names and detects missing attachment references", async () => {
    expect(() => sanitizeAttachmentFilename("../secret.png")).toThrow(
      "invalid",
    );
    const r = await repo();
    await r.saveEmbed(
      "Broken",
      '{"embeds":[{"image":{"url":"attachment://missing.png"}}]}',
      "admin",
    );
    await expect(renderTemplate(r, (await r.embed("Broken"))!)).rejects.toThrow(
      "missing attachment",
    );
  });
  it("claims a due schedule once and keeps history without deleting it", async () => {
    const r = await repo();
    await r.saveEmbed("Soon", '{"content":"hi"}', "admin");
    const template = (await r.embed("Soon"))!,
      stamp = new Date().toISOString();
    await r.createSchedule({
      id: "schedule",
      template_id: template.template_id,
      guild_id: "a",
      channel_id: "c",
      scheduled_for_utc: stamp,
      timezone: "Europe/Ljubljana",
      created_by_user_id: "admin",
      created_at: stamp,
      updated_by_user_id: "admin",
      updated_at: stamp,
      status: "scheduled",
      sent_at: null,
      sent_message_id: null,
      failure_code: null,
      failure_reason: null,
      attempt_count: 0,
      last_attempt_at: null,
      processing_started_at: null,
      template_name_snapshot: "Soon",
    });
    expect((await r.claimDueSchedule(stamp))?.status).toBe("processing");
    expect(await r.claimDueSchedule(stamp)).toBeUndefined();
    await r.updateSchedule("schedule", "scheduler", {
      status: "sent",
      sent_at: stamp,
      sent_message_id: "message",
    });
    expect(await r.scheduleHistory("a")).toMatchObject([
      { id: "schedule", status: "sent", sent_message_id: "message" },
    ]);
  });
  it("converts Ljubljana CET/CEST wall times and rejects DST gaps and ambiguity", () => {
    expect(
      parseLjubljanaSchedule(
        "2026-01-15 18:30",
        new Date("2026-01-01"),
      ).utc.toISOString(),
    ).toContain("17:30:00");
    expect(
      parseLjubljanaSchedule(
        "2026-07-15 18:30",
        new Date("2026-01-01"),
      ).utc.toISOString(),
    ).toContain("16:30:00");
    expect(() =>
      parseLjubljanaSchedule("2026-03-29 02:30", new Date("2026-01-01")),
    ).toThrow("does not exist");
    expect(() =>
      parseLjubljanaSchedule("2026-10-25 02:30", new Date("2026-01-01")),
    ).toThrow("ambiguous");
  });
  it("uses explicit allowed mentions and never parses template mention text by default", () => {
    const base = { content: "@everyone ordinary text" };
    expect(withDeliveryMention(base, noMention)).toMatchObject({
      content: "@everyone ordinary text",
      allowedMentions: { parse: [], roles: [] },
    });
    expect(
      withDeliveryMention(base, {
        roles: ["role-a", "role-b"],
        everyone: false,
        here: false,
      }),
    ).toMatchObject({
      content: "<@&role-a> <@&role-b>\n@everyone ordinary text",
      allowedMentions: { parse: [], roles: ["role-a", "role-b"] },
    });
    expect(
      withDeliveryMention(base, { roles: [], everyone: true, here: false })
        .allowedMentions,
    ).toMatchObject({ parse: ["everyone"] });
  });
  it("drops deleted or unpermitted scheduled mentions without failing delivery", () => {
    const guild = {
      roles: {
        cache: new Map([
          ["live", { managed: false }],
          ["managed", { managed: true }],
        ]),
      },
      members: { me: { permissions: { has: () => false } } },
    };
    expect(
      resolveMention(guild as any, {
        roles: ["live", "missing", "managed"],
        everyone: true,
        here: false,
      }),
    ).toMatchObject({
      mention: { roles: ["live"], everyone: false, here: false },
      warning: expect.any(String),
    });
  });
  it("routes native role selection into a manual delivery mention session", async () => {
    const r = await repo(),
      bot = new CookieBot(r);
    (bot as any).embedSessions.set("token", {
      guildId: "g",
      userId: "admin",
      name: "Notice",
      channelId: "channel",
      mention: { roles: [], everyone: false, here: false },
    });
    const updates: any[] = [];
    const guild: any = {
      id: "g",
      members: {
        fetch: async () => ({ permissions: { has: () => true } }),
        me: { permissions: { has: () => true } },
      },
      roles: {
        cache: new Map([
          ["r1", { managed: false }],
          ["r2", { managed: false }],
        ]),
      },
    };
    await (bot as any).handleEmbedUi({
      guild,
      user: { id: "admin" },
      customId: "embedui:mentionroles:manual:token",
      values: ["r1", "r2"],
      isRoleSelectMenu: () => true,
      isButton: () => false,
      isStringSelectMenu: () => false,
      update: async (payload: any) => updates.push(payload),
    });
    expect((bot as any).embedSessions.get("token").mention).toEqual({
      roles: ["r1", "r2"],
      everyone: false,
      here: false,
    });
    expect(updates[0].embeds[0].data.title).toBe("Mention Settings");
    bot.client.destroy();
  });
});

describe("admin role management", () => {
  const roleAt = (id: string, position: number) => ({
    id,
    name: `role-${id}`,
    position,
    comparePositionTo: (other: any) => position - other.position,
  });
  const fixture = (options: { actorPosition?: number; assignable?: boolean; failFor?: string } = {}) => {
    const role = roleAt("r1", 5);
    const calls: string[] = [];
    const memberWith = (id: string, bot: boolean, roles: string[]) => {
      const held = new Set(roles);
      return {
        id,
        user: { bot },
        permissions: { has: () => true },
        roles: {
          cache: held,
          highest: roleAt(`top-${id}`, id === "admin" ? (options.actorPosition ?? 10) : 1),
          add: async (r: any) => {
            if (id === options.failFor) throw new Error("Missing Permissions");
            calls.push(`add:${id}`);
            held.add(r.id);
          },
          remove: async (r: any) => {
            calls.push(`remove:${id}`);
            held.delete(r.id);
          },
        },
      };
    };
    const members = new Map(
      [
        memberWith("admin", false, []),
        memberWith("human1", false, []),
        memberWith("human2", false, ["r1"]),
        memberWith("bot1", true, ["r1"]),
        memberWith("bot2", true, []),
      ].map((m) => [m.id, m]),
    );
    let fullFetches = 0;
    const guild = {
      id: "a",
      name: "Guild",
      ownerId: "owner",
      roles: { cache: new Map<string, any>([["r1", role], ["a", roleAt("a", 0)]]) },
      members: {
        cache: members,
        fetch: async (id?: string) => {
          // Mirrors Discord: a second full member request within 30s is rejected.
          if (id === undefined && fullFetches++)
            throw new Error("Request with opcode 8 was rate limited. Retry after 29.5 seconds.");
          if (id === undefined) return members;
          const member = members.get(id);
          if (!member) throw new Error("Unknown Member");
          return member;
        },
      },
    };
    const audits: any[] = [];
    const ui = new AdminUi(
      {} as any,
      {} as any,
      {} as any,
      { assignable: () => options.assignable ?? true } as any,
      {} as any,
      () => "0s",
      { record: (event: any) => audits.push(event) } as any,
    );
    const interact = (customId: string, values?: string[]) => {
      const out = { updates: [] as any[], replies: [] as any[], edits: [] as any[] };
      const interaction = {
        id: "i1",
        customId,
        values,
        guild,
        user: { id: "admin", tag: "admin#0" },
        update: async (payload: any) => void out.updates.push(payload),
        reply: async (payload: any) => void out.replies.push(payload),
        deferUpdate: async () => undefined,
        editReply: async (payload: any) => void out.edits.push(payload),
      };
      return { interaction, out };
    };
    return { ui, calls, members, audits, interact };
  };
  const ids = (payload: any) =>
    payload.components.flatMap((row: any) => row.components.map((c: any) => c.data.custom_id));

  it("shows a single Remove/Add Roles entry on the dashboard that opens add and remove choices", async () => {
    const r = await repo();
    const { interact } = fixture();
    const ui = new AdminUi(r, {} as any, {} as any, {} as any, {} as any, () => "0s");
    const open = interact("");
    await ui.open(open.interaction as any);
    expect(ids(open.out.replies[0])).toContain("adminui:nav:roles:a");
    const nav = interact("adminui:nav:roles:a");
    await ui.handle(nav.interaction as any);
    expect(ids(nav.out.updates[0])).toEqual([
      "adminui:rolemgmt:add:a",
      "adminui:rolemgmt:remove:a",
      "adminui:nav:dashboard:a",
    ]);
  });

  it("walks add → role → target and offers user, everyone and bots", async () => {
    const { ui, interact } = fixture();
    const pick = interact("adminui:rolemgmt:add:a");
    await ui.handle(pick.interaction as any);
    expect(ids(pick.out.updates[0])[0]).toBe("adminui:rmrole:add:a");
    const select = interact("adminui:rmrole:add:a", ["r1"]);
    await ui.handleSelect(select.interaction as any);
    expect(ids(select.out.updates[0])).toEqual([
      "adminui:rmtarget:add-r1-user:a",
      "adminui:rmtarget:add-r1-everyone:a",
      "adminui:rmtarget:add-r1-bots:a",
      "adminui:nav:roles:a",
    ]);
  });

  it("adds a role to everyone who lacks it after confirmation", async () => {
    const { ui, calls, members, audits, interact } = fixture();
    const confirm = interact("adminui:rmtarget:add-r1-everyone:a");
    await ui.handle(confirm.interaction as any);
    expect(confirm.out.updates[0].content).toContain("**3** members");
    expect(calls).toEqual([]);
    const apply = interact("adminui:rmapply:add-r1-everyone:a");
    await ui.handle(apply.interaction as any);
    expect(calls.sort()).toEqual(["add:admin", "add:bot2", "add:human1"]);
    expect([...members.values()].every((m) => m.roles.cache.has("r1"))).toBe(true);
    expect(apply.out.edits[0].components).toEqual([]);
    expect(apply.out.edits.at(-1).content).toContain("Added <@&r1> to 3 members");
    expect(audits[0]).toMatchObject({ title: "Role Added", details: { Target: "Everyone", Changed: 3, Failed: 0 } });
  });

  it("removes a role only from bots that hold it", async () => {
    const { ui, calls, interact } = fixture();
    const confirm = interact("adminui:rmtarget:remove-r1-bots:a");
    await ui.handle(confirm.interaction as any);
    expect(confirm.out.updates[0].content).toContain("**1** bot?");
    await ui.handle(interact("adminui:rmapply:remove-r1-bots:a").interaction as any);
    expect(calls).toEqual(["remove:bot1"]);
  });

  it("adds or removes a role for a specific user and skips no-op changes", async () => {
    const { ui, calls, interact } = fixture();
    const target = interact("adminui:rmtarget:add-r1-user:a");
    await ui.handle(target.interaction as any);
    expect(ids(target.out.updates[0])[0]).toBe("adminui:rmuser:add-r1:a");
    const add = interact("adminui:rmuser:add-r1:a", ["human1"]);
    await ui.handleSelect(add.interaction as any);
    expect(add.out.updates[0].content).toContain("Added <@&r1> to 1 member.");
    const again = interact("adminui:rmuser:add-r1:a", ["human1"]);
    await ui.handleSelect(again.interaction as any);
    expect(again.out.updates[0].content).toContain("already has");
    const remove = interact("adminui:rmuser:remove-r1:a", ["human2"]);
    await ui.handleSelect(remove.interaction as any);
    expect(calls).toEqual(["add:human1", "remove:human2"]);
    const missing = interact("adminui:rmuser:add-r1:a", ["ghost"]);
    await ui.handleSelect(missing.interaction as any);
    expect(missing.out.replies[0].content).toContain("not a member");
  });

  it("reports per-member failures without stopping the run", async () => {
    const { ui, calls, audits, interact } = fixture({ failFor: "human1" });
    const apply = interact("adminui:rmapply:add-r1-everyone:a");
    await ui.handle(apply.interaction as any);
    expect(calls.sort()).toEqual(["add:admin", "add:bot2"]);
    expect(apply.out.edits.at(-1).content).toContain("Failed for 1: <@human1>");
    expect(audits[0]).toMatchObject({ color: "danger", details: { Changed: 2, Failed: 1 } });
  });

  it("refuses @everyone, unassignable roles and roles at or above the actor", async () => {
    const cases: [Parameters<typeof fixture>[0], string, string[] | undefined, string][] = [
      [{}, "adminui:rmrole:add:a", ["a"], "cannot be assigned"],
      [{ assignable: false }, "adminui:rmrole:add:a", ["r1"], "Cookie cannot use that role"],
      [{ actorPosition: 5 }, "adminui:rmrole:add:a", ["r1"], "below your own highest role"],
      [{ actorPosition: 5 }, "adminui:rmapply:add-r1-everyone:a", undefined, "below your own highest role"],
      [{}, "adminui:rmapply:grant-r1-everyone:a", undefined, "Unknown role action"],
    ];
    for (const [options, customId, values, message] of cases) {
      const { ui, calls, interact } = fixture(options);
      const step = interact(customId, values);
      if (values) await ui.handleSelect(step.interaction as any);
      else await ui.handle(step.interaction as any);
      expect(step.out.replies[0].content).toContain(message);
      expect(step.out.updates).toEqual([]);
      expect(calls).toEqual([]);
    }
  });

  it("rejects role management once Administrator permission is gone", async () => {
    const { ui, calls, members, interact } = fixture();
    members.get("admin")!.permissions.has = () => false;
    const step = interact("adminui:rmapply:add-r1-everyone:a");
    await ui.handle(step.interaction as any);
    expect(step.out.replies[0].content).toContain("Administrator");
    expect(calls).toEqual([]);
  });
});

describe("verification channel honeypot", () => {
  const setup = async () => {
    const r = await repo();
    await r.updateVerification("g", { channel_id: "verify" });
    const audits: any[] = [];
    const service = new VerificationService(r, { record: (e: any) => audits.push(e) } as any);
    const events: string[] = [];
    const memberOf = (id: string, opts: { admin?: boolean; kickable?: boolean; dmFails?: boolean } = {}) => ({
      id,
      user: { tag: `${id}#0` },
      kickable: opts.kickable ?? true,
      permissions: { has: () => !!opts.admin },
      send: async (text: string) => {
        if (opts.dmFails) throw new Error("Cannot send messages to this user");
        events.push(`dm:${id}:${text}`);
      },
      kick: async () => void events.push(`kick:${id}`),
    });
    const message = (member: any, overrides: Record<string, unknown> = {}) => ({
      inGuild: () => true,
      author: { id: member.id, bot: false },
      webhookId: null,
      system: false,
      channelId: "verify",
      guild: { id: "g", name: "DarkDragon", members: { fetch: async () => member } },
      member,
      delete: async () => void events.push(`delete:${member.id}`),
      ...overrides,
    });
    return { service, events, audits, memberOf, message };
  };

  it("deletes the message, DMs the rejoin link, then kicks", async () => {
    const { service, events, audits, memberOf, message } = await setup();
    await service.honeypot(message(memberOf("spammer")) as any);
    expect(events.map((e) => e.split(":").slice(0, 2).join(":"))).toEqual([
      "delete:spammer",
      "dm:spammer",
      "kick:spammer",
    ]);
    expect(events[1]).toContain("**DarkDragon**");
    expect(events[1]).toContain(HONEYPOT_REJOIN_URL);
    expect(HONEYPOT_REJOIN_URL).toBe("https://darkdragon.dev/discord");
    expect(audits[0]).toMatchObject({ title: "Honeypot Kick", details: { "DM Sent": true } });
  });

  it("still kicks when the member has DMs closed", async () => {
    const { service, events, audits, memberOf, message } = await setup();
    await service.honeypot(message(memberOf("spammer", { dmFails: true })) as any);
    expect(events).toEqual(["delete:spammer", "kick:spammer"]);
    expect(audits[0].details["DM Sent"]).toBe(false);
  });

  it("kicks and DMs once for a burst of messages but deletes every one", async () => {
    const { service, events, memberOf, message } = await setup();
    const member = memberOf("spammer");
    await Promise.all([1, 2, 3].map(() => service.honeypot(message(member) as any)));
    expect(events.filter((e) => e.startsWith("delete:"))).toHaveLength(3);
    expect(events.filter((e) => e.startsWith("dm:"))).toHaveLength(1);
    expect(events.filter((e) => e.startsWith("kick:"))).toHaveLength(1);
  });

  it("ignores other channels, bots, webhooks, system messages and administrators", async () => {
    const { service, events, memberOf, message } = await setup();
    const m = memberOf("someone");
    await service.honeypot(message(m, { channelId: "general" }) as any);
    await service.honeypot(message(m, { author: { id: "someone", bot: true } }) as any);
    await service.honeypot(message(m, { webhookId: "hook" }) as any);
    await service.honeypot(message(m, { system: true }) as any);
    await service.honeypot(message(m, { inGuild: () => false }) as any);
    await service.honeypot(message(memberOf("boss", { admin: true })) as any);
    expect(events).toEqual([]);
  });

  it("does nothing until a verification message has been deployed", async () => {
    const { service, events, memberOf, message } = await setup();
    await (service as any).repo.updateVerification("g", { channel_id: null });
    await service.honeypot(message(memberOf("spammer")) as any);
    expect(events).toEqual([]);
  });

  it("deletes but reports a failure when Cookie cannot kick the member", async () => {
    const { service, events, audits, memberOf, message } = await setup();
    await service.honeypot(message(memberOf("mod", { kickable: false })) as any);
    expect(events).toEqual(["delete:mod"]);
    expect(audits[0].title).toBe("Honeypot Kick Failed");
  });

  it("subscribes to guild message events", async () => {
    const bot = new CookieBot(await repo());
    expect(bot.client.options.intents.has(GatewayIntentBits.GuildMessages)).toBe(true);
    expect(bot.client.options.intents.has(GatewayIntentBits.MessageContent)).toBe(false);
    expect(bot.client.listenerCount("messageCreate")).toBe(1);
    bot.client.destroy();
  });
});
