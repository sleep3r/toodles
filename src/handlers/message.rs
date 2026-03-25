use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::Message;

use crate::aggregator::{MessageAggregator, MessagePart};
use crate::config::Config;
use crate::session::SessionManager;

use super::{session_key, CancelRegistry};

/// Handle a plain text message: aggregate with nearby messages, then forward
/// to the user's gemini-cli session and stream the response back.
pub async fn handle_text(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
    aggregator: Arc<MessageAggregator>,
    cancel_registry: CancelRegistry,
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
    let is_first = aggregator.push(
        key,
        MessagePart {
            text,
            files: vec![],
            _guards: vec![],
        },
    );

    if is_first {
        super::spawn_drain_task(bot, msg, config, sessions, aggregator, key, cancel_registry);
    }

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
