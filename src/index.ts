import { config, requireConfig } from './config.js';
import { createDatabase } from './db/database.js';
import { CookieRepository } from './db/repositories.js';
import { CookieBot } from './bot.js';
import { logger } from './logger.js';

let shuttingDown = false;
try {
  requireConfig();
  const db = await createDatabase(config.databasePath);
  const bot = new CookieBot(new CookieRepository(db), config.applicationId);
  await bot.start(config.token);
  const shutdown = async (signal: string, exitCode = 0) => {
    if (shuttingDown) return;
    shuttingDown = true;
    logger.info('Cookie shutting down', { signal });
    bot.mutes.stop();
    bot.backups.stop();
    bot.embedScheduler.stop();
    bot.client.destroy();
    await db.close();
    process.exit(exitCode);
  };
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => void shutdown(signal).catch((error) => { logger.error('graceful shutdown failed', { error: error instanceof Error ? error.name : 'unknown' }); process.exit(1); }));
  process.once('unhandledRejection', (reason) => { logger.error('unhandled promise rejection', { error: reason instanceof Error ? reason.name : 'unknown' }); });
  process.once('uncaughtException', (error) => { logger.error('uncaught exception', { error: error.name }); void shutdown('uncaughtException', 1).catch(() => process.exit(1)); });
} catch (error) {
  logger.error('Cookie failed to start', { error: error instanceof Error ? error.message : 'unknown' });
  process.exitCode = 1;
}
