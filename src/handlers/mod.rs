pub mod document;
pub mod message;
pub mod photo;
pub mod voice;

/// RAII guard that deletes a temporary file when dropped.
/// Ensures cleanup even on error/panic paths.
pub struct TempFileGuard(pub String);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        // Use sync fs to avoid panicking if Tokio runtime is shutting down.
        std::fs::remove_file(&self.0).ok();
    }
}

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashMap;
use serde::Deserialize;

use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, Message, ParseMode,
};
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::aggregator::MessageAggregator;
use crate::config::{Config, DraftMode};
use crate::session::{SessionKey, SessionManager, STREAM_EVENT_PREFIX};

/// Registry of active cancellation tokens, keyed by callback data string.
/// Shared between the streaming task and the callback query handler.
pub type CancelRegistry = Arc<DashMap<String, CancellationToken>>;

/// Per-session query queue state.
#[derive(Default)]
pub(crate) struct SessionQueue {
    running: bool,
    pending: VecDeque<QueuedRequest>,
}

/// A queued user request waiting to be processed for a session.
struct QueuedRequest {
    msg: Message,
    text: String,
    files: Vec<String>,
    guards: Vec<Arc<TempFileGuard>>,
}

/// Registry of per-session query queues.
pub type QueryRegistry = Arc<DashMap<SessionKey, Arc<tokio::sync::Mutex<SessionQueue>>>>;

/// Build the session key from a Telegram message.
///
/// Uses `(chat_id, thread_id)` so that each forum topic gets its own
/// gemini-cli session while still isolating chats from each other.
pub fn session_key(msg: &Message) -> SessionKey {
    // ThreadId(MessageId(i32)) — extract the inner i32
    (msg.chat.id.0, msg.thread_id.map(|t| t.0 .0))
}

/// Spawn a background task that waits for the aggregation deadline,
/// combines all buffered parts, and processes the query through gemini-cli.
///
/// By running this in a separate task (instead of blocking the handler),
/// we allow the teloxide dispatcher to process subsequent updates from the
/// same chat immediately — which is critical for the aggregator to actually
/// collect multiple parts from split messages.
pub fn spawn_drain_task(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
    aggregator: Arc<MessageAggregator>,
    key: SessionKey,
    cancel_registry: CancelRegistry,
    query_registry: QueryRegistry,
) {
    tokio::spawn(async move {
        // Wait for the aggregation deadline.
        let combined = loop {
            if let Some(parts) = aggregator.take_if_ready(&key) {
                break MessageAggregator::combine(&parts);
            }
            match aggregator.wait_deadline(&key) {
                Some(d) if !d.is_zero() => tokio::time::sleep(d).await,
                Some(_) => continue,
                None => return,
            }
        };
        let (combined_text, combined_files, guards) = combined;

        enqueue_query(
            bot,
            msg,
            config,
            sessions,
            key,
            combined_text,
            combined_files,
            guards,
            cancel_registry,
            query_registry,
        )
        .await;
    });
}

/// Enqueue a user query for a session and run it when the session is free.
pub async fn enqueue_query(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
    key: SessionKey,
    text: String,
    files: Vec<String>,
    guards: Vec<Arc<TempFileGuard>>,
    cancel_registry: CancelRegistry,
    query_registry: QueryRegistry,
) {
    let queue_arc = query_registry
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(SessionQueue::default())))
        .clone();

    let request = QueuedRequest {
        msg: msg.clone(),
        text,
        files,
        guards,
    };

    let mut start_worker = false;
    let queue_position;
    {
        let mut queue = queue_arc.lock().await;
        queue.pending.push_back(request);
        queue_position = queue.pending.len();
        if !queue.running {
            queue.running = true;
            start_worker = true;
        }
    }

    if !start_worker {
        let text = if queue_position == 1 {
            "⏳ Another request is running. Yours is queued next.".to_string()
        } else {
            format!("⏳ Request queued (position #{queue_position}).")
        };
        let mut req = bot
            .send_message(msg.chat.id, text)
            .disable_notification(true);
        if let Some(tid) = msg.thread_id {
            req = req.message_thread_id(tid);
        }
        req.await.ok();
        return;
    }

    tokio::spawn(process_queue_worker(
        bot,
        config,
        sessions,
        key,
        cancel_registry,
        query_registry,
    ));
}

/// Process queued jobs for one session until the queue is empty.
async fn process_queue_worker(
    bot: Bot,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
    key: SessionKey,
    cancel_registry: CancelRegistry,
    query_registry: QueryRegistry,
) {
    loop {
        let queue_arc = query_registry.get(&key).map(|entry| entry.value().clone());
        let Some(queue_arc) = queue_arc else {
            return;
        };

        let maybe_request = {
            let mut queue = queue_arc.lock().await;
            match queue.pending.pop_front() {
                Some(request) => Some(request),
                None => {
                    queue.running = false;
                    None
                }
            }
        };

        let Some(request) = maybe_request else {
            // No more queued work: remove entry if it is still idle.
            let should_remove = {
                let queue = queue_arc.lock().await;
                !queue.running && queue.pending.is_empty()
            };
            if should_remove {
                query_registry.remove(&key);
            }
            return;
        };

        run_query_job(
            &bot,
            &config,
            &sessions,
            key,
            request,
            cancel_registry.clone(),
        )
        .await;
    }
}

