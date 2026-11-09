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
    (11, include_str!("../../db/migrations/0011.sql")),
    (12, include_str!("../../db/migrations/0012.sql")),
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

    fn before_0011(foreign_keys: bool) -> rusqlite::Connection {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.pragma_update(None, "foreign_keys", foreign_keys).unwrap();
        for (_, sql) in &super::MIGRATIONS[..10] {
            db.execute_batch(sql).unwrap();
        }
        db
    }

    /// 0011 rebuilds the embed tables under foreign_keys=ON without cascading, and adopts legacy
    /// templates only into an unambiguous guild.
    #[test]
    fn per_guild_embed_migration_keeps_data_and_hides_ambiguous_templates() {
        let db = before_0011(true);
        db.execute_batch(
            r#"INSERT INTO guild_settings(guild_id,created_at,updated_at) VALUES ('a','t','t'),('b','t','t');
            INSERT INTO global_embeds(name,payload_json,created_by_id,created_at,updated_at,template_id) VALUES
              ('OnlyA','{}','u','t','t','t1'),('Both','{}','u','t','t','t2'),('Never','{}','u','t','t','t3');
            INSERT INTO sent_embed_messages(embed_id,guild_id,channel_id,message_id,created_at) VALUES ('t1','a','c','m','t'),('t2','a','c','m2','t');
            INSERT INTO scheduled_embed_sends(id,template_id,guild_id,channel_id,scheduled_for_utc,timezone,created_by_user_id,created_at,
              updated_by_user_id,updated_at,status,template_name_snapshot,mention_json)
              VALUES ('s','t2','b','c','t','tz','u','t','u','t','scheduled','Both','{"roles":["1"],"everyone":false,"here":false}');
            INSERT INTO global_embed_attachments(id,template_id,filename,content_type,size_bytes,sha256,data,created_by_id,created_at)
              VALUES ('x','t1','a.png','image/png',1,'h',x'00','u','t');"#,
        )
        .unwrap();
        db.execute_batch(super::MIGRATIONS[10].1).unwrap();
        let owner = |id: &str| {
            db.query_row("SELECT guild_id FROM embed_templates WHERE template_id=?", [id], |r| r.get::<_, Option<String>>(0)).unwrap()
        };
        assert_eq!(owner("t1").as_deref(), Some("a"));
        assert_eq!(owner("t2"), None, "used in two guilds: hidden");
        assert_eq!(owner("t3"), None, "never used and two guilds known: hidden");
        let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM embed_attachments"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM sent_embed_messages"), 2);
        assert_eq!(count(r#"SELECT COUNT(*) FROM scheduled_embed_sends WHERE mention_json LIKE '%"1"%'"#), 1);
        assert!(db.prepare("SELECT 1 FROM global_embeds").is_err());
        let parents: Vec<String> = db
            .prepare("SELECT \"table\" FROM pragma_foreign_key_list('sent_embed_messages') UNION ALL SELECT \"table\" FROM pragma_foreign_key_list('scheduled_embed_sends')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(parents, ["embed_templates", "embed_templates"]);
        assert_eq!(count("SELECT COUNT(*) FROM pragma_foreign_key_check"), 0);
    }

    #[test]
    fn per_guild_embed_migration_adopts_unused_templates_into_the_only_guild() {
        let db = before_0011(false);
        db.execute_batch(
            "INSERT INTO guild_settings(guild_id,created_at,updated_at) VALUES ('a','t','t');
             INSERT INTO global_embeds(name,payload_json,created_by_id,created_at,updated_at,template_id) VALUES ('Never','{}','u','t','t','t3');",
        )
        .unwrap();
        db.execute_batch(super::MIGRATIONS[10].1).unwrap();
        let owner: Option<String> = db.query_row("SELECT guild_id FROM embed_templates", [], |r| r.get(0)).unwrap();
        assert_eq!(owner.as_deref(), Some("a"));
    }
}
