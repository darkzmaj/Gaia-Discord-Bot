import { mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';
import sqlite3 from 'sqlite3';
import { open, type Database } from 'sqlite';
import { migrations } from './migrations.js';

export type SqliteDatabase = Database<sqlite3.Database, sqlite3.Statement>;

export async function createDatabase(filename: string): Promise<SqliteDatabase> {
  await mkdir(dirname(filename), { recursive: true });
  const db = await open({ filename, driver: sqlite3.Database });
  await db.exec('PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;');
  await migrate(db);
  return db;
}

export async function migrate(db: SqliteDatabase): Promise<void> {
  await db.exec('CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)');
  const applied = new Set((await db.all<{ version: number }[]>('SELECT version FROM schema_migrations')).map((row) => row.version));
  for (const migration of migrations) {
    if (applied.has(migration.version)) continue;
    await db.exec('BEGIN IMMEDIATE');
    try {
      await db.exec(migration.sql);
      await db.run('INSERT INTO schema_migrations(version, applied_at) VALUES (?, ?)', migration.version, new Date().toISOString());
      await db.exec('COMMIT');
    } catch (error) {
      await db.exec('ROLLBACK');
      throw error;
    }
  }
}
