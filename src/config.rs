use std::env;
use std::sync::OnceLock;

/// Used when TIMEZONE is unset: keeps existing deployments on their original schedule.
pub const DEFAULT_TIMEZONE: chrono_tz::Tz = chrono_tz::Europe::Ljubljana;
pub const DEFAULT_BACKUP_HOUR: u32 = 3;

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().map(|value| value.trim().to_string())
}

#[derive(Clone)]
pub struct Config {
    pub token: String,
    pub database_path: String,
    pub application_id: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();
        Self {
            token: env::var("DISCORD_TOKEN").unwrap_or_default(),
            database_path: env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/gaia.sqlite".into()),
            application_id: ["APPLICATION_ID", "CLIENT_ID", "DISCORD_CLIENT_ID"]
                .iter()
                .find_map(|name| env::var(name).ok()),
        }
    }

    pub fn require(&self) -> anyhow::Result<()> {
        if let Some(tz) = optional_env("TIMEZONE").filter(|v| !v.is_empty()) {
            if tz.parse::<chrono_tz::Tz>().is_err() {
                anyhow::bail!("TIMEZONE '{tz}' is not a valid IANA timezone (for example Europe/Berlin or America/New_York).");
            }
        }
        if let Some(hour) = optional_env("BACKUP_HOUR").filter(|v| !v.is_empty()) {
            if !hour.parse::<u32>().is_ok_and(|h| h <= 23) {
                anyhow::bail!("BACKUP_HOUR must be a whole number from 0 to 23.");
            }
        }
        if self.token.trim().is_empty() {
            anyhow::bail!("DISCORD_TOKEN is required. See .env.example.");
        }
        if self.database_path.trim().is_empty() {
            anyhow::bail!("DATABASE_PATH cannot be empty. See .env.example.");
        }
        Ok(())
    }
}

/// Timezone for schedules, backup times and displayed dates (validated by `Config::require`).
pub fn timezone() -> chrono_tz::Tz {
    static TZ: OnceLock<chrono_tz::Tz> = OnceLock::new();
    *TZ.get_or_init(|| optional_env("TIMEZONE").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TIMEZONE))
}

/// Local hour (0-23) from which the daily automatic backup runs.
pub fn backup_hour() -> u32 {
    static HOUR: OnceLock<u32> = OnceLock::new();
    *HOUR.get_or_init(|| optional_env("BACKUP_HOUR").and_then(|v| v.parse().ok()).filter(|h| *h <= 23).unwrap_or(DEFAULT_BACKUP_HOUR))
}

/// The bot token prints only as whether it is set.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("token", &if self.token.is_empty() { "[unset]" } else { "[redacted]" })
            .field("database_path", &self.database_path)
            .field("application_id", &self.application_id)
            .finish()
    }
}
