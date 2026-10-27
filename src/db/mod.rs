pub mod repositories;

use rusqlite::Connection;
use std::path::Path;

/// Shared with the Ruby tooling (`ruby/lib/gaia/database.rb`): both apply `db/migrations/*.sql` in order.
pub const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../db/migrations/0001.sql")),
    (2, include_str!("../../db/migrations/0002.sql")),
    (3, include_str!("../../db/migrations/0003.sql")),
    (4, include_str!("../../db/migrations/0004.sql")),
    (5, include_str!("../../db/migrations/0005.sql")),
    (6, include_str!("../../db/migrations/0006.sql")),
    (7, include_str!("../../db/migrations/0007.sql")),
    (8, include_str!("../../db/migrations/0008.sql")),
    (9, include_str!("../../db/migrations/0009.sql")),
    (10, include_str!("../../db/migrations/0010.sql")),
];

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn create_database(filename: &str) -> anyhow::Result<Connection> {
    if let Some(parent) = Path::new(filename).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut db = Connection::open(filename)?;
    db.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")?;
    migrate(&mut db)?;
    Ok(db)
}

pub fn migrate(db: &mut Connection) -> anyhow::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)")?;
    let applied: Vec<i64> = db
        .prepare("SELECT version FROM schema_migrations")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for (version, sql) in MIGRATIONS {
        if applied.contains(version) {
            continue;
        }
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.execute("INSERT INTO schema_migrations(version, applied_at) VALUES (?, ?)", rusqlite::params![version, now()])?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn migrations_apply_twice() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        super::migrate(&mut db).unwrap();
        super::migrate(&mut db).unwrap();
        let count: i64 = db.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0)).unwrap();
        assert_eq!(count, super::MIGRATIONS.len() as i64);
    }
}
