use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use dashmap::DashMap;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::acp::{AcpConnection, AcpEvent, ContentBlock};
use crate::config::Config;

/// Session key: `(chat_id, thread_id)`.
/// `thread_id` is `Some` when the message belongs to a Telegram forum topic.
pub type SessionKey = (i64, Option<i32>);

/// Internal prefix for structured stream events sent over the text channel.
///
/// `stream_response_with_drafts` consumes these events to render nicer draft UX
/// (tool/progress/waiting) without polluting the final assistant text.
pub const STREAM_EVENT_PREFIX: &str = "__TOODLES_EVT__:";

// ──────────────────────────────────────────────────────────────────────────────
// ACP Session
// ──────────────────────────────────────────────────────────────────────────────

/// A gemini-cli session that uses ACP mode for persistent, low-latency
/// communication.
///
/// The configured ACP agent process is spawned once and kept alive for
/// the lifetime of the session. Prompts are sent via JSON-RPC over stdin/stdout.
pub struct Session {
    /// The ACP connection to the agent process.
    conn: Arc<AcpConnection>,
    /// The ACP session ID returned by `session/new`.
    session_id: String,
    /// Receiver for ACP events (text chunks, tool calls, etc.).
    event_rx: mpsc::UnboundedReceiver<AcpEvent>,
    /// Whether to use --yolo (auto-approve all actions).
    #[allow(dead_code)]
    yolo: bool,
    /// System prompt prepended to the first query.
    system_prompt: Option<String>,
    /// Whether we've sent the first prompt (system prompt is prepended on first).
    has_history: bool,
}

impl Session {
    /// Create a new ACP session by spawning the configured agent command.
    pub async fn new(
        agent_cmd: &str,
        working_dir: Option<&str>,
        yolo: bool,
        system_prompt: Option<String>,
    ) -> Result<Self> {
        let dir = working_dir.map(Path::new);
        let (conn, event_rx) = AcpConnection::spawn(agent_cmd, dir).await?;

        // Initialize the ACP connection.
        let init_result = conn.initialize().await?;
        info!("ACP initialized: {:?}", init_result.get("agentInfo"));

        // Create a new session.
        let cwd = dir
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        let session_id = conn.new_session(&cwd).await?;

        // Auto-set yolo mode if requested (auto-approve all tool calls).
        if yolo {
            match conn.set_session_mode(&session_id, "yolo").await {
                Ok(_) => info!("ACP session mode set to yolo"),
                Err(e) => {
                    let text = e.to_string();
                    if text.contains("Method not found") {
                        debug!("ACP agent does not support session/set-mode; continuing");
                    } else {
                        warn!("Failed to set yolo mode (will use default): {e}");
                    }
                }
            }
        }

        Ok(Self {
            conn: Arc::new(conn),
            session_id,
            event_rx,
            yolo,
            system_prompt,
            has_history: false,
        })
    }

