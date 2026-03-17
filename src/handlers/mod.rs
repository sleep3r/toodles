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
    let mut last_line = String::new();
    let mut first_content = true;
    const UPDATE_INTERVAL: Duration = Duration::from_millis(500);
    const TYPING_INTERVAL: Duration = Duration::from_secs(4);
    // Show typing indicator immediately.
    bot.send_chat_action(msg.chat.id, ChatAction::Typing).await.ok();

    // Send instant placeholder so the user sees activity right away.
    let mut placeholder_id: Option<teloxide::types::MessageId> = None;
    {
        let mut req = bot.send_message(msg.chat.id, "⏳");
        if let Some(tid) = msg.thread_id {
            req = req.message_thread_id(tid);
        }
        if let Ok(sent) = req.disable_notification(true).await {
            placeholder_id = Some(sent.id);
        }
    }

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

        // Skip consecutive duplicate non-empty lines (gemini-cli --resume can replay history).
        if line == last_line && !line.is_empty() {
            continue;
        }
        last_line = line.clone();

        // Accumulate with newlines for paragraph separation.
        if !accumulated.is_empty() {
            accumulated.push('\n');
        }
        accumulated.push_str(&line);

        // Refresh typing indicator periodically.
        if last_typing.elapsed() >= TYPING_INTERVAL {
            bot.send_chat_action(msg.chat.id, ChatAction::Typing).await.ok();
            last_typing = Instant::now();
        }

        // Stream updates via sendMessageDraft.
        if last_update.elapsed() >= UPDATE_INTERVAL && !accumulated.is_empty() {
            // On first real content, update the placeholder with actual text.
            if first_content {
                first_content = false;
                if let Some(pid) = placeholder_id {
                    bot.edit_message_text(msg.chat.id, pid, &truncate_text(&accumulated))
                        .await
                        .ok();
                }
                last_update = Instant::now();
                continue;
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
        let edit_res = bot.edit_message_text(msg.chat.id, pid, &final_text)
            .parse_mode(ParseMode::Markdown)
            .await;
        if let Err(e) = edit_res {
            // Markdown parse error — fallback to plain text.
            tracing::warn!("Markdown failed: {e}, falling back to plain text");
            bot.edit_message_text(msg.chat.id, pid, &final_text).await.ok();
        }
    } else {
        // No placeholder was ever sent.
        let send_res = bot.send_message(msg.chat.id, &final_text)
            .parse_mode(ParseMode::Markdown)
            .await;
        if let Err(_) = send_res {
            let mut req = bot.send_message(msg.chat.id, &final_text);
            if let Some(tid) = msg.thread_id {
                req = req.message_thread_id(tid);
            }
            req.await.ok();
        }
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
