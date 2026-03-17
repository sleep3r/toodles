use anyhow::{Context, Result};
use dashmap::DashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::config::Config;

/// Session key: `(chat_id, thread_id)`.
/// `thread_id` is `Some` when the message belongs to a Telegram forum topic.
pub type SessionKey = (i64, Option<i32>);

// ──────────────────────────────────────────────────────────────────────────────
// Session
// ──────────────────────────────────────────────────────────────────────────────

/// A gemini-cli session that uses headless mode (`-p`) with `--resume` for
/// multi-turn context.
///
/// Each query spawns a fresh gemini-cli process in non-interactive mode.
/// gemini-cli internally saves session state, and `--resume latest` restores
/// the conversation history for continuity.
pub struct Session {
    gemini_cli_path: String,
    working_dir: Option<String>,
    /// First query doesn't use `--resume`; subsequent ones do.
    has_history: bool,
    /// Whether to use --yolo (auto-approve all actions).
    yolo: bool,
    /// System prompt prepended to the first query.
    system_prompt: Option<String>,
    /// Whether the session has been warmed up (gemini-cli initialised).
    is_warm: bool,
}

impl Session {
    pub fn new(
        gemini_cli_path: &str,
        working_dir: Option<&str>,
        yolo: bool,
        system_prompt: Option<String>,
    ) -> Self {
        info!("Created new headless session");
        Self {
            gemini_cli_path: gemini_cli_path.to_string(),
            working_dir: working_dir.map(|s| s.to_string()),
            has_history: false,
            yolo,
            system_prompt,
            is_warm: false,
        }
    }


    /// Warm up the session by sending a lightweight probe query to gemini-cli.
    ///
    /// This forces gemini-cli to perform its slow initialisation (auth, model
    /// loading, etc.) so that subsequent `query()` calls with `--resume latest`
    /// are fast.
    pub async fn warm_up(&mut self) -> Result<()> {
        if self.is_warm {
            return Ok(());
        }

        info!("Warming up gemini-cli session…");

        // Include the system prompt in the warm-up so it's part of the
        // session history that `--resume latest` restores.
        let warmup_prompt = if let Some(ref sp) = self.system_prompt {
            format!(
                "[System instruction]: {}\n\nRespond with just the word 'ready'.",
                sp
            )
        } else {
            "Respond with just the word 'ready'.".to_string()
        };

        let mut cmd = Command::new(&self.gemini_cli_path);
        cmd.arg("-p").arg(&warmup_prompt)
            .arg("-o").arg("text")
            .arg("--sandbox=false")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if self.yolo {
            cmd.arg("--yolo");
        }

        if let Some(ref dir) = self.working_dir {
            cmd.current_dir(dir);
        }

        let child = cmd.spawn()
            .with_context(|| format!("Failed to spawn '{}' for warm-up", self.gemini_cli_path))?;

        let output = child.wait_with_output().await
            .context("Failed to read gemini-cli warm-up output")?;

        if !output.status.success() {
            let stderr_hint = String::from_utf8_lossy(&output.stderr);
            error!("gemini-cli warm-up exited with {}: {}", output.status, stderr_hint);
        }

        self.has_history = true;
        self.is_warm = true;
        info!("Gemini-cli session warmed up");
        Ok(())
    }

    /// Send a prompt and stream the response line-by-line.
    ///
    /// Uses `gemini -p "prompt" -o text --sandbox=false [--yolo] [--resume latest]`.
    /// Each line is sent through `tx` as soon as it arrives from gemini-cli.
    pub async fn query(
        &mut self,
        prompt: &str,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        debug!("Query: {}", &prompt[..prompt.len().min(80)]);

        // Prepend system prompt on the first query of a new session.
        let full_prompt = if !self.has_history {
            if let Some(ref sp) = self.system_prompt {
                format!("[System instruction]: {}\n\n{}", sp, prompt)
            } else {
                prompt.to_string()
            }
        } else {
            prompt.to_string()
        };

        let mut cmd = Command::new(&self.gemini_cli_path);
        cmd.arg("-p").arg(&full_prompt)
            .arg("-o").arg("text")
            .arg("--sandbox=false")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        if self.yolo {
            cmd.arg("--yolo");
        }

        if self.has_history {
            cmd.arg("--resume").arg("latest");
        }

        if let Some(ref dir) = self.working_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn()
            .with_context(|| format!("Failed to spawn '{}'", self.gemini_cli_path))?;

        // Stream stdout line-by-line as gemini-cli produces output.
        let stdout = child.stdout.take()
            .context("Failed to capture gemini-cli stdout")?;

        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            let n = reader.read_line(&mut line_buf).await
                .context("Error reading gemini-cli stdout")?;
            if n == 0 {
                break; // EOF
            }
            let stripped = strip_ansi(line_buf.trim_end_matches('\n'));
            if !stripped.is_empty() {
                let _ = tx.send(stripped).await;
            }
        }

        // Wait for the process to exit (stdout is already drained).
        let status = child.wait().await.context("Failed to wait for gemini-cli")?;
        if !status.success() {
            error!("gemini-cli exited with {}", status);
        }

