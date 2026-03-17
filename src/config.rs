use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::transcription;

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

    /// Whether to use local Parakeet transcription instead of OpenAI Whisper.
    pub use_local_transcription: bool,

    /// Directory where models are stored.
    pub models_dir: PathBuf,

    /// System prompt prepended to the first query in each session.
    pub system_prompt: Option<String>,
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

        let use_local_transcription = env::var("USE_LOCAL_TRANSCRIPTION")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        let models_dir = env::var("MODELS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| transcription::default_models_dir());

        let system_prompt = env::var("SYSTEM_PROMPT").ok().or_else(|| {
            Some(
                "You are Toodles — a versatile, friendly AI assistant in a Telegram chat. \
                 You help with any task: answering questions, writing code, brainstorming ideas, \
                 translating text, explaining concepts, and more. \
                 Keep answers concise and useful. Respond in the user's language. \
                 To send a file to the user, output a line containing only: ATTACH_FILE:/absolute/path/to/file"
                    .to_string(),
            )
        });

        Ok(Self {
            telegram_bot_token,
            allowed_user_ids,
            gemini_cli_path,
            gemini_working_dir,
            openai_api_key,
            use_local_transcription,
            models_dir,
            system_prompt,
        })
    }

    /// Returns `true` if the given user ID is permitted to use the bot.
    /// An empty allowlist means all users are permitted.
    pub fn is_user_allowed(&self, user_id: u64) -> bool {
        self.allowed_user_ids.is_empty() || self.allowed_user_ids.contains(&user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(allowed: Vec<u64>) -> Config {
        Config {
            telegram_bot_token: "test_token".to_string(),
            allowed_user_ids: allowed,
            gemini_cli_path: "gemini".to_string(),
            gemini_working_dir: None,
            openai_api_key: None,
            use_local_transcription: false,
            models_dir: PathBuf::from("/tmp/models"),
            system_prompt: None,
        }
    }

    #[test]
    fn empty_allowlist_permits_everyone() {
        let config = test_config(vec![]);
        assert!(config.is_user_allowed(12345));
        assert!(config.is_user_allowed(99999));
        assert!(config.is_user_allowed(0));
    }

    #[test]
    fn allowlist_permits_listed_users() {
        let config = test_config(vec![100, 200, 300]);
        assert!(config.is_user_allowed(100));
        assert!(config.is_user_allowed(200));
        assert!(config.is_user_allowed(300));
    }

    #[test]
    fn allowlist_rejects_unlisted_users() {
        let config = test_config(vec![100, 200]);
        assert!(!config.is_user_allowed(999));
        assert!(!config.is_user_allowed(0));
        assert!(!config.is_user_allowed(101));
    }

    #[test]
    fn single_user_allowlist() {
        let config = test_config(vec![42]);
        assert!(config.is_user_allowed(42));
        assert!(!config.is_user_allowed(43));
    }
}
