pub mod message;
pub mod voice;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{InputFile, Message};
use tokio::sync::Mutex;
use tracing::{error, warn};

use crate::config::Config;
use crate::session::{Session, SessionKey};
use crate::telegram_api;

/// Build the session key from a Telegram message.
///
/// Uses `(chat_id, thread_id)` so that each forum topic gets its own
/// gemini-cli session while still isolating chats from each other.
pub fn session_key(msg: &Message) -> SessionKey {
    // ThreadId(MessageId(i32)) — extract the inner i32
    (msg.chat.id.0, msg.thread_id.map(|t| t.0 .0))
}

/// Extract the raw thread ID as `Option<i32>` for API calls.
fn thread_id_i32(msg: &Message) -> Option<i32> {
    msg.thread_id.map(|t| t.0 .0)
}

/// Truncate text to Telegram's 4096-character message limit, appending an
/// ellipsis when the content is cut.
pub fn truncate_for_telegram(text: &str) -> String {
    const MAX: usize = 4000; // leave headroom for markdown escaping
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX).collect();
        format!("{truncated}…")
    }
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

    let placeholder = send_reply(bot, msg, "🚀 Starting Gemini session…").await?;

    // Spawn warm-up in a background task so we can animate concurrently.
    let session_clone = session.clone();
    let mut warmup_handle = tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        sess.warm_up().await
    });

    // Animate the placeholder while warm-up runs.
    let dots = [".", "..", "..."];
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
                            "✅ Session ready!",
                        ).await.ok();
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        error!("Warm-up failed: {e}");
                        bot.edit_message_text(
                            msg.chat.id,
                            placeholder.id,
                            format!("❌ Failed to start session: {e}"),
                        ).await.ok();
                        return Err(e);
                    }
                    Err(join_err) => {
                        error!("Warm-up task panicked: {join_err}");
                        bot.edit_message_text(
                            msg.chat.id,
                            placeholder.id,
                            "❌ Warm-up task crashed",
                        ).await.ok();
                        anyhow::bail!("warm-up task panicked");
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(1500)) => {
                // Update the animation.
                let suffix = dots[tick % dots.len()];
                tick += 1;
                bot.edit_message_text(
                    msg.chat.id,
                    placeholder.id,
                    format!("🚀 Starting Gemini session{suffix}"),
                ).await.ok();
            }
        }
    }
}

/// Stream the gemini-cli response to the user, handling file attachments.
///
/// Strategy (hybrid):
/// 1. Send a placeholder message `⏳ Thinking…`
/// 2. Try `sendMessageDraft` for animated streaming
/// 3. If drafts fail → fall back to `edit_message_text` on the placeholder
/// 4. Final response → always `edit_message_text` on the placeholder
pub async fn stream_response_with_drafts(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    mut rx: tokio::sync::mpsc::Receiver<String>,
) -> Result<()> {
    use message::send_reply;

    let chat_id = msg.chat.id.0;
    let thread_id = thread_id_i32(msg);
    let token = &config.telegram_bot_token;

    // Always send a placeholder so the user sees immediate feedback.
    let placeholder = send_reply(bot, msg, "⏳ Thinking…").await?;

    // Use a unique draft_id per response (timestamp-based).
    let draft_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let mut accumulated = String::new();
    let mut last_update = Instant::now();
    let mut use_drafts = true; // optimistic; flipped on first failure
    const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(400);

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
                    bot.send_message(msg.chat.id, format!("❌ Failed to send file: {e}"))
                        .await
                        .ok();
                }
            } else {
                bot.send_message(msg.chat.id, format!("❌ File not found: {path_str}"))
                    .await
                    .ok();
            }
            continue;
        }

        if !line.is_empty() {
            if !accumulated.is_empty() {
                accumulated.push('\n');
            }
            accumulated.push_str(&line);
        }

        // Send streaming updates at a reasonable rate.
        if last_update.elapsed() >= MIN_UPDATE_INTERVAL && !accumulated.is_empty() {
            let preview = truncate_for_telegram(&accumulated);

            if use_drafts {
                match telegram_api::send_message_draft(
                    token, chat_id, draft_id, &preview, thread_id,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        warn!("sendMessageDraft not supported, falling back to edit: {e}");
                        use_drafts = false;
                        // Immediate fallback edit.
                        bot.edit_message_text(msg.chat.id, placeholder.id, &preview)
                            .await
                            .ok();
                    }
                }
            } else {
                bot.edit_message_text(msg.chat.id, placeholder.id, &preview)
                    .await
                    .ok();
            }

            last_update = Instant::now();
        }
    }

    // Final edit with the complete response.
    let final_text = if accumulated.is_empty() {
        "_(no response)_".to_string()
    } else {
        truncate_for_telegram(&accumulated)
    };

    bot.edit_message_text(msg.chat.id, placeholder.id, final_text)
        .await
        .ok();

    Ok(())
}