/// Execute one query against the agent and stream its output.
async fn run_query_job(
    bot: &Bot,
    config: &Arc<Config>,
    sessions: &Arc<SessionManager>,
    key: SessionKey,
    request: QueuedRequest,
    cancel_registry: CancelRegistry,
) {
    let mut startup_placeholder_id = None;
    if !sessions.has_session(&key) {
        let mut req = bot
            .send_message(request.msg.chat.id, "⏳ Подключаю Gemini-сессию…")
            .disable_notification(true);
        if let Some(tid) = request.msg.thread_id {
            req = req.message_thread_id(tid);
        }
        if let Ok(sent) = req.await {
            startup_placeholder_id = Some(sent.id);
        }
    }

    let (session, _is_new) = match sessions.get_or_create(key).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create session: {e}");
            let err_msg = format!(
                "❌ Не удалось запустить gemini-cli ACP сессию.\n\
                 Проверь `GEMINI_CLI_COMMAND` (или `GEMINI_CLI_PATH`) в конфиге.\n\
                 Error: {e}"
            );
            if let Some(pid) = startup_placeholder_id {
                bot.edit_message_text(request.msg.chat.id, pid, err_msg)
                    .await
                    .ok();
            } else {
                let mut req = bot.send_message(request.msg.chat.id, err_msg);
                if let Some(tid) = request.msg.thread_id {
                    req = req.message_thread_id(tid);
                }
                req.await.ok();
            }
            return;
        }
    };

    let title_data = sessions.record_user_message_for_thread_title(&key, &request.text);
    maybe_rename_thread_title(
        bot,
        &request.msg,
        &title_data.context_snippet,
        title_data.total_messages,
        config.thread_rename_every,
    )
    .await;

    let cancel = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
    let session_clone = session.clone();
    let sessions_clone = sessions.clone();
    let key_for_reset = key;
    let text = request.text.clone();
    let files = request.files.clone();
    let guards = request.guards;
    let cancel_clone = cancel.clone();

    let query_handle = tokio::spawn(async move {
        let _guards = guards;
        let mut sess = session_clone.lock().await;
        let result = if files.is_empty() {
            sess.query(&text, tx, cancel_clone).await
        } else {
            sess.query_with_files(&text, &files, tx, cancel_clone).await
        };
        if let Err(e) = result {
            error!("Session query error: {e}");
            drop(sess);
            sessions_clone.reset(&key_for_reset).await;
        }
    });

    if let Err(e) = stream_response_with_drafts(
        bot,
        &request.msg,
        config,
        rx,
        startup_placeholder_id,
        cancel,
        cancel_registry.clone(),
    )
    .await
    {
        error!("Stream response error: {e}");
    }

    if let Err(e) = query_handle.await {
        error!("Query task join error: {e}");
    }
}