    /// Send a prompt and stream the response via the provided channel.
    ///
    /// Text chunks from ACP events are forwarded to `tx`, matching the
    /// interface expected by `stream_response_with_drafts`.
    ///
    /// If the `cancel` token is triggered, the ACP cancel notification is sent.
    pub async fn query(
        &mut self,
        prompt: &str,
        tx: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Result<()> {
        self.query_with_files(prompt, &[], tx, cancel).await
    }

    /// Send a prompt with optional file attachments and stream the response.
    ///
    /// Files are referenced using gemini-cli's `@{path}` syntax in the prompt
    /// text, which gemini-cli processes in ACP mode.
    pub async fn query_with_files(
        &mut self,
        prompt: &str,
        file_paths: &[String],
        tx: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Result<()> {
        debug!("ACP query: {}", &prompt[..prompt.len().min(80)]);

        // Prepend system prompt on the first query.
        let mut full_prompt = if !self.has_history {
            if let Some(ref sp) = self.system_prompt {
                format!("[System instruction]: {}\n\n{}", sp, prompt)
            } else {
                prompt.to_string()
            }
        } else {
            prompt.to_string()
        };

        // Append file references using @{path} syntax.
        if !file_paths.is_empty() {
            full_prompt.push_str("\n\n");
            for path in file_paths {
                full_prompt.push_str(&format!("@{{{}}}\n", path));
            }
        }

        // Send the prompt via ACP.
        let content = vec![ContentBlock::Text(full_prompt)];
        let conn = self.conn.clone();
        let session_id = self.session_id.clone();

        let prompt_fut = conn.prompt(&session_id, content);
        tokio::pin!(prompt_fut);

        let mut prompt_done = false;
        let mut cancelled = false;

        loop {
            tokio::select! {
                result = &mut prompt_fut, if !prompt_done => {
                    prompt_done = true;
                    match result {
                        Ok(result) => {
                            let stop_reason = result["stopReason"].as_str().unwrap_or("unknown");
                            debug!("ACP prompt finished: stopReason={stop_reason}");
                            if stop_reason == "cancelled" {
                                cancelled = true;
                            }
                        }
                        Err(e) => {
                            error!("ACP prompt error: {e}");
                            let _ = tx.send(format!("\n⚠️ ACP error: {e}")).await;
                        }
                    }
                }
                event = self.event_rx.recv() => {
                    match event {
                        Some(event) => forward_event(&tx, event).await,
                        None => {
                            error!("ACP event channel closed unexpectedly");
                            break;
                        }
                    }
                }
                _ = cancel.cancelled(), if !cancelled => {
                    info!("Cancelling ACP prompt");
                    cancelled = true;
                    if let Err(e) = conn.cancel(&session_id).await {
                        warn!("Failed to send ACP cancel: {e}");
                    }
                }
            }

            if prompt_done {
                // Drain any remaining buffered events.
                while let Ok(event) = self.event_rx.try_recv() {
                    forward_event(&tx, event).await;
                }
                break;
            }
        }

        self.has_history = true;
        Ok(())
    }
}

/// Forward an ACP event to the text channel used by streaming display.
async fn forward_event(tx: &mpsc::Sender<String>, event: AcpEvent) {
    match event {
        AcpEvent::TextChunk(text) => {
            // Send raw text chunk — no newline added, ACP chunks include
            // their own whitespace/newlines.
            let _ = tx.send(text).await;
        }
        AcpEvent::ToolCall { title, status } => {
            if status == "pending" {
                emit_stream_event(tx, "tool", Some("pending"), Some(&title)).await;
            }
        }
        AcpEvent::ToolCallUpdate { status, content } => {
            if status == "completed" {
                let text = content
                    .as_deref()
                    .map(|t| truncate(t, 200))
                    .unwrap_or_else(|| "completed".to_string());
                emit_stream_event(tx, "tool", Some("completed"), Some(&text)).await;
            }
        }
        AcpEvent::Plan(entries) => {
            for entry in entries {
                emit_stream_event(
                    tx,
                    "plan",
                    Some(entry.status.as_str()),
                    Some(entry.content.as_str()),
                )
                .await;
            }
        }
        AcpEvent::Usage {
            input_tokens,
            output_tokens,
        } => {
            emit_stream_event(
                tx,
                "usage",
                None,
                Some(&format!(
                    "input={} output={}",
                    input_tokens.unwrap_or(0),
                    output_tokens.unwrap_or(0)
                )),
            )
            .await;
        }
        AcpEvent::Error(e) => {
            emit_stream_event(tx, "error", None, Some(e.as_str())).await;
        }
    }
}

async fn emit_stream_event(
    tx: &mpsc::Sender<String>,
    kind: &str,
    status: Option<&str>,
    text: Option<&str>,
) {
    let payload = json!({
        "kind": kind,
        "status": status,
        "text": text,
    });
    let _ = tx.send(format!("{STREAM_EVENT_PREFIX}{payload}")).await;
}

/// Truncate a string to a maximum length, adding "…" if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Strip ANSI escape sequences from a string.
#[cfg(test)]
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

/// Thread-safe registry of active ACP sessions.
pub struct SessionManager {
    sessions: DashMap<SessionKey, Arc<Mutex<Session>>>,
    message_counters: DashMap<SessionKey, usize>,
    recent_user_messages: DashMap<SessionKey, VecDeque<String>>,
    warm_pool: Arc<Mutex<Vec<Arc<Mutex<Session>>>>>,
    warm_refill_running: Arc<AtomicBool>,
    config: Arc<Config>,
}

/// Data used to derive a forum topic title update.
pub struct ThreadTitleData {
    pub total_messages: usize,
    pub context_snippet: String,
}

impl SessionManager {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            sessions: DashMap::new(),
            message_counters: DashMap::new(),
            recent_user_messages: DashMap::new(),
            warm_pool: Arc::new(Mutex::new(Vec::new())),
            warm_refill_running: Arc::new(AtomicBool::new(false)),
            config,
        }
    }

