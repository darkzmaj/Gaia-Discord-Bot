import { once } from 'node:events';
import { Client, GatewayIntentBits } from 'discord.js';
import { config, requireConfig } from './config.js';
import { createDatabase } from './db/database.js';
import { CookieRepository } from './db/repositories.js';
import { CommandSynchronizer } from './services/command-sync.js';

let client: Client | undefined;
let db: Awaited<ReturnType<typeof createDatabase>> | undefined;
try {
  requireConfig();
  db = await createDatabase(config.databasePath);
  client = new Client({ intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildMembers] });
  const ready = once(client, 'ready');
  await client.login(config.token);
  await ready;
  const sync = new CommandSynchronizer(new CookieRepository(db), config.applicationId);
  const report = await sync.inspect(client, process.env.DIAGNOSTIC_GUILD_ID, process.env.DIAGNOSTIC_USER_ID);
  console.log('COOKIE COMMAND DIAGNOSTIC');
  console.log(JSON.stringify(report, null, 2));
} catch (error) {
  console.error(`Command inspection failed: ${error instanceof Error ? error.message : 'unknown error'}`);
  process.exitCode = 1;
} finally {
  client?.destroy();
  await db?.close();
}