/// Stream the gemini-cli response to the user, handling file attachments.
///
/// Edits a placeholder message in-place as lines arrive.
/// Final response committed with HTML formatting; long responses are split
/// into multiple Telegram messages.
///
/// An inline keyboard with a "Stop" button is shown during streaming.
pub async fn stream_response_with_drafts(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    mut rx: tokio::sync::mpsc::Receiver<String>,
    initial_placeholder_id: Option<teloxide::types::MessageId>,
    cancel: CancellationToken,
    cancel_registry: CancelRegistry,
) -> Result<()> {
    let draft_mode = config.draft_mode;
    let mut accumulated = String::new();
    let mut activity_log: Vec<String> = Vec::new();
    let mut last_update = Instant::now();
    let mut last_typing = Instant::now();
    let mut last_stream_event = Instant::now();
    let mut last_line = String::new();
    let mut waiting_phase: usize = 0;
    let mut last_draft = String::new();
    let mut attachment_tail = String::new();
    let mut sent_attachments: HashSet<PathBuf> = HashSet::new();
    const UPDATE_INTERVAL: Duration = Duration::from_millis(500);
    const TYPING_INTERVAL: Duration = Duration::from_secs(4);
    const WAITING_TICK_INTERVAL: Duration = Duration::from_secs(2);
    const STALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

    // Show typing indicator immediately.
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await
        .ok();

    // Send placeholder (or reuse startup placeholder) with a "Stop" inline button.
    let mut placeholder_id = initial_placeholder_id;

    if let Some(pid) = placeholder_id {
        if bot
            .edit_message_text(msg.chat.id, pid, "⏳ Думаю…")
            .await
            .is_err()
        {
            placeholder_id = None;
        }
    }

    if placeholder_id.is_none() {
        let mut req = bot.send_message(msg.chat.id, "⏳ Думаю…");
        if let Some(tid) = msg.thread_id {
            req = req.message_thread_id(tid);
        }
        if let Ok(sent) = req.disable_notification(true).await {
            placeholder_id = Some(sent.id);
        }
    }

    let mut callback_key = String::new();
    if let Some(pid) = placeholder_id {
        callback_key = format!("stop:{}:{}", msg.chat.id.0, pid.0);
        cancel_registry.insert(callback_key.clone(), cancel.clone());
        bot.edit_message_reply_markup(msg.chat.id, pid)
            .reply_markup(stop_inline_keyboard(&callback_key))
            .await
            .ok();
    }

    let mut was_cancelled = false;
    let mut was_stalled = false;
    let mut waiting_tick = tokio::time::interval(WAITING_TICK_INTERVAL);
    waiting_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    waiting_tick.tick().await;

    loop {
        tokio::select! {
            line = rx.recv() => {
                let Some(line) = line else { break };
                last_stream_event = Instant::now();

                if let Some(event) = parse_stream_event(&line) {
                    apply_stream_event(&mut activity_log, event);

                    if last_update.elapsed() >= UPDATE_INTERVAL {
                        render_placeholder_draft(
                            bot,
                            msg,
                            placeholder_id,
                            &accumulated,
                            &activity_log,
                            waiting_phase,
                            draft_mode,
                            if callback_key.is_empty() {
                                None
                            } else {
                                Some(callback_key.as_str())
                            },
                            &mut last_draft,
                        )
                        .await;
                        last_update = Instant::now();
                    }
                    continue;
                }

                let clean_text = extract_text_and_send_attachments(
                    bot,
                    msg,
                    config,
                    &line,
                    &mut attachment_tail,
                    &mut sent_attachments,
                )
                .await;

                if clean_text.is_empty() {
                    continue;
                }

                // Skip consecutive duplicate non-empty chunks.
                if clean_text == last_line && !clean_text.is_empty() {
                    continue;
                }
                last_line = clean_text.clone();

                // Accumulate — ACP chunks already include proper
                // whitespace/newlines, so we concatenate directly.
                accumulated.push_str(&clean_text);

                // Refresh typing indicator periodically.
                if last_typing.elapsed() >= TYPING_INTERVAL {
                    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
                        .await
                        .ok();
                    last_typing = Instant::now();
                }

                // Stream updates by editing the placeholder message.
                if last_update.elapsed() >= UPDATE_INTERVAL {
                    render_placeholder_draft(
                        bot,
                        msg,
                        placeholder_id,
                        &accumulated,
                        &activity_log,
                        waiting_phase,
                        draft_mode,
                        if callback_key.is_empty() {
                            None
                        } else {
                            Some(callback_key.as_str())
                        },
                        &mut last_draft,
                    )
                    .await;
                    last_update = Instant::now();
                }
            }
            _ = waiting_tick.tick() => {
                waiting_phase = waiting_phase.wrapping_add(1);

                if last_stream_event.elapsed() >= STALL_TIMEOUT
                    && accumulated.trim().is_empty()
                    && activity_log.is_empty()
                {
                    was_stalled = true;
                    was_cancelled = true;
                    cancel.cancel();
                    break;
                }

                if last_typing.elapsed() >= TYPING_INTERVAL {
                    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
                        .await
                        .ok();
                    last_typing = Instant::now();
                }

                if accumulated.trim().is_empty() || !activity_log.is_empty() {
                    render_placeholder_draft(
                        bot,
                        msg,
                        placeholder_id,
                        &accumulated,
                        &activity_log,
                        waiting_phase,
                        draft_mode,
                        if callback_key.is_empty() {
                            None
                        } else {
                            Some(callback_key.as_str())
                        },
                        &mut last_draft,
                    )
                    .await;
                    last_update = Instant::now();
                }
            }
            _ = cancel.cancelled() => {
                was_cancelled = true;
                break;
            }
        }
    }

    // Flush buffered tail to catch attachment directives that arrived
    // without a trailing newline.
    if !attachment_tail.is_empty() {
        let buffered_tail = std::mem::take(&mut attachment_tail);
        let forced = format!("{}\n", buffered_tail);
        let mut flush_tail = String::new();
        let clean_tail = extract_text_and_send_attachments(
            bot,
            msg,
            config,
            &forced,
            &mut flush_tail,
            &mut sent_attachments,
        )
        .await;
        if !clean_tail.is_empty() {
            accumulated.push_str(&clean_tail);
        }
    }

    // Clean up: remove cancellation token from registry.
    if !callback_key.is_empty() {
        cancel_registry.remove(&callback_key);
    }

    // Remove the inline keyboard from the placeholder.
    if let Some(pid) = placeholder_id {
        bot.edit_message_reply_markup(msg.chat.id, pid).await.ok();
    }

    // Build suffix for cancelled messages.
    let cancel_suffix = if was_stalled {
        "\n\n⏱️ <i>Зависло по таймауту ожидания, остановлено</i>"
    } else if was_cancelled {
        "\n\n⬛ Генерация остановлена"
    } else {
        ""
    };

    // Final commit.
    if accumulated.is_empty() {
        let empty_msg = if was_stalled {
            "⏱️ Зависло по таймауту ожидания, остановлено"
        } else if was_cancelled {
            "⬛ Генерация остановлена"
        } else if !activity_log.is_empty() {
            "🧠 Ход работы получен, но текстовый ответ не пришел."
        } else {
            "_(нет ответа)_"
        };
        if let Some(pid) = placeholder_id {
            bot.edit_message_text(msg.chat.id, pid, empty_msg)
                .await
                .ok();
        }
        return Ok(());
    }

    let final_text = accumulated.clone();

    let html = format!(
        "{}{}",
        markdown_to_telegram_html(&final_text),
        if was_cancelled {
            "\n\n⬛ <i>Генерация остановлена</i>"
        } else {
            ""
        }
    );
    let chunks = split_text(&html);

    // In verbose mode we keep progress in a separate message and leave
    // the final answer clean.
    if draft_mode == DraftMode::Verbose && !activity_log.is_empty() {
        let progress_text = format_progress_message(&activity_log, 8);
        if let Some(pid) = placeholder_id {
            bot.edit_message_text(msg.chat.id, pid, truncate_text(&progress_text))
                .await
                .ok();
        } else {
            send_plain_message(bot, msg, &progress_text, true).await;
        }
        send_html_chunks(bot, msg, &chunks).await;
        return Ok(());
    }

    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            if let Some(pid) = placeholder_id {
                let edit_res = bot
                    .edit_message_text(msg.chat.id, pid, chunk)
                    .parse_mode(ParseMode::Html)
                    .await;
                if let Err(e) = edit_res {
                    tracing::warn!("HTML format failed: {e}, falling back to plain text");
                    let plain = format!("{}{}", final_text, cancel_suffix);
                    let plain_chunks = split_text(&plain);
                    bot.edit_message_text(msg.chat.id, pid, &plain_chunks[0])
                        .await
                        .ok();
                    for plain_chunk in &plain_chunks[1..] {
                        let mut req = bot.send_message(msg.chat.id, plain_chunk);
                        if let Some(tid) = msg.thread_id {
                            req = req.message_thread_id(tid);
                        }
                        req.await.ok();
                    }
                    return Ok(());
                }
            } else {
                let mut req = bot
                    .send_message(msg.chat.id, chunk)
                    .parse_mode(ParseMode::Html);
                if let Some(tid) = msg.thread_id {
                    req = req.message_thread_id(tid);
                }
                req.await.ok();
            }
        } else {
            let mut req = bot
                .send_message(msg.chat.id, chunk)
                .parse_mode(ParseMode::Html);
            if let Some(tid) = msg.thread_id {
                req = req.message_thread_id(tid);
            }
            if let Err(_) = req.await {
                let mut req = bot.send_message(msg.chat.id, chunk);
                if let Some(tid) = msg.thread_id {
                    req = req.message_thread_id(tid);
                }
                req.await.ok();
            }
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct StreamEventPayload {
    kind: String,
    status: Option<String>,
    text: Option<String>,
}

fn parse_stream_event(chunk: &str) -> Option<StreamEventPayload> {
    let payload = chunk.strip_prefix(STREAM_EVENT_PREFIX)?;
    serde_json::from_str(payload).ok()
}

fn apply_stream_event(activity_log: &mut Vec<String>, event: StreamEventPayload) {
    match event.kind.as_str() {
        "tool" => match event.status.as_deref() {
            Some("completed") => {
                push_activity(activity_log, "✅ Инструменты выполнены".to_string())
            }
            Some("pending") => push_activity(activity_log, "🛠 Использую инструменты".to_string()),
            _ => push_activity(activity_log, "🛠 Работаю с инструментами".to_string()),
        },
        "plan" => match event.status.as_deref() {
            Some("completed") => push_activity(activity_log, "✅ Шаг плана завершен".to_string()),
            Some("in_progress") => push_activity(activity_log, "🔄 Выполняю шаг плана".to_string()),
            _ => push_activity(activity_log, "📋 Обновляю план".to_string()),
        },
        "error" => {
            if let Some(text) = event.text {
                push_activity(activity_log, format!("⚠️ {}", compact_line(&text)));
            }
        }
        "usage" => {}
        _ => {}
    }
}

fn push_activity(activity_log: &mut Vec<String>, line: String) {
    let line = line.trim().to_string();
    if line.is_empty() {
        return;
    }
    if activity_log.last().is_some_and(|last| last == &line) {
        return;
    }
    activity_log.push(line);

    const MAX_ACTIVITY: usize = 20;
    if activity_log.len() > MAX_ACTIVITY {
        let drop_count = activity_log.len() - MAX_ACTIVITY;
        activity_log.drain(0..drop_count);
    }
}

async fn render_placeholder_draft(
    bot: &Bot,
    msg: &Message,
    placeholder_id: Option<teloxide::types::MessageId>,
    accumulated: &str,
    activity_log: &[String],
    waiting_phase: usize,
    draft_mode: DraftMode,
    callback_key: Option<&str>,
    last_draft: &mut String,
) {
    let Some(pid) = placeholder_id else {
        return;
    };

    let draft = build_draft_text(accumulated, activity_log, waiting_phase, draft_mode);
    if draft == *last_draft {
        return;
    }

    let mut req = bot.edit_message_text(msg.chat.id, pid, draft.clone());
    if let Some(key) = callback_key {
        req = req.reply_markup(stop_inline_keyboard(key));
    }
    req.await.ok();
    *last_draft = draft;
}

fn stop_inline_keyboard(callback_key: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "🛑 Stop",
        callback_key,
    )]])
}

