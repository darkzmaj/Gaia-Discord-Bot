import { once } from 'node:events';
import { Client, GatewayIntentBits } from 'discord.js';
import { config, requireConfig } from './config.js';
import { createDatabase } from './db/database.js';
import { CookieRepository } from './db/repositories.js';
import { CommandSynchronizer } from './services/command-sync.js';
import { assertCommandSchema, commandSchemaFingerprint, commandNames } from './services/command-sync.js';

let client: Client | undefined;
let db: Awaited<ReturnType<typeof createDatabase>> | undefined;
try {
  if (process.argv.includes('--dry-run')) {
    assertCommandSchema();
    console.log(`Command synchronization dry run: schema=${commandSchemaFingerprint().slice(0, 12)} commands=${commandNames().join(',')}`);
  } else {
  requireConfig();
  db = await createDatabase(config.databasePath);
  client = new Client({ intents: [GatewayIntentBits.Guilds] });
  const ready = once(client, 'clientReady');
  await client.login(config.token);
  await ready;
  const sync = new CommandSynchronizer(new CookieRepository(db), config.applicationId);
  const result = await sync.synchronize(client, true);
  console.log(`Command synchronization complete: application=${result.applicationId} schema=${result.fingerprint.slice(0, 12)} cleaned=${result.cleaned} failed=${result.failed}`);
  process.exitCode = result.failed ? 1 : 0;
  }
} catch (error) {
  console.error(`Command synchronization failed: ${error instanceof Error ? error.message : 'unknown error'}`);
  process.exitCode = 1;
} finally {
  client?.destroy();
  await db?.close();
}
