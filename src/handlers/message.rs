use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::Message;
use tokio::sync::mpsc;
use tracing::error;

use crate::config::Config;
use crate::session::SessionManager;

use super::{session_key, truncate_for_telegram};

/// Handle a plain text message: forward it to the user's gemini-cli session
/// and stream the response back, editing the placeholder message in place.
pub async fn handle_text(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = match msg.from.as_ref() {
        Some(u) => u.id.0,
        None => return Ok(()),
    };

    if !config.is_user_allowed(user_id) {
        send_reply(&bot, &msg, "⛔ You are not authorised to use this bot.").await?;
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) if !t.starts_with('/') => t.to_string(),
        _ => return Ok(()),
    };

    let key = session_key(&msg);
    let session = match sessions.get_or_create(key).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create session: {e}");
            bot.send_message(
                msg.chat.id,
                format!(
                    "❌ Could not start gemini-cli.\n\
                     Make sure `{}` is installed and on your PATH.\n\
                     Error: {e}",
                    config.gemini_cli_path
                ),
            )
            .await?;
            return Ok(());
        }
    };

    // Send a placeholder so the user sees immediate feedback.
    let placeholder = send_reply(&bot, &msg, "⏳ Thinking…").await?;

    let (tx, mut rx) = mpsc::channel::<String>(64);

    // Spawn a task that writes to gemini-cli and streams lines back via `tx`.
    let session_clone = session.clone();
    let prompt_clone = text.clone();
    tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        if let Err(e) = sess.query_streaming(&prompt_clone, tx).await {
            error!("Session query error: {e}");
        }
    });

    // Collect streamed lines and periodically edit the placeholder message.
    let mut accumulated = String::new();
    let mut last_edit = Instant::now();
    const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(500);

    while let Some(line) = rx.recv().await {
        if !line.is_empty() {
            if !accumulated.is_empty() {
                accumulated.push('\n');
            }
            accumulated.push_str(&line);
        }

        if last_edit.elapsed() >= MIN_EDIT_INTERVAL && !accumulated.is_empty() {
            let preview = truncate_for_telegram(&accumulated);
            bot.edit_message_text(msg.chat.id, placeholder.id, &preview)
                .await
                .ok();
            last_edit = Instant::now();
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

// ──────────────────────────────────────────────────────────────────────────────
// Helper
// ──────────────────────────────────────────────────────────────────────────────

/// Send a message, preserving the forum topic thread if present.
pub async fn send_reply(bot: &Bot, msg: &Message, text: &str) -> Result<Message> {
    let mut req = bot.send_message(msg.chat.id, text);
    if let Some(tid) = msg.thread_id {
        req = req.message_thread_id(tid);
    }
    Ok(req.await?)
}