fn build_draft_text(
    accumulated: &str,
    activity_log: &[String],
    waiting_phase: usize,
    draft_mode: DraftMode,
) -> String {
    const WAITING_LABELS: [&str; 3] = ["⏳ Думаю…", "⏳ Собираю контекст…", "⏳ Все еще работаю…"];
    let waiting_label = WAITING_LABELS[waiting_phase % WAITING_LABELS.len()];

    match draft_mode {
        DraftMode::Compact => {
            let out = if accumulated.trim().is_empty() {
                waiting_label.to_string()
            } else {
                tail_text(accumulated, 2800)
            };
            truncate_text(&out)
        }
        DraftMode::Verbose => {
            let mut out = String::new();
            if !activity_log.is_empty() {
                out.push_str("🧠 Ход работы\n");
                for line in recent_activity(activity_log, 4) {
                    out.push_str("• ");
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }

            if accumulated.trim().is_empty() {
                out.push_str(waiting_label);
            } else {
                out.push_str(&tail_text(accumulated, 2800));
            }

            truncate_text(&out)
        }
    }
}

fn recent_activity(activity_log: &[String], count: usize) -> Vec<&str> {
    let mut recent: Vec<&str> = activity_log
        .iter()
        .rev()
        .take(count)
        .map(String::as_str)
        .collect();
    recent.reverse();
    recent
}

fn compact_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tail_text(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }

    let start_idx = total.saturating_sub(max_chars.saturating_sub(1));
    let tail: String = text.chars().skip(start_idx).collect();
    format!("…{tail}")
}

