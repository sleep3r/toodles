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
        }
    }



    /// Send a prompt and stream the response line-by-line.
    ///
    /// Uses `gemini -p "prompt" -o text --sandbox=false [--yolo] [--resume latest]`.
    pub async fn query(
        &mut self,
        prompt: &str,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        self.query_with_files(prompt, &[], tx).await
    }

    /// Send a prompt with optional file attachments and stream the response line-by-line.
    ///
    /// Files are passed as positional arguments to gemini-cli so it uploads them
    /// via the Gemini API as multimodal parts.
    pub async fn query_with_files(
        &mut self,
        prompt: &str,
        file_paths: &[String],
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        debug!("Query: {}", &prompt[..prompt.len().min(80)]);

        // Prepend system prompt on the first query of a new session.
        let mut full_prompt = if !self.has_history {
            if let Some(ref sp) = self.system_prompt {
                format!("[System instruction]: {}\n\n{}", sp, prompt)
            } else {
                prompt.to_string()
            }
        } else {
            prompt.to_string()
        };

        // Use native @{path} syntax for multimodal file injection.
        if !file_paths.is_empty() {
            full_prompt.push_str("\n\n");
            for path in file_paths {
                full_prompt.push_str(&format!("@{{{}}}\n", path));
            }
        }

        let mut cmd = Command::new(&self.gemini_cli_path);
        cmd.arg("-p").arg(&full_prompt)
            .arg("-o").arg("text")
            .arg("--sandbox=false")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if self.yolo {
            cmd.arg("--yolo");
        }

        if self.has_history {
            cmd.arg("--resume").arg("latest");
        }

        // Allow gemini-cli to read directories containing attached files.
        if !file_paths.is_empty() {
            let mut dirs: Vec<String> = file_paths.iter()
                .filter_map(|p| std::path::Path::new(p).parent().map(|d| d.to_string_lossy().to_string()))
                .collect();
            dirs.sort();
            dirs.dedup();
            cmd.arg("--include-directories").arg(dirs.join(","));
        }

        if let Some(ref dir) = self.working_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn()
            .with_context(|| format!("Failed to spawn '{}'", self.gemini_cli_path))?;

        // Stream stdout line-by-line as gemini-cli produces output.
        let stdout = child.stdout.take()
            .context("Failed to capture gemini-cli stdout")?;

        // Spawn a task to log stderr in the background.
        let stderr = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            if let Some(err_stream) = stderr {
                let mut err_reader = tokio::io::BufReader::new(err_stream);
                let mut err_line = String::new();
                loop {
                    err_line.clear();
                    match err_reader.read_line(&mut err_line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = err_line.trim();
                            if !trimmed.is_empty() {
                                debug!("gemini-cli stderr: {}", trimmed);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let stripped = strip_ansi(line_buf.trim_end_matches('\n'));
                    if !stripped.is_empty() {
                        let _ = tx.send(stripped).await;
                    }
                }
                Err(e) => {
                    error!("Error reading gemini-cli stdout: {e}");
                    break;
                }
            }
        }

        // Wait for the process to exit with a timeout to prevent deadlocks.
        match tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await {
            Ok(Ok(status)) if !status.success() => {
                error!("gemini-cli exited with {}", status);
            }
            Ok(Err(e)) => {
                error!("Failed to wait for gemini-cli: {e}");
            }
            Err(_) => {
                error!("gemini-cli timed out after 120s! Killing process.");
                let _ = child.kill().await;
            }
            _ => {}
        }

        // Let stderr drain.
        stderr_task.await.ok();

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
        use dashmap::mapref::entry::Entry;
        match self.sessions.entry(key) {
            Entry::Occupied(e) => Ok((e.get().clone(), false)),
            Entry::Vacant(e) => {
                info!("Creating new gemini-cli session for {:?}", key);
                let session = Session::new(
                    &self.config.gemini_cli_path,
                    self.config.gemini_working_dir.as_deref(),
                    true, // yolo mode
                    self.config.system_prompt.clone(),
                );
                let session = Arc::new(Mutex::new(session));
                e.insert(session.clone());
                Ok((session, true))
            }
        }
    }

    /// Reset a session – drop our `Session` and start fresh.
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
