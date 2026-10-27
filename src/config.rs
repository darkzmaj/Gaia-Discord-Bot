use std::env;

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().map(|value| value.trim().to_string())
}

#[derive(Clone, Debug, Default)]
pub struct AuditLogging {
    pub webhook_url: Option<String>,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct MemberLogging {
    pub webhook_url: Option<String>,
    pub channel_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub token: String,
    pub database_path: String,
    pub application_id: Option<String>,
    pub audit_logging: AuditLogging,
    pub member_logging: MemberLogging,
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
            audit_logging: AuditLogging {
                webhook_url: optional_env("LOG_WEBHOOK_URL"),
                guild_id: optional_env("LOG_GUILD"),
                channel_id: optional_env("LOG_GUILD_CHANNEL"),
            },
            member_logging: MemberLogging {
                webhook_url: optional_env("MEMBER_LOG_WEBHOOK_URL"),
                channel_id: optional_env("MEMBER_LOG_CHANNEL"),
            },
        }
    }

    pub fn require(&self) -> anyhow::Result<()> {
        if self.token.trim().is_empty() {
            anyhow::bail!("DISCORD_TOKEN is required. See .env.example.");
        }
        if self.database_path.trim().is_empty() {
            anyhow::bail!("DATABASE_PATH cannot be empty. See .env.example.");
        }
        Ok(())
    }
}
