use anyhow::{Context, Result};
use std::env;

/// Bot configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Telegram bot token from @BotFather.
    pub telegram_bot_token: String,

    /// Comma-separated list of allowed Telegram user IDs.
    /// If empty, all users are allowed.
    pub allowed_user_ids: Vec<u64>,

    /// Path to the gemini-cli binary (default: "gemini").
    pub gemini_cli_path: String,

    /// Optional working directory for gemini-cli sessions.
    pub gemini_working_dir: Option<String>,

    /// Optional OpenAI API key for Whisper voice transcription.
    pub openai_api_key: Option<String>,
}

impl Config {
    /// Load configuration from environment variables.
    pub fn from_env() -> Result<Self> {
        let telegram_bot_token = env::var("TELEGRAM_BOT_TOKEN")
            .context("TELEGRAM_BOT_TOKEN must be set")?;

        let allowed_user_ids = env::var("ALLOWED_USER_IDS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.trim().parse::<u64>().ok())
            .collect();

        let gemini_cli_path =
            env::var("GEMINI_CLI_PATH").unwrap_or_else(|_| "gemini".to_string());

        let gemini_working_dir = env::var("GEMINI_WORKING_DIR").ok();
        let openai_api_key = env::var("OPENAI_API_KEY").ok();

        Ok(Self {
            telegram_bot_token,
            allowed_user_ids,
            gemini_cli_path,
            gemini_working_dir,
            openai_api_key,
        })
    }

    /// Returns `true` if the given user ID is permitted to use the bot.
    /// An empty allowlist means all users are permitted.
    pub fn is_user_allowed(&self, user_id: u64) -> bool {
        self.allowed_user_ids.is_empty() || self.allowed_user_ids.contains(&user_id)
    }
}
