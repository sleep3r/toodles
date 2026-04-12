use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::transcription;

/// Draft rendering verbosity for streaming UX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftMode {
    /// Minimal waiting state and concise draft text.
    Compact,
    /// Rich waiting state with a progress log from tool/plan events.
    Verbose,
}

/// Bot configuration loaded from environment variables and optional TOML file.
#[derive(Debug, Clone)]
pub struct Config {
    /// Telegram bot token from @BotFather.
    pub telegram_bot_token: String,

    /// Comma-separated list of allowed Telegram user IDs.
    /// If empty, all users are allowed.
    pub allowed_user_ids: Vec<u64>,

    /// Full shell command that starts gemini-cli in ACP mode.
    pub gemini_cli_command: String,

    /// Optional working directory for gemini-cli sessions.
    pub gemini_working_dir: Option<String>,

    /// Whether to set ACP session mode to yolo.
    pub gemini_yolo: bool,

    /// Optional OpenAI API key for Whisper voice transcription.
    pub openai_api_key: Option<String>,

    /// Whether to use local Parakeet transcription instead of OpenAI Whisper.
    pub use_local_transcription: bool,

    /// Directory where models are stored.
    pub models_dir: PathBuf,

    /// System prompt prepended to the first query in each session.
    pub system_prompt: Option<String>,

    /// Draft rendering mode for in-flight responses.
    pub draft_mode: DraftMode,

    /// Rename forum topic title every N user messages in that thread.
    /// Set to 0 to disable automatic renaming.
    pub thread_rename_every: usize,

    /// Number of idle prewarmed ACP sessions to keep ready.
    /// Set to 0 to disable hot session pool.
    pub warm_session_pool_size: usize,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FileGeminiConfig {
    cmd: Option<String>,
    working_dir: Option<String>,
    cwd: Option<String>,
    yolo: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    bot_token: Option<String>,
    telegram_bot_token: Option<String>,
    allowed_user_ids: Option<Vec<u64>>,
    gemini_cli_path: Option<String>,
    gemini_cli_command: Option<String>,
    gemini_working_dir: Option<String>,
    gemini_yolo: Option<bool>,
    openai_api_key: Option<String>,
    use_local_transcription: Option<bool>,
    models_dir: Option<PathBuf>,
    system_prompt: Option<String>,
    draft_mode: Option<String>,
    thread_rename_every: Option<usize>,
    warm_session_pool_size: Option<usize>,
    gemini: Option<FileGeminiConfig>,
}

impl Config {
    /// Load configuration from environment variables and optional config file.
    pub fn from_env() -> Result<Self> {
        let file_config = load_file_config()?;

        let telegram_bot_token = env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .or_else(|| {
                file_config.as_ref().and_then(|cfg| {
                    cfg.telegram_bot_token
                        .clone()
                        .or_else(|| cfg.bot_token.clone())
                })
            })
            .context("TELEGRAM_BOT_TOKEN must be set")?;

        let allowed_user_ids = env::var("ALLOWED_USER_IDS")
            .ok()
            .map(|raw| parse_allowed_user_ids_csv(&raw))
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.allowed_user_ids.clone())
            })
            .unwrap_or_default();

        let gemini_cli_path = env::var("GEMINI_CLI_PATH")
            .ok()
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.gemini_cli_path.clone())
            })
            .unwrap_or_else(|| "gemini".to_string());

        let gemini_cli_command = env::var("GEMINI_CLI_COMMAND")
            .ok()
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.gemini_cli_command.clone())
            })
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.gemini.as_ref())
                    .and_then(|gemini| gemini.cmd.clone())
            })
            .unwrap_or_else(|| format!("{} --acp", gemini_cli_path));

        let gemini_working_dir = env::var("GEMINI_WORKING_DIR")
            .ok()
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.gemini_working_dir.clone())
            })
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.gemini.as_ref())
                    .and_then(|gemini| gemini.working_dir.clone().or(gemini.cwd.clone()))
            });

        let gemini_yolo = env_bool("GEMINI_YOLO")
            .or_else(|| file_config.as_ref().and_then(|cfg| cfg.gemini_yolo))
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.gemini.as_ref())
                    .and_then(|gemini| gemini.yolo)
            })
            .unwrap_or(true);

        let openai_api_key = env::var("OPENAI_API_KEY").ok().or_else(|| {
            file_config
                .as_ref()
                .and_then(|cfg| cfg.openai_api_key.clone())
        });

        let use_local_transcription = env_bool("USE_LOCAL_TRANSCRIPTION")
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.use_local_transcription)
            })
            .unwrap_or(false);

        let models_dir = env::var("MODELS_DIR")
            .ok()
            .map(|p| expand_tilde(Path::new(&p)))
            .or_else(|| file_config.as_ref().and_then(|cfg| cfg.models_dir.clone()))
            .unwrap_or_else(transcription::default_models_dir);

        let system_prompt = env::var("SYSTEM_PROMPT")
            .ok()
            .or_else(|| file_config.as_ref().and_then(|cfg| cfg.system_prompt.clone()))
            .or_else(|| {
                Some(
                    "You are Toodles - a versatile, friendly AI assistant in a Telegram chat. \
                     You help with any task: answering questions, writing code, brainstorming ideas, \
                     translating text, explaining concepts, and more. \
                     Keep answers concise and useful. Respond in the user's language. \
                     To send a file to the user, output a line containing only: ATTACH_FILE:/absolute/path/to/file"
                        .to_string(),
                )
            });

        let draft_mode = env::var("DRAFT_MODE")
            .ok()
            .as_deref()
            .and_then(parse_draft_mode)
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.draft_mode.as_deref())
                    .and_then(parse_draft_mode)
            })
            .unwrap_or(DraftMode::Verbose);

        let thread_rename_every = env::var("THREAD_RENAME_EVERY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .or_else(|| file_config.as_ref().and_then(|cfg| cfg.thread_rename_every))
            .unwrap_or(4);

        let warm_session_pool_size = env::var("WARM_SESSION_POOL_SIZE")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.warm_session_pool_size)
            })
            .unwrap_or(1);

        Ok(Self {
            telegram_bot_token,
            allowed_user_ids,
            gemini_cli_command,
            gemini_working_dir,
            gemini_yolo,
            openai_api_key,
            use_local_transcription,
            models_dir,
            system_prompt,
            draft_mode,
            thread_rename_every,
            warm_session_pool_size,
        })
    }

    /// Returns `true` if the given user ID is permitted to use the bot.
    /// An empty allowlist means all users are permitted.
    pub fn is_user_allowed(&self, user_id: u64) -> bool {
        self.allowed_user_ids.is_empty() || self.allowed_user_ids.contains(&user_id)
    }
}

