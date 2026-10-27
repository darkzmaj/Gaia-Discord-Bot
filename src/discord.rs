use serenity::cache::Cache;
use serenity::http::Http;
use std::sync::Arc;

/// Shared Discord handle for services and background tasks (the Rust analogue of discord.js `Client`).
#[derive(Clone)]
pub struct Discord {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
}

/// Gaia's default embed colour (purple). Status colours (success/warning/danger) stay semantic.
pub const GAIA_PURPLE: u32 = 0x9b59b6;