async fn extract_text_and_send_attachments(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    chunk: &str,
    tail: &mut String,
    sent_attachments: &mut HashSet<PathBuf>,
) -> String {
    let mut combined = String::new();
    if !tail.is_empty() {
        combined.push_str(tail);
        tail.clear();
    }
    combined.push_str(chunk);

    let ends_with_newline = combined.ends_with('\n');
    let parts: Vec<&str> = combined.split('\n').collect();
    let mut clean_text = String::new();

    for (idx, raw_part) in parts.iter().enumerate() {
        let is_last = idx + 1 == parts.len();
        let line = raw_part.trim_end_matches('\r');

        if is_last && !ends_with_newline {
            if could_be_attachment_fragment(line) {
                tail.push_str(raw_part);
            } else {
                clean_text.push_str(raw_part);
            }
            break;
        }

        if let Some(path_hint) = extract_attachment_hint(line) {
            if let Some(path) = resolve_attachment_path(&path_hint, config) {
                if sent_attachments.insert(path.clone()) {
                    let mut req = bot.send_document(msg.chat.id, InputFile::file(path));
                    if let Some(tid) = msg.thread_id {
                        req = req.message_thread_id(tid);
                    }
                    if let Err(e) = req.await {
                        error!("Failed to send document: {e}");
                    }
                }
            }
        } else {
            clean_text.push_str(raw_part);
            clean_text.push('\n');
        }
    }

    clean_text
}

fn extract_attachment_hint(line: &str) -> Option<String> {
    const ATTACH_PREFIX: &str = "ATTACH_FILE:";

    let normalized = normalize_attachment_line(line);
    if normalized.is_empty() {
        return None;
    }

    if let Some(pos) = normalized.find(ATTACH_PREFIX) {
        let hint = normalized[pos + ATTACH_PREFIX.len()..].trim();
        let hint = sanitize_path_hint(hint);
        if !hint.is_empty() {
            return Some(hint);
        }
    }

    // Fallback: if the model outputs a standalone filename/path on a line
    // (without ATTACH_FILE), try using it as attachment hint.
    if !normalized.contains(' ') && normalized.contains('.') {
        let hint = sanitize_path_hint(&normalized);
        if !hint.is_empty() {
            return Some(hint);
        }
    }

    None
}

fn normalize_attachment_line(line: &str) -> String {
    let mut s = line.trim().to_string();
    for prefix in ["- ", "• ", "* "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start().to_string();
            break;
        }
    }
    s
}

fn sanitize_path_hint(raw: &str) -> String {
    let mut s = raw
        .trim()
        .trim_matches(|c| c == '`' || c == '"' || c == '\'')
        .to_string();

    if let Some(stripped) = s.strip_prefix("file://") {
        s = stripped.to_string();
    }

    while s
        .chars()
        .last()
        .is_some_and(|c| matches!(c, ',' | '.' | ';' | ':' | ')' | ']' | '>'))
    {
        s.pop();
    }

    s
}