        self.has_history = true;
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

// ──────────────────────────────────────────────────────────────────────────────
// SessionManager
// ──────────────────────────────────────────────────────────────────────────────

/// Thread-safe registry of active gemini-cli sessions.
pub struct SessionManager {
    sessions: DashMap<SessionKey, Arc<Mutex<Session>>>,
    config: Arc<Config>,
}

impl SessionManager {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            sessions: DashMap::new(),
            config,
        }
    }

    /// Get an existing session or create a new one.
    ///
    /// Returns `(session, is_new)` where `is_new` is `true` when a fresh
    /// session was just created (caller should warm it up).
    pub async fn get_or_create(&self, key: SessionKey) -> Result<(Arc<Mutex<Session>>, bool)> {
        if let Some(entry) = self.sessions.get(&key) {
            return Ok((entry.clone(), false));
        }

        info!("Creating new gemini-cli session for {:?}", key);
        let session = Session::new(
            &self.config.gemini_cli_path,
            self.config.gemini_working_dir.as_deref(),
            true, // yolo mode
            self.config.system_prompt.clone(),
        );
        let session = Arc::new(Mutex::new(session));
        self.sessions.insert(key, session.clone());
        Ok((session, true))
    }

    /// Reset a session – gemini-cli manages its own session files,
    /// so we just drop our `Session` and start fresh.
    pub async fn reset(&self, key: &SessionKey) {
        self.sessions.remove(key);
        info!("Session {:?} reset", key);
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_ansi ───────────────────────────────────────────────────────

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strip_ansi_only_escape_sequence() {
        assert_eq!(strip_ansi("\x1b[0m"), "");
    }

    #[test]
    fn strip_ansi_nested_sequences() {
        assert_eq!(
            strip_ansi("\x1b[1m\x1b[31mbold red\x1b[0m normal"),
            "bold red normal"
        );
    }

    #[test]
    fn strip_ansi_preserves_unicode() {
        assert_eq!(strip_ansi("привет \x1b[32mмир\x1b[0m 🌍"), "привет мир 🌍");
    }

    #[test]
    fn strip_ansi_256_color() {
        // 256-color: \x1b[38;5;196m
        assert_eq!(strip_ansi("\x1b[38;5;196mred\x1b[0m"), "red");
    }

    #[test]
    fn strip_ansi_cursor_movement() {
        assert_eq!(strip_ansi("\x1b[2Ahello"), "hello");
    }

    // ── SessionManager ──────────────────────────────────────────────────

    fn test_config() -> Arc<Config> {
        Arc::new(Config {
            telegram_bot_token: "test".to_string(),
            allowed_user_ids: vec![],
            gemini_cli_path: "echo".to_string(), // Use echo as a dummy
            gemini_working_dir: None,
            openai_api_key: None,
            use_local_transcription: false,
            models_dir: std::path::PathBuf::from("/tmp"),
            system_prompt: Some("test prompt".to_string()),
        })
    }

    #[tokio::test]
    async fn session_manager_create_returns_new() {
        let mgr = SessionManager::new(test_config());
        let key: SessionKey = (100, None);
        let (_session, is_new) = mgr.get_or_create(key).await.unwrap();
        assert!(is_new);
    }

    #[tokio::test]
    async fn session_manager_get_existing_returns_not_new() {
        let mgr = SessionManager::new(test_config());
        let key: SessionKey = (100, None);
        mgr.get_or_create(key).await.unwrap();
        let (_session, is_new) = mgr.get_or_create(key).await.unwrap();
        assert!(!is_new);
    }

    #[tokio::test]
    async fn session_manager_different_keys_are_separate() {
        let mgr = SessionManager::new(test_config());
        let key_a: SessionKey = (100, None);
        let key_b: SessionKey = (200, None);

        let (_, is_new_a) = mgr.get_or_create(key_a).await.unwrap();
        let (_, is_new_b) = mgr.get_or_create(key_b).await.unwrap();

        assert!(is_new_a);
        assert!(is_new_b);
        assert_eq!(mgr.session_count(), 2);
    }

    #[tokio::test]
    async fn session_manager_thread_id_isolates_sessions() {
        let mgr = SessionManager::new(test_config());
        let key_no_thread: SessionKey = (100, None);
        let key_with_thread: SessionKey = (100, Some(42));

        mgr.get_or_create(key_no_thread).await.unwrap();
        mgr.get_or_create(key_with_thread).await.unwrap();

        assert_eq!(mgr.session_count(), 2);
    }

    #[tokio::test]
    async fn session_manager_reset_removes_session() {
        let mgr = SessionManager::new(test_config());
        let key: SessionKey = (100, None);
        mgr.get_or_create(key).await.unwrap();
        assert_eq!(mgr.session_count(), 1);

        mgr.reset(&key).await;
        assert_eq!(mgr.session_count(), 0);

        // Next get_or_create should be "new" again.
        let (_, is_new) = mgr.get_or_create(key).await.unwrap();
        assert!(is_new);
    }

    #[tokio::test]
    async fn session_manager_session_count() {
        let mgr = SessionManager::new(test_config());
        assert_eq!(mgr.session_count(), 0);

        mgr.get_or_create((1, None)).await.unwrap();
        assert_eq!(mgr.session_count(), 1);

        mgr.get_or_create((2, None)).await.unwrap();
        assert_eq!(mgr.session_count(), 2);

        mgr.get_or_create((1, None)).await.unwrap(); // existing
        assert_eq!(mgr.session_count(), 2);
    }
}
