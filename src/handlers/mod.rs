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

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InputFile, Message, ParseMode};
use tracing::error;

use crate::config::Config;
use crate::session::SessionKey;
use crate::telegram_api;


/// Build the session key from a Telegram message.
///
/// Uses `(chat_id, thread_id)` so that each forum topic gets its own
/// gemini-cli session while still isolating chats from each other.
pub fn session_key(msg: &Message) -> SessionKey {
    // ThreadId(MessageId(i32)) — extract the inner i32
    (msg.chat.id.0, msg.thread_id.map(|t| t.0 .0))
}



/// Stream the gemini-cli response to the user, handling file attachments.
///
/// Uses `sendMessageDraft` for animated streaming (plain text).
/// Final response committed via `edit_message_text`.
pub async fn stream_response_with_drafts(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    mut rx: tokio::sync::mpsc::Receiver<String>,
) -> Result<()> {
    let chat_id = msg.chat.id.0;
    let thread_id = msg.thread_id.map(|t| t.0 .0);
    let token = &config.telegram_bot_token;

    // Unique draft_id for this response.
    let draft_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let mut accumulated = String::new();
    let mut last_update = Instant::now();
    let mut last_typing = Instant::now();
    let mut placeholder_id: Option<teloxide::types::MessageId> = None;
    let mut last_line = String::new();
    const UPDATE_INTERVAL: Duration = Duration::from_millis(500);
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

        // Skip consecutive duplicate lines (gemini-cli --resume can replay history).
        if line == last_line {
            continue;
        }
        last_line = line.clone();

        if !accumulated.is_empty() {
            accumulated.push_str("\n\n");
        }
        accumulated.push_str(&line);

        // Refresh typing indicator periodically.
        if last_typing.elapsed() >= TYPING_INTERVAL {
            bot.send_chat_action(msg.chat.id, ChatAction::Typing).await.ok();
            last_typing = Instant::now();
        }

        // Stream updates via sendMessageDraft.
        if last_update.elapsed() >= UPDATE_INTERVAL && !accumulated.is_empty() {
            // Send a placeholder on first real content so we have a message to edit later.
            if placeholder_id.is_none() {
                let mut req = bot.send_message(msg.chat.id, &truncate_text(&accumulated));
                if let Some(tid) = msg.thread_id {
                    req = req.message_thread_id(tid);
                }
                if let Ok(sent) = req.disable_notification(true).await {
                    placeholder_id = Some(sent.id);
                }
                last_update = Instant::now();
                continue; // placeholder already shows text, skip draft this iteration
            }

            let draft_text = truncate_text(&accumulated);
            telegram_api::send_message_draft(
                token, chat_id, draft_id, &draft_text, thread_id,
            )
            .await
            .ok();

            last_update = Instant::now();
        }
    }

    // Final edit — commit the complete response as a persistent message.
    let final_text = if accumulated.is_empty() {
        "(no response)".to_string()
    } else {
        truncate_text(&accumulated)
    };

    #[allow(deprecated)]
    if let Some(pid) = placeholder_id {
        bot.edit_message_text(msg.chat.id, pid, &final_text)
            .parse_mode(ParseMode::Markdown)
            .await
            .ok();
    } else {
        // No draft was ever sent (very fast or empty response).
        let mut req = bot.send_message(msg.chat.id, &final_text)
            .parse_mode(ParseMode::Markdown);
        if let Some(tid) = msg.thread_id {
            req = req.message_thread_id(tid);
        }
        req.await.ok();
    }

    // Clear the draft so text doesn't linger in the user's input field.
    telegram_api::send_message_draft(token, chat_id, draft_id, "", thread_id)
        .await
        .ok();

    Ok(())
}

/// Truncate text to Telegram's 4096-character limit.
fn truncate_text(text: &str) -> String {
    const MAX: usize = 4096;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    }
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
}
