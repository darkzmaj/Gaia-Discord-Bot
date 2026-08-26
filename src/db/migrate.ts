import { config } from '../config.js';
import { createDatabase } from './database.js';

const db = await createDatabase(config.databasePath);
await db.close();
console.log('Migrations completed.');