fn parse_allowed_user_ids_csv(raw: &str) -> Vec<u64> {
    raw.split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .collect()
}

fn parse_draft_mode(raw: &str) -> Option<DraftMode> {
    let normalized = raw.trim();
    if normalized.eq_ignore_ascii_case("compact") {
        Some(DraftMode::Compact)
    } else if normalized.eq_ignore_ascii_case("verbose") {
        Some(DraftMode::Verbose)
    } else {
        None
    }
}

fn env_bool(name: &str) -> Option<bool> {
    env::var(name).ok().and_then(|value| {
        let value = value.trim();
        if value.eq_ignore_ascii_case("1")
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
        {
            Some(true)
        } else if value.eq_ignore_ascii_case("0")
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("no")
            || value.eq_ignore_ascii_case("off")
        {
            Some(false)
        } else {
            None
        }
    })
}

fn load_file_config() -> Result<Option<FileConfig>> {
    let config_path = env::var("TOODLES_CONFIG")
        .ok()
        .map(|p| expand_tilde(Path::new(&p)))
        .or_else(default_config_path);

    let Some(path) = config_path else {
        return Ok(None);
    };

    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let parsed: FileConfig =
        toml::from_str(&content).with_context(|| format!("Invalid TOML: {}", path.display()))?;
    Ok(Some(parsed))
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("toodles").join("config.toml"))
}

fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(suffix) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(suffix);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(allowed: Vec<u64>) -> Config {
        Config {
            telegram_bot_token: "test_token".to_string(),
            allowed_user_ids: allowed,
            gemini_cli_command: "gemini --acp".to_string(),
            gemini_working_dir: None,
            gemini_yolo: true,
            openai_api_key: None,
            use_local_transcription: false,
            models_dir: PathBuf::from("/tmp/models"),
            system_prompt: None,
            draft_mode: DraftMode::Verbose,
            thread_rename_every: 4,
            warm_session_pool_size: 1,
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

    #[test]
    fn parse_allowed_ids_csv() {
        assert_eq!(parse_allowed_user_ids_csv("1,2,3"), vec![1, 2, 3]);
        assert_eq!(parse_allowed_user_ids_csv(" 10 , bad, 30"), vec![10, 30]);
        assert_eq!(parse_allowed_user_ids_csv(""), Vec::<u64>::new());
    }

    #[test]
    fn env_bool_parser() {
        std::env::set_var("TOODLES_BOOL_TEST", "true");
        assert_eq!(env_bool("TOODLES_BOOL_TEST"), Some(true));

        std::env::set_var("TOODLES_BOOL_TEST", "0");
        assert_eq!(env_bool("TOODLES_BOOL_TEST"), Some(false));

        std::env::set_var("TOODLES_BOOL_TEST", "maybe");
        assert_eq!(env_bool("TOODLES_BOOL_TEST"), None);

        std::env::remove_var("TOODLES_BOOL_TEST");
        assert_eq!(env_bool("TOODLES_BOOL_TEST"), None);
    }

    #[test]
    fn parse_draft_mode_values() {
        assert_eq!(parse_draft_mode("compact"), Some(DraftMode::Compact));
        assert_eq!(parse_draft_mode("VERBOSE"), Some(DraftMode::Verbose));
        assert_eq!(parse_draft_mode("unknown"), None);
    }
}
