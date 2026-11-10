//! Load test, run explicitly: `STRESS_USERS=1000 cargo test --release stress -- --ignored --nocapture`.
//! A wave of members joins at the same moment (AutoRole + member log), admins open `/admin` at the same
//! moment, then every member leaves at once. The bot runs its real code paths against a real SQLite file
//! (WAL); only Discord is faked, in the same process, so the reported peak memory includes the fake and is
//! an upper bound for the bot alone.
use crate::bot::{GaiaBot, Handler};
use crate::db::repositories::GaiaRepository;
use crate::test_support::*;
use rusqlite::types::Value as Sql;
use serde_json::json;
use serenity::all::*;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const FIRST_USER: u64 = 100_000;
const ADMIN: u64 = 8;
const AUTOROLE: u64 = 25;
const MEMBER_LOG: u64 = 71;

fn peak_rss_mib() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with("VmHWM"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or("n/a (not Linux)".into(), |kb| format!("{} MiB", kb / 1024))
}

/// Runs every future at once and prints throughput, latency percentiles and peak memory.
async fn burst<F: Future<Output = ()> + Send + 'static>(phase: &str, work: Vec<F>) {
    let wall = Instant::now();
    let tasks: Vec<_> = work
        .into_iter()
        .map(|f| {
            tokio::spawn(async move {
                let t = Instant::now();
                f.await;
                t.elapsed()
            })
        })
        .collect();
    let mut latencies: Vec<Duration> = Vec::new();
    for task in tasks {
        latencies.push(task.await.unwrap());
    }
    let wall = wall.elapsed();
    latencies.sort();
    let at = |p: f64| latencies[((latencies.len() - 1) as f64 * p) as usize].as_millis();
    println!(
        "STRESS {phase}: {} events in {:.2}s = {:.0}/s | latency p50 {}ms p95 {}ms p99 {}ms max {}ms | peak memory {}",
        latencies.len(),
        wall.as_secs_f64(),
        latencies.len() as f64 / wall.as_secs_f64(),
        at(0.50),
        at(0.95),
        at(0.99),
        at(1.0),
        peak_rss_mib()
    );
}

fn member(user: u64) -> Member {
    serde_json::from_value(json!({
        "user": user_json(user), "roles": [], "joined_at": "2026-01-01T00:00:00.000Z", "deaf": false, "mute": false,
        "flags": 0, "guild_id": "1",
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test; run explicitly"]
async fn stress_member_waves_and_admin_dashboards() {
    let users: u64 = std::env::var("STRESS_USERS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let mut h = Harness::start().await;
    let dir = std::env::temp_dir().join(format!("gaia-stress-{}", std::process::id()));
    let db = crate::db::create_database(dir.join("gaia.sqlite").to_str().unwrap()).unwrap();
    h.repo = Arc::new(GaiaRepository::new(db));
    let bot_perms = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::MANAGE_ROLES;
    h.seed(
        GuildSeed::new(1, "Guild")
            .role(20, "Gaia", 10, bot_perms)
            .role(AUTOROLE, "Member", 2, Permissions::empty())
            .role(40, "Admin", 8, Permissions::ADMINISTRATOR)
            .channel(70, ChannelType::Text, None)
            .channel(MEMBER_LOG, ChannelType::Text, None)
            .bot(&[20])
            .member(ADMIN, &[40]),
    );
    let set = |key: &str, value: Sql| h.repo.update_setting("1", key, value).unwrap();
    set("log_mode", Sql::Text("server".into()));
    set("log_channel_id", Sql::Text("70".into()));
    set("member_log_channel_id", Sql::Text(MEMBER_LOG.to_string()));
    set("autorole_enabled", Sql::Integer(1));
    h.repo.set_autorole("1", "user_role_id", Some(&AUTOROLE.to_string())).unwrap();

    // Joining members are not seeded in the fake, so accept their role grants the way Discord would.
    h.on("PUT", &format!("/guilds/1/members/*/roles/{AUTOROLE}"), 204, serde_json::Value::Null);
    let bot: Arc<GaiaBot> = Arc::new(h.bot());
    let cell = OnceLock::new();
    let _ = cell.set(bot.clone());
    let handler = Arc::new(Handler { bot: Arc::new(cell) });
    let ctx = h.ctx();
    println!("STRESS start: {users} members, baseline memory {}", peak_rss_mib());

    let joins = (0..users)
        .map(|i| {
            let (handler, ctx) = (handler.clone(), ctx.clone());
            async move { handler.guild_member_addition(ctx, member(FIRST_USER + i)).await }
        })
        .collect();
    burst("member joins (AutoRole + member log)", joins).await;

    let admins = (0..users)
        .map(|_| {
            let (bot, ctx) = (bot.clone(), ctx.clone());
            let ix = Interaction::Command(h.command(1, ADMIN, "admin", json!([])));
            async move { bot.on_interaction(&ctx, &ix).await }
        })
        .collect();
    burst("/admin dashboards", admins).await;

    let leaves = (0..users)
        .map(|i| {
            let (handler, ctx) = (handler.clone(), ctx.clone());
            async move { handler.guild_member_removal(ctx, GuildId::new(1), User::from(user_json_struct(FIRST_USER + i)), None).await }
        })
        .collect();
    burst("member leaves (member log)", leaves).await;

    let drain = Instant::now();
    bot.audit.idle().await;
    let roles = h.calls_to("PUT", &format!("/guilds/1/members/*/roles/{AUTOROLE}")).len();
    let logged = h.calls_to("POST", &format!("/channels/{MEMBER_LOG}/messages")).len();
    let dashboards = h.replies().len();
    println!(
        "STRESS result: {roles} AutoRoles given, {logged} member log posts, {dashboards} dashboards answered, log queue drained {:.2}s after the last event, {} Discord calls total, peak memory {}",
        drain.elapsed().as_secs_f64(),
        h.calls().len(),
        peak_rss_mib()
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(roles as u64, users, "every member got the AutoRole");
    assert_eq!(logged as u64, 2 * users, "every join and leave was logged");
    assert_eq!(dashboards as u64, users, "every /admin was answered");
}

fn user_json_struct(id: u64) -> serenity::all::CurrentUser {
    serde_json::from_value(user_json(id)).unwrap()
}
