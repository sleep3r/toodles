use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::Message;
use tokio::sync::mpsc;
use tracing::error;

use crate::config::Config;
use crate::session::SessionManager;

use super::session_key;

/// Handle a plain text message: forward it to the user's gemini-cli session
/// and stream the response back via `sendMessageDraft`.
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
    let (session, is_new) = match sessions.get_or_create(key).await {
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

    // Warm up new sessions with an animated indicator.
    if is_new {
        if let Err(e) = super::warm_up_with_indicator(&bot, &msg, &session).await {
            error!("Session warm-up failed: {e}");
            return Ok(());
        }
    }

    let (tx, rx) = mpsc::channel::<String>(64);

    // Spawn a task that writes to gemini-cli and streams lines back via `tx`.
    let session_clone = session.clone();
    let prompt_clone = text.clone();
    tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        if let Err(e) = sess.query(&prompt_clone, tx).await {
            error!("Session query error: {e}");
        }
    });

    // Stream the response via sendMessageDraft.
    super::stream_response_with_drafts(&bot, &msg, &config, rx).await?;

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Send a message, preserving the forum topic thread if present.
pub async fn send_reply(bot: &Bot, msg: &Message, text: &str) -> Result<Message> {
    let mut req = bot.send_message(msg.chat.id, text);
    if let Some(tid) = msg.thread_id {
        req = req.message_thread_id(tid);
    }
    Ok(req.await?)
}
