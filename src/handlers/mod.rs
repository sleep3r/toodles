pub mod document;
pub mod message;
pub mod voice;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InputFile, Message};
use tokio::sync::Mutex;
use tracing::error;

use crate::config::Config;
use crate::session::{Session, SessionKey};


/// Build the session key from a Telegram message.
///
/// Uses `(chat_id, thread_id)` so that each forum topic gets its own
/// gemini-cli session while still isolating chats from each other.
pub fn session_key(msg: &Message) -> SessionKey {
    // ThreadId(MessageId(i32)) — extract the inner i32
    (msg.chat.id.0, msg.thread_id.map(|t| t.0 .0))
}



/// Show an animated "starting session" indicator in the chat while
/// warming up gemini-cli. Edits the placeholder message with a dot
/// animation, then shows ✅ on success or ❌ on failure.
///
/// Returns `Ok(())` if warm-up succeeded, or an error.
pub async fn warm_up_with_indicator(
    bot: &Bot,
    msg: &Message,
    session: &Arc<Mutex<Session>>,
) -> Result<()> {
    use message::send_reply;

    let placeholder = send_reply(bot, msg, "Загружаю Gemini ·").await?;

    // Spawn warm-up in a background task so we can animate concurrently.
    let session_clone = session.clone();
    let mut warmup_handle = tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        sess.warm_up().await
    });

    // Animate the placeholder while warm-up runs.
    let mut tick = 0usize;
    loop {
        tokio::select! {
            result = &mut warmup_handle => {
                // Warm-up finished.
                match result {
                    Ok(Ok(())) => {
                        bot.edit_message_text(
                            msg.chat.id,
                            placeholder.id,
                            "✨ Готово, слушаю!",
                        ).await.ok();
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        error!("Warm-up failed: {e}");
                        bot.edit_message_text(
                            msg.chat.id,
                            placeholder.id,
                            format!("Не удалось запуститься: {e}"),
                        ).await.ok();
                        return Err(e);
                    }
                    Err(join_err) => {
                        error!("Warm-up task panicked: {join_err}");
                        bot.edit_message_text(
                            msg.chat.id,
                            placeholder.id,
                            "Что-то пошло не так :(",
                        ).await.ok();
                        anyhow::bail!("warm-up task panicked");
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(1500)) => {
                let frames = ["Загружаю Gemini ·", "Загружаю Gemini ··", "Загружаю Gemini ···"];
                let frame = frames[tick % frames.len()];
                tick += 1;
                bot.edit_message_text(
                    msg.chat.id,
                    placeholder.id,
                    frame,
                ).await.ok();
            }
        }
    }
}

/// Stream the gemini-cli response to the user, handling file attachments.
///
/// Uses a single placeholder message updated via `edit_message_text`.
/// No drafts, no fallbacks — just simple edits with rate limiting.
pub async fn stream_response_with_drafts(
    bot: &Bot,
    msg: &Message,
    _config: &Config,
    mut rx: tokio::sync::mpsc::Receiver<String>,
) -> Result<()> {
    use teloxide::types::ParseMode;

    // Send a placeholder so the user sees immediate feedback.
    let mut req = bot.send_message(msg.chat.id, "Думаю...");
    if let Some(tid) = msg.thread_id {
        req = req.message_thread_id(tid);
    }
    let placeholder = req.disable_notification(true).await?;

    let mut accumulated = String::new();
    let mut last_update = Instant::now();
    let mut last_typing = Instant::now();
    let mut last_sent_text = String::from("Думаю...");
    const UPDATE_INTERVAL: Duration = Duration::from_secs(1);
    const TYPING_INTERVAL: Duration = Duration::from_secs(4);

    // Show typing indicator immediately.
    bot.send_chat_action(msg.chat.id, ChatAction::Typing).await.ok();

    while let Some(line) = rx.recv().await {
        // Intercept file attachments.
        if line.starts_with("ATTACH_FILE:") {
            let path_str = line.trim_start_matches("ATTACH_FILE:").trim();
            let path = Path::new(path_str);
            if path.exists() {
                let mut req = bot.send_document(msg.chat.id, InputFile::file(path));
                if let Some(tid) = msg.thread_id {
                    req = req.message_thread_id(tid);
                }
                if let Err(e) = req.await {
                    error!("Failed to send document: {e}");
                }
            }
            continue;
        }

        if !line.is_empty() {
            if !accumulated.is_empty() {
                accumulated.push('\n');
            }
            accumulated.push_str(&line);
        }

        // Refresh typing indicator periodically.
        if last_typing.elapsed() >= TYPING_INTERVAL {
            bot.send_chat_action(msg.chat.id, ChatAction::Typing).await.ok();
            last_typing = Instant::now();
        }

        // Update the message at most once per second.
        if last_update.elapsed() >= UPDATE_INTERVAL && !accumulated.is_empty() {
            let html = format_html(&accumulated);

            if html != last_sent_text {
                bot.edit_message_text(msg.chat.id, placeholder.id, &html)
                    .parse_mode(ParseMode::Html)
                    .await
                    .ok();
                last_sent_text = html;
            }

            last_update = Instant::now();
        }
    }

    // Final edit with the complete response.
    let final_html = if accumulated.is_empty() {
        "(нет ответа)".to_string()
    } else {
        format_final_html(&accumulated)
    };

    bot.edit_message_text(msg.chat.id, placeholder.id, final_html)
        .parse_mode(ParseMode::Html)
        .await
        .ok();

    Ok(())
}

/// Escape text for Telegram HTML parse mode.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Format text as an HTML code block for Telegram (used during streaming).
fn format_html(text: &str) -> String {
    let truncated = truncate_raw_for_html(text);
    let escaped = escape_html(&truncated);
    format!("<pre>{escaped}</pre>")
}

/// Format the final response: thinking in `<pre>`, last line as plain text.
fn format_final_html(text: &str) -> String {
    let truncated = truncate_raw_for_html(text);
    let lines: Vec<&str> = truncated.lines().collect();
    if lines.len() <= 1 {
        // Single line — just plain text, no code block needed.
        return escape_html(&truncated);
    }
    let thinking = lines[..lines.len() - 1].join("\n");
    let answer = lines[lines.len() - 1];
    format!(
        "<pre>{}</pre>\n\n{}",
        escape_html(&thinking),
        escape_html(answer),
    )
}

/// Helper to safely truncate raw text before HTML escaping so we don't
/// exceed Telegram's 4096 char limit after escaping.
pub fn truncate_raw_for_html(text: &str) -> String {
    const MAX_RAW: usize = 3800;
    if text.chars().count() <= MAX_RAW {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX_RAW).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        let text = "Hello, world!";
        assert_eq!(truncate_raw_for_html(text), text);
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_raw_for_html(""), "");
    }

    #[test]
    fn truncate_exactly_at_limit() {
        let text: String = "a".repeat(3800);
        assert_eq!(truncate_raw_for_html(&text), text);
    }

    #[test]
    fn truncate_over_limit() {
        let text: String = "a".repeat(4500);
        let result = truncate_raw_for_html(&text);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 3801); // 3800 chars + 1 '…' char
    }

    #[test]
    fn truncate_unicode_characters() {
        // Each emoji is 1 char. 4001 of them should trigger truncation.
        let text: String = "🎉".repeat(4001);
        let result = truncate_raw_for_html(&text);
        let char_count = result.chars().count();
        assert!(result.ends_with('…'));
        assert_eq!(char_count, 3801);
    }

    #[test]
    fn truncate_over_limit_with_multi_byte_chars() {
        let text = "Привет ".repeat(600); // ~4200 chars
        let result = truncate_raw_for_html(&text);
        assert!(result.ends_with('…'));
        assert!(result.chars().count() <= 3801);
    }
}
