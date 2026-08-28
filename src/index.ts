import { config, requireConfig } from "./config.js";
import { createDatabase } from "./db/database.js";
import { CookieRepository } from "./db/repositories.js";
import { CookieBot } from "./bot.js";
import { logDiscordError, logger } from "./logger.js";

let shuttingDown = false;
try {
  requireConfig();
  const db = await createDatabase(config.databasePath);
  const bot = new CookieBot(new CookieRepository(db), config.applicationId);
  await bot.start(config.token);
  const shutdown = async (signal: string, exitCode = 0) => {
    if (shuttingDown) return;
    shuttingDown = true;
    logger.info("Cookie shutting down", { signal });
    bot.audit.record({ title: "Cookie Shutting Down", category: "Bot", color: "warning", priority: "critical", details: { Signal: signal } });
    for (const guildId of bot.backups.activeRestoreDestinations())
      bot.audit.record({ title: "Restore Interrupted By Shutdown", category: "Backups", color: "danger", priority: "critical", guild: { id: guildId }, reason: `Cookie received ${signal}; durable restore state was left for operator review.` });
    bot.mutes.stop();
    bot.backups.stop();
    bot.embedScheduler.stop();
    await bot.audit.shutdown();
    bot.client.destroy();
    await db.close();
    process.exit(exitCode);
  };
  for (const signal of ["SIGINT", "SIGTERM"])
    process.once(
      signal,
      () =>
        void shutdown(signal).catch((error) => {
          logger.error("graceful shutdown failed", {
            error: error instanceof Error ? error.name : "unknown",
          });
          process.exit(1);
        }),
    );
  process.on("unhandledRejection", (reason) => {
    logDiscordError("process.unhandledRejection", reason, { service: "runtime" });
  });
  process.once("uncaughtException", (error) => {
    logger.error("uncaught exception", { error: error.name });
    void shutdown("uncaughtException", 1).catch(() => process.exit(1));
  });
} catch (error) {
  logger.error("Cookie failed to start", {
    error: error instanceof Error ? error.message : "unknown",
  });
  process.exitCode = 1;
}
