use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::Message;
use tokio::sync::mpsc;
use tracing::error;

use crate::aggregator::{MessageAggregator, MessagePart};
use crate::config::Config;
use crate::session::SessionManager;

use super::session_key;

/// Handle a plain text message: aggregate with nearby messages, then forward
/// to the user's gemini-cli session and stream the response back.
pub async fn handle_text(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
    aggregator: Arc<MessageAggregator>,
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
    let is_first = aggregator.push(key, MessagePart { text, files: vec![] });

    if !is_first {
        // Another handler instance is already waiting; our part was appended.
        return Ok(());
    }

    let combined = loop {
        if let Some(parts) = aggregator.take_if_ready(&key) {
            break MessageAggregator::combine(&parts);
        }
        match aggregator.wait_deadline(&key) {
            Some(d) if !d.is_zero() => tokio::time::sleep(d).await,
            Some(_) => continue, // deadline just passed, try take again
            None => return Ok(()), // batch was taken by another task
        }
    };
    let (combined, combined_files) = combined;

    let (session, _is_new) = match sessions.get_or_create(key).await {
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

    let (tx, rx) = mpsc::channel::<String>(64);

    let session_clone = session.clone();
    tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        if combined_files.is_empty() {
            if let Err(e) = sess.query(&combined, tx).await {
                error!("Session query error: {e}");
            }
        } else {
            if let Err(e) = sess.query_with_files(&combined, &combined_files, tx).await {
                error!("Session query error: {e}");
            }
        }
    });

    // Stream the response.
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