fn resolve_attachment_path(path_hint: &str, config: &Config) -> Option<PathBuf> {
    let expanded = expand_tilde(Path::new(path_hint));

    if expanded.is_absolute() && expanded.exists() {
        return Some(expanded);
    }

    if let Some(base) = config.gemini_working_dir.as_deref() {
        let candidate = Path::new(base).join(&expanded);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    std::env::current_dir().ok().and_then(|cwd| {
        let candidate = cwd.join(&expanded);
        if candidate.exists() {
            Some(candidate)
        } else {
            None
        }
    })
}

fn could_be_attachment_fragment(line: &str) -> bool {
    const ATTACH_PREFIX: &str = "ATTACH_FILE:";

    let normalized = normalize_attachment_line(line);
    if normalized.is_empty() {
        return false;
    }

    normalized.contains(ATTACH_PREFIX)
        || ATTACH_PREFIX.starts_with(normalized.as_str())
        || normalized
            .split_whitespace()
            .last()
            .is_some_and(|last| ATTACH_PREFIX.starts_with(last))
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

async fn maybe_rename_thread_title(
    bot: &Bot,
    msg: &Message,
    context_snippet: &str,
    total_messages: usize,
    rename_every: usize,
) {
    if total_messages == 0 {
        return;
    }

    let should_rename =
        total_messages == 1 || (rename_every > 0 && total_messages.is_multiple_of(rename_every));
    if !should_rename {
        return;
    }

    let Some(thread_id) = msg.thread_id else {
        return;
    };

    let title = make_thread_title(context_snippet, total_messages);
    bot.edit_forum_topic(msg.chat.id, thread_id)
        .name(title)
        .await
        .ok();
}

fn make_thread_title(context_snippet: &str, total_messages: usize) -> String {
    let summary = compact_line(context_snippet);
    let summary = if summary.is_empty() {
        "Сессия".to_string()
    } else {
        summary
    };

    let summary: String = summary.chars().take(96).collect();
    let mut title = if total_messages <= 1 {
        summary
    } else {
        format!("{summary} · #{total_messages}")
    };
    if title.chars().count() > 128 {
        title = title.chars().take(128).collect();
    }
    if title.trim().is_empty() {
        "Сессия".to_string()
    } else {
        title
    }
}

fn format_progress_message(activity_log: &[String], count: usize) -> String {
    let mut out = String::from("🧠 Ход работы\n");
    for line in recent_activity(activity_log, count) {
        out.push_str("• ");
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

async fn send_plain_message(bot: &Bot, msg: &Message, text: &str, disable_notification: bool) {
    for chunk in split_text(text) {
        let mut req = bot.send_message(msg.chat.id, chunk);
        if let Some(tid) = msg.thread_id {
            req = req.message_thread_id(tid);
        }
        if disable_notification {
            req = req.disable_notification(true);
        }
        req.await.ok();
    }
}

async fn send_html_chunks(bot: &Bot, msg: &Message, chunks: &[String]) {
    for chunk in chunks {
        let mut req = bot
            .send_message(msg.chat.id, chunk)
            .parse_mode(ParseMode::Html);
        if let Some(tid) = msg.thread_id {
            req = req.message_thread_id(tid);
        }
        if req.await.is_err() {
            let mut plain_req = bot.send_message(msg.chat.id, chunk);
            if let Some(tid) = msg.thread_id {
                plain_req = plain_req.message_thread_id(tid);
            }
            plain_req.await.ok();
        }
    }
}

/// Handle the "Stop" inline button callback.
pub async fn handle_stop_callback(
    bot: Bot,
    q: CallbackQuery,
    cancel_registry: CancelRegistry,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(data) = &q.data {
        if data.starts_with("stop:") {
            if let Some((_, token)) = cancel_registry.remove(data) {
                token.cancel();
            }
        }
    }
    // Acknowledge the callback to dismiss the spinner on the button.
    bot.answer_callback_query(&q.id)
        .text("🛑 Остановлено")
        .await
        .ok();
    Ok(())
}

/// Convert standard Markdown (as emitted by gemini-cli) to Telegram-compatible HTML.
///
/// Telegram supports: `<b>`, `<i>`, `<code>`, `<pre>`, `<a href="...">`, `<s>`, `<blockquote>`.
/// Standard Markdown features like `###` headers or `* ` bullets are NOT supported natively.
fn markdown_to_telegram_html(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 256);
    let mut in_code_block = false;
    #[allow(unused_assignments)]
    let mut code_lang = String::new();

    for line in md.split('\n') {
        // Toggle fenced code blocks (``` or ```rust)
        if line.starts_with("```") {
            if in_code_block {
                out.push_str("</code></pre>\n");
                in_code_block = false;
            } else {
                code_lang = line.trim_start_matches('`').trim().to_string();
                if code_lang.is_empty() {
                    out.push_str("<pre><code>");
                } else {
                    out.push_str(&format!("<pre><code class=\"language-{code_lang}\">"));
                }
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            // Inside code blocks: only escape HTML entities, no formatting.
            out.push_str(&escape_html(line));
            out.push('\n');
            continue;
        }

        // Headers → bold text
        let processed = if let Some(header) = line.strip_prefix("### ") {
            format!("\n<b>{}</b>", escape_html(header.trim()))
        } else if let Some(header) = line.strip_prefix("## ") {
            format!("\n<b>{}</b>", escape_html(header.trim()))
        } else if let Some(header) = line.strip_prefix("# ") {
            format!("\n<b>{}</b>", escape_html(header.trim()))
        }
        // Bullet lists → •
        else if let Some(rest) = line.strip_prefix("* ") {
            format!("• {}", format_inline(&escape_html(rest)))
        } else if let Some(rest) = line.strip_prefix("- ") {
            format!("• {}", format_inline(&escape_html(rest)))
        }
        // Numbered lists — pass through with inline formatting
        else if line.chars().next().map_or(false, |c| c.is_ascii_digit()) && line.contains(". ") {
            format_inline(&escape_html(line))
        }
        // Horizontal rules
        else if line.trim() == "---" || line.trim() == "***" {
            "—————".to_string()
        }
        // Blockquotes
        else if let Some(rest) = line.strip_prefix("> ") {
            format!(
                "<blockquote>{}</blockquote>",
                format_inline(&escape_html(rest))
            )
        }
        // Regular text
        else {
            format_inline(&escape_html(line))
        };

        out.push_str(&processed);
        out.push('\n');
    }

    // Close unclosed code block
    if in_code_block {
        out.push_str("</code></pre>\n");
    }

    // Trim trailing newlines
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Escape HTML special characters.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Apply inline Markdown formatting to already-escaped HTML text.
/// Handles: **bold**, *italic*, `code`, [text](url)
fn format_inline(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Inline code: `text`
        if chars[i] == '`' {
            if let Some(end) = find_closing(&chars, i + 1, '`') {
                result.push_str("<code>");
                let code: String = chars[i + 1..end].iter().collect();
                result.push_str(&code);
                result.push_str("</code>");
                i = end + 1;
                continue;
            }
        }
        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_double_closing(&chars, i + 2, '*') {
                result.push_str("<b>");
                let inner: String = chars[i + 2..end].iter().collect();
                result.push_str(&inner);
                result.push_str("</b>");
                i = end + 2;
                continue;
            }
        }
        // Italic: *text* (single asterisk, not at start of bullet)
        if chars[i] == '*' && i + 1 < len && chars[i + 1] != ' ' {
            if let Some(end) = find_closing(&chars, i + 1, '*') {
                result.push_str("<i>");
                let inner: String = chars[i + 1..end].iter().collect();
                result.push_str(&inner);
                result.push_str("</i>");
                i = end + 1;
                continue;
            }
        }
        // Links: [text](url)
        if chars[i] == '[' {
            if let Some((text_end, url_start, url_end)) = find_link(&chars, i) {
                let link_text: String = chars[i + 1..text_end].iter().collect();
                let url: String = chars[url_start..url_end].iter().collect();
                result.push_str(&format!("<a href=\"{url}\">{link_text}</a>"));
                i = url_end + 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

fn find_closing(chars: &[char], start: usize, marker: char) -> Option<usize> {
    for i in start..chars.len() {
        if chars[i] == marker {
            return Some(i);
        }
    }
    None
}

fn find_double_closing(chars: &[char], start: usize, marker: char) -> Option<usize> {
    for i in start..chars.len().saturating_sub(1) {
        if chars[i] == marker && chars[i + 1] == marker {
            return Some(i);
        }
    }
    None
}

fn find_link(chars: &[char], start: usize) -> Option<(usize, usize, usize)> {
    // Find ]( after [
    let text_end = find_closing(chars, start + 1, ']')?;
    if text_end + 1 < chars.len() && chars[text_end + 1] == '(' {
        let url_start = text_end + 2;
        let url_end = find_closing(chars, url_start, ')')?;
        Some((text_end, url_start, url_end))
    } else {
        None
    }
}

/// Truncate text to Telegram's 4096-character limit (used for interim draft
/// updates only — the final response uses `split_text` instead).
fn truncate_text(text: &str) -> String {
    const MAX: usize = 4096;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    }
}

/// Split text into chunks that each fit within Telegram's 4096-character limit.
///
/// Splits on newline boundaries when possible to avoid breaking mid-sentence.
fn split_text(text: &str) -> Vec<String> {
    const MAX: usize = 4096;
    if text.chars().count() <= MAX {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        let char_count = remaining.chars().count();
        if char_count <= MAX {
            chunks.push(remaining.to_string());
            break;
        }

        // Find a newline boundary within the limit to split on.
        let byte_limit = remaining
            .char_indices()
            .nth(MAX)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let split_at = remaining[..byte_limit]
            .rfind('\n')
            .map(|pos| pos + 1) // Include the newline in current chunk
            .unwrap_or(byte_limit); // No newline found — hard split at limit

        chunks.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        let text = "Hello, world!";
        assert_eq!(truncate_text(text), text);
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_text(""), "");
    }

    #[test]
    fn truncate_exactly_at_limit() {
        let text: String = "a".repeat(4096);
        assert_eq!(truncate_text(&text), text);
    }

    #[test]
    fn truncate_over_limit() {
        let text: String = "a".repeat(5000);
        let result = truncate_text(&text);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 4096);
    }

    #[test]
    fn truncate_unicode_characters() {
        let text: String = "🎉".repeat(5000);
        let result = truncate_text(&text);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 4096);
    }

    // ── split_text ───────────────────────────────────────────────────────

    #[test]
    fn split_short_text_single_chunk() {
        let text = "Hello, world!";
        let chunks = split_text(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn split_empty_string_single_chunk() {
        let chunks = split_text("");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "");
    }

    #[test]
    fn split_exactly_at_limit_single_chunk() {
        let text: String = "a".repeat(4096);
        let chunks = split_text(&text);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn split_over_limit_multiple_chunks() {
        // Build text with newlines so splitting is predictable.
        let line = "abcdefghij\n"; // 11 chars per line
        let text: String = line.repeat(500); // 5500 chars
        let chunks = split_text(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 4096);
        }
        // Joined chunks should reconstruct the original text.
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn split_unicode_text() {
        let line = "Привет мир!\n"; // ~12 chars per line
        let text: String = line.repeat(500);
        let chunks = split_text(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 4096);
        }
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn split_no_newlines_hard_splits() {
        let text: String = "a".repeat(5000);
        let chunks = split_text(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 4096);
        assert_eq!(chunks[1].chars().count(), 904);
    }

    #[test]
    fn test_markdown_headers() {
        assert_eq!(markdown_to_telegram_html("# Header 1"), "\n<b>Header 1</b>");
        assert_eq!(
            markdown_to_telegram_html("## Header 2"),
            "\n<b>Header 2</b>"
        );
        assert_eq!(
            markdown_to_telegram_html("### Header 3"),
            "\n<b>Header 3</b>"
        );
        // Escaping HTML in headers
        assert_eq!(
            markdown_to_telegram_html("# Header <1>"),
            "\n<b>Header &lt;1&gt;</b>"
        );
    }

    #[test]
    fn test_markdown_bullet_lists() {
        assert_eq!(
            markdown_to_telegram_html("* Item 1\n* Item 2"),
            "• Item 1\n• Item 2"
        );
        assert_eq!(
            markdown_to_telegram_html("- Item 1\n- Item 2"),
            "• Item 1\n• Item 2"
        );
    }

    #[test]
    fn test_markdown_numbered_lists() {
        assert_eq!(
            markdown_to_telegram_html("1. Item 1\n2. Item 2"),
            "1. Item 1\n2. Item 2"
        );
        // Inline formatting maintained in numbered lists
        assert_eq!(
            markdown_to_telegram_html("1. **Bold** item"),
            "1. <b>Bold</b> item"
        );
    }

    #[test]
    fn test_markdown_fenced_code_blocks() {
        assert_eq!(
            markdown_to_telegram_html("```\nfn main() {}\n```"),
            "<pre><code>fn main() {}\n</code></pre>"
        );
        assert_eq!(
            markdown_to_telegram_html("```rust\nfn main() {}\n```"),
            "<pre><code class=\"language-rust\">fn main() {}\n</code></pre>"
        );
        // Escaping in code blocks but no inline formatting
        assert_eq!(
            markdown_to_telegram_html("```\n**bold** <tag>\n```"),
            "<pre><code>**bold** &lt;tag&gt;\n</code></pre>"
        );

        // Unclosed code block
        assert_eq!(
            markdown_to_telegram_html("```\nfn main() {}"),
            "<pre><code>fn main() {}\n</code></pre>"
        );
    }

    #[test]
    fn test_markdown_html_escaping() {
        assert_eq!(
            markdown_to_telegram_html("<hello> & \"world\""),
            "&lt;hello&gt; &amp; \"world\""
        );
        assert_eq!(
            markdown_to_telegram_html("Me & You > Them"),
            "Me &amp; You &gt; Them"
        );
    }

    #[test]
    fn test_markdown_inline_formatting() {
        assert_eq!(
            markdown_to_telegram_html("This is **bold**"),
            "This is <b>bold</b>"
        );
        assert_eq!(
            markdown_to_telegram_html("This is *italic*"),
            "This is <i>italic</i>"
        );
        assert_eq!(
            markdown_to_telegram_html("This is `code`"),
            "This is <code>code</code>"
        );
        assert_eq!(
            markdown_to_telegram_html("This is a [link](https://example.com)"),
            "This is a <a href=\"https://example.com\">link</a>"
        );

        // Mixed
        assert_eq!(
            markdown_to_telegram_html("This is **bold** and *italic*"),
            "This is <b>bold</b> and <i>italic</i>"
        );
    }

    // ── CancelRegistry ──────────────────────────────────────────────────

    #[test]
    fn cancel_registry_insert_and_remove() {
        let registry: CancelRegistry = Arc::new(DashMap::new());
        let token = CancellationToken::new();
        registry.insert("stop:123:456".to_string(), token.clone());

        assert_eq!(registry.len(), 1);
        assert!(!token.is_cancelled());

        let (_, removed) = registry.remove("stop:123:456").expect("should exist");
        removed.cancel();
        assert!(token.is_cancelled());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn cancel_registry_remove_nonexistent_returns_none() {
        let registry: CancelRegistry = Arc::new(DashMap::new());
        assert!(registry.remove("stop:999:999").is_none());
    }

    #[test]
    fn cancel_registry_multiple_tokens_isolated() {
        let registry: CancelRegistry = Arc::new(DashMap::new());
        let token_a = CancellationToken::new();
        let token_b = CancellationToken::new();
        registry.insert("stop:1:1".to_string(), token_a.clone());
        registry.insert("stop:2:2".to_string(), token_b.clone());

        assert_eq!(registry.len(), 2);

        // Cancel only token A.
        if let Some((_, t)) = registry.remove("stop:1:1") {
            t.cancel();
        }
        assert!(token_a.is_cancelled());
        assert!(!token_b.is_cancelled());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn cancel_token_propagates_to_clones() {
        let token = CancellationToken::new();
        let clone1 = token.clone();
        let clone2 = token.clone();

        assert!(!clone1.is_cancelled());
        assert!(!clone2.is_cancelled());

        token.cancel();

        assert!(clone1.is_cancelled());
        assert!(clone2.is_cancelled());
    }

    // ── split_text edge cases ───────────────────────────────────────────

    #[test]
    fn split_three_or_more_chunks() {
        // 12288 chars > 3 * 4096 → should produce 3 chunks.
        let line = "abcdefghij\n"; // 11 chars
        let text: String = line.repeat(1200); // 13200 chars
        let chunks = split_text(&text);
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 4096);
        }
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn split_preserves_newline_boundaries() {
        // Ensure we split at \n, not in the middle of a word.
        let mut text = String::new();
        for i in 0..500 {
            text.push_str(&format!("Line number {i:04}\n"));
        }
        let chunks = split_text(&text);
        for chunk in &chunks {
            // Every chunk (except possibly the last) should end with \n.
            if chunk.chars().count() == 4096 {
                // Hard-split case — allowed but unlikely with these short lines.
            } else if !chunk.is_empty() {
                assert!(
                    chunk.ends_with('\n') || chunk == chunks.last().unwrap(),
                    "Expected chunk to end with newline"
                );
            }
        }
    }

    #[test]
    fn split_emoji_heavy_text() {
        // Each emoji is 1 char but 4 bytes — verify char-based splitting.
        let line = "🎉🎊🎈🎁🎀\n"; // 6 chars per line
        let text: String = line.repeat(1000); // 6000 chars
        let chunks = split_text(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 4096);
        }
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn split_single_long_line_with_trailing() {
        // Long first line + short second line.
        let long = "x".repeat(4000);
        let text = format!("{long}\nshort line");
        let chunks = split_text(&text);
        assert_eq!(chunks.len(), 1); // 4011 chars < 4096
    }

    #[test]
    fn split_exactly_double_limit() {
        let text: String = "a".repeat(8192);
        let chunks = split_text(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 4096);
    }

    // ── markdown edge cases ─────────────────────────────────────────────

    #[test]
    fn test_markdown_horizontal_rules() {
        assert_eq!(markdown_to_telegram_html("---"), "—————");
        assert_eq!(markdown_to_telegram_html("***"), "—————");
    }

    #[test]
    fn test_markdown_blockquotes() {
        assert_eq!(
            markdown_to_telegram_html("> This is a quote"),
            "<blockquote>This is a quote</blockquote>"
        );
        assert_eq!(
            markdown_to_telegram_html("> **Bold** quote"),
            "<blockquote><b>Bold</b> quote</blockquote>"
        );
    }

    #[test]
    fn test_markdown_empty_string() {
        assert_eq!(markdown_to_telegram_html(""), "");
    }

    #[test]
    fn test_markdown_mixed_content() {
        let md = "# Title\n\nSome **bold** text\n\n- Item 1\n- Item 2\n\n```\ncode\n```\n\n> Quote";
        let html = markdown_to_telegram_html(md);
        assert!(html.contains("<b>Title</b>"));
        assert!(html.contains("<b>bold</b>"));
        assert!(html.contains("• Item 1"));
        assert!(html.contains("<pre><code>"));
        assert!(html.contains("<blockquote>"));
    }

    #[test]
    fn test_markdown_link_with_special_chars() {
        assert_eq!(
            markdown_to_telegram_html("[docs](https://example.com/path?a=1&b=2)"),
            "<a href=\"https://example.com/path?a=1\u{26}amp;b=2\">docs</a>"
        );
    }

    #[test]
    fn test_escape_html_all_entities() {
        assert_eq!(escape_html("<>&"), "&lt;&gt;&amp;");
        assert_eq!(escape_html("normal text"), "normal text");
        assert_eq!(escape_html(""), "");
        assert_eq!(escape_html("a < b > c & d"), "a &lt; b &gt; c &amp; d");
    }

    #[test]
    fn test_markdown_dash_bullet_with_inline() {
        assert_eq!(
            markdown_to_telegram_html("- **bold** item"),
            "• <b>bold</b> item"
        );
    }
}
