use anyhow::{Context, Result};
use dashmap::DashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Duration};
use tracing::{error, info};

use crate::config::Config;

/// Session key: `(chat_id, thread_id)`.
/// `thread_id` is `Some` when the message belongs to a Telegram forum topic.
pub type SessionKey = (i64, Option<i32>);

// ──────────────────────────────────────────────────────────────────────────────
// Session
// ──────────────────────────────────────────────────────────────────────────────

/// A live gemini-cli subprocess with piped I/O.
pub struct Session {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    /// Whether the startup banner has already been drained.
    initialized: bool,
}

impl Session {
    /// Spawn a new gemini-cli process.
    pub async fn new(gemini_cli_path: &str, working_dir: Option<&str>) -> Result<Self> {
        let mut cmd = Command::new(gemini_cli_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn '{gemini_cli_path}'"))?;

        let stdin = child.stdin.take().context("Failed to take stdin handle")?;
        let stdout = child.stdout.take().context("Failed to take stdout handle")?;
        let reader = BufReader::new(stdout);

        Ok(Self {
            child,
            stdin,
            reader,
            initialized: false,
        })
    }

    /// Drain the startup banner / initial prompt that gemini-cli prints on launch.
    async fn ensure_initialized(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }

        let mut line = String::new();
        loop {
            // Wait up to 1.5 s per line; stop as soon as there is a pause.
            match timeout(
                Duration::from_millis(1500),
                self.reader.read_line(&mut line),
            )
            .await
            {
                Ok(Ok(0)) | Err(_) => break, // EOF or idle → startup complete
                Ok(Ok(_)) => line.clear(),
                Ok(Err(e)) => return Err(e.into()),
            }
        }

        self.initialized = true;
        Ok(())
    }

    /// Send `prompt` to gemini-cli and stream response lines via `tx`.
    ///
    /// The method holds the caller's `&mut self` borrow for the entire query
    /// duration, so queries are automatically serialised per session.
    pub async fn query_streaming(
        &mut self,
        prompt: &str,
        tx: mpsc::Sender<String>,
    ) -> Result<()> {
        self.ensure_initialized().await?;

        self.stdin.write_all(prompt.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        let mut line = String::new();
        let mut got_first_line = false;

        loop {
            // Allow up to 30 s for the first line; 800 ms idle afterward.
            let read_timeout = if got_first_line {
                Duration::from_millis(800)
            } else {
                Duration::from_secs(30)
            };

            match timeout(read_timeout, self.reader.read_line(&mut line)).await {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(_)) => {
                    got_first_line = true;
                    let stripped = strip_ansi(line.trim_end());
                    if !stripped.is_empty() && !looks_like_prompt(&stripped) {
                        let _ = tx.send(stripped).await;
                    }
                    line.clear();
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => break, // idle timeout → response is complete
            }
        }

        Ok(())
    }

    /// Kill the underlying child process.
    pub async fn kill(&mut self) {
        if let Err(e) = self.child.kill().await {
            error!("Failed to kill gemini-cli process: {e}");
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Return `true` if the line looks like a gemini-cli input prompt (e.g. `> `).
fn looks_like_prompt(s: &str) -> bool {
    let t = s.trim();
    // Accept bare `>`, `❯`, or any line whose non-whitespace tail ends with
    // `>` or `❯` (handles "gemini> ", "> ", "❯ ", etc.)
    t == ">" || t == "❯" || t.ends_with('>') || t.ends_with('❯')
}

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip the escape sequence up to (and including) the terminating
            // ASCII letter (e.g. 'm' in `ESC[31m`).
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

    /// Get an existing session or create a new one for `key`.
    pub async fn get_or_create(&self, key: SessionKey) -> Result<Arc<Mutex<Session>>> {
        if let Some(entry) = self.sessions.get(&key) {
            return Ok(entry.clone());
        }

        info!("Creating new gemini-cli session for {:?}", key);
        let session = Session::new(
            &self.config.gemini_cli_path,
            self.config.gemini_working_dir.as_deref(),
        )
        .await?;
        let session = Arc::new(Mutex::new(session));
        self.sessions.insert(key, session.clone());
        Ok(session)
    }

    /// Kill and remove the session for `key` (if it exists).
    pub async fn reset(&self, key: &SessionKey) {
        if let Some((_, session)) = self.sessions.remove(key) {
            let mut s = session.lock().await;
            s.kill().await;
        }
    }

    /// Return the number of currently active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[1;32mgreen bold\x1b[0m text"), "green bold text");
    }

    #[test]
    fn looks_like_prompt_detects_prompts() {
        assert!(looks_like_prompt("> "));
        assert!(looks_like_prompt(">"));
        assert!(looks_like_prompt("gemini> "));
        assert!(!looks_like_prompt("Hello, world!"));
        assert!(!looks_like_prompt(""));
    }
}