    /// Background-fill the warm ACP session pool up to configured target.
    pub fn ensure_warm_pool_background(&self) {
        let target = self.config.warm_session_pool_size;
        if target == 0 {
            return;
        }

        if self.warm_refill_running.swap(true, Ordering::AcqRel) {
            return;
        }

        let pool = self.warm_pool.clone();
        let running = self.warm_refill_running.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            loop {
                let pool_size = { pool.lock().await.len() };
                if pool_size >= target {
                    break;
                }

                match create_session_with_retries(config.clone(), None).await {
                    Ok(session) => {
                        pool.lock().await.push(Arc::new(Mutex::new(session)));
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to fill warm ACP pool");
                        break;
                    }
                }
            }

            running.store(false, Ordering::Release);
        });
    }

    /// Returns whether an ACP session is already warm for this key.
    pub fn has_session(&self, key: &SessionKey) -> bool {
        self.sessions.contains_key(key)
    }

    /// Record one user message for thread naming and return naming context.
    pub fn record_user_message_for_thread_title(
        &self,
        key: &SessionKey,
        text: &str,
    ) -> ThreadTitleData {
        const MAX_STORED_MESSAGES: usize = 12;
        const MAX_CONTEXT_MESSAGES: usize = 5;
        const MAX_CONTEXT_CHARS: usize = 180;

        let mut entry = self.message_counters.entry(*key).or_insert(0);
        *entry += 1;
        let total_messages = *entry;

        let normalized = normalize_for_title(text);
        if !normalized.is_empty() {
            let mut history = self
                .recent_user_messages
                .entry(*key)
                .or_insert_with(VecDeque::new);
            history.push_back(normalized);
            while history.len() > MAX_STORED_MESSAGES {
                history.pop_front();
            }
        }

        let context_snippet = self
            .recent_user_messages
            .get(key)
            .map(|history| {
                let mut selected: Vec<&str> = Vec::new();
                let mut used_chars = 0usize;

                for message in history.iter().rev() {
                    if message.is_empty() {
                        continue;
                    }

                    let message_chars = message.chars().count();
                    let add_chars = if selected.is_empty() {
                        message_chars
                    } else {
                        3 + message_chars // " · "
                    };

                    if used_chars + add_chars > MAX_CONTEXT_CHARS {
                        break;
                    }

                    selected.push(message.as_str());
                    used_chars += add_chars;

                    if selected.len() >= MAX_CONTEXT_MESSAGES {
                        break;
                    }
                }

                selected.reverse();
                selected.join(" · ")
            })
            .unwrap_or_default();

        ThreadTitleData {
            total_messages,
            context_snippet,
        }
    }

    /// Get an existing session or create a new one.
    ///
    /// Returns `(session, is_new)` where `is_new` is `true` when a fresh
    /// session was just created.
    pub async fn get_or_create(&self, key: SessionKey) -> Result<(Arc<Mutex<Session>>, bool)> {
        use dashmap::mapref::entry::Entry;

        if let Some(existing) = self.sessions.get(&key) {
            return Ok((existing.value().clone(), false));
        }

        let maybe_warm = { self.warm_pool.lock().await.pop() };
        let created = if let Some(warm) = maybe_warm {
            info!(?key, "Using prewarmed ACP session from pool");
            warm
        } else {
            Arc::new(Mutex::new(
                create_session_with_retries(self.config.clone(), Some(key)).await?,
            ))
        };

        match self.sessions.entry(key) {
            Entry::Occupied(e) => Ok((e.get().clone(), false)),
            Entry::Vacant(e) => {
                e.insert(created.clone());
                self.ensure_warm_pool_background();
                Ok((created, true))
            }
        }
    }

    /// Reset a session – drop the ACP process and start fresh.
    pub async fn reset(&self, key: &SessionKey) {
        self.sessions.remove(key);
        self.message_counters.remove(key);
        self.recent_user_messages.remove(key);
        info!("Session {:?} reset", key);
        self.ensure_warm_pool_background();
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Number of currently ready prewarmed sessions in the idle pool.
    pub async fn warm_pool_ready_count(&self) -> usize {
        self.warm_pool.lock().await.len()
    }
}

fn normalize_for_title(text: &str) -> String {
    const MAX_SINGLE_MESSAGE_CHARS: usize = 96;

    let compact = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    compact.chars().take(MAX_SINGLE_MESSAGE_CHARS).collect()
}

async fn create_session_with_retries(
    config: Arc<Config>,
    key: Option<SessionKey>,
) -> Result<Session> {
    const MAX_ATTEMPTS: usize = 3;
    const BASE_RETRY_DELAY_MS: u64 = 600;

    let mut last_error: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_ATTEMPTS {
        info!(
            ?key,
            attempt,
            cmd = %config.gemini_cli_command,
            "Creating ACP session"
        );

        match Session::new(
            &config.gemini_cli_command,
            config.gemini_working_dir.as_deref(),
            config.gemini_yolo,
            config.system_prompt.clone(),
        )
        .await
        {
            Ok(session) => {
                if attempt > 1 {
                    info!(?key, attempt, "ACP session created after retry");
                }
                return Ok(session);
            }
            Err(e) => {
                warn!(?key, attempt, error = %e, "ACP session creation attempt failed");
                last_error = Some(e);
                if attempt < MAX_ATTEMPTS {
                    let delay = Duration::from_millis(BASE_RETRY_DELAY_MS * attempt as u64);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    match last_error {
        Some(e) => Err(e.context(format!(
            "Failed to create ACP session for gemini-cli after {MAX_ATTEMPTS} attempts"
        ))),
        None => Err(anyhow::anyhow!(
            "Failed to create ACP session for gemini-cli"
        )),
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
}
