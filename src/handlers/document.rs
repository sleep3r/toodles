use std::sync::Arc;

use anyhow::Result;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::Message;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::aggregator::{MessageAggregator, MessagePart};
use crate::config::Config;
use crate::session::SessionManager;

use super::session_key;

/// Handle a document message: download the file, then forward the path
/// and caption to gemini-cli.
pub async fn handle_document(
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
        super::message::send_reply(&bot, &msg, "⛔ You are not authorised to use this bot.")
            .await?;
        return Ok(());
    }

    let document = match msg.document() {
        Some(d) => d,
        None => return Ok(()),
    };

    let file_name = document
        .file_name
        .clone()
        .unwrap_or_else(|| "unknown_file".to_string());
    let unique_id = &document.file.unique_id;
    let local_path = format!("/tmp/toodles_{unique_id}_{file_name}");

    // Download the file from Telegram.
    info!("Downloading document: {file_name} → {local_path}");
    let file = bot.get_file(&document.file.id).await?;
    let mut dst = tokio::fs::File::create(&local_path).await?;
    bot.download_file(&file.path, &mut dst).await?;
    info!("Document saved: {local_path}");

    // Build the prompt with file path and caption.
    let caption = msg
        .caption()
        .unwrap_or("No caption provided. Describe the file contents and ask what I should do.");
    let prompt = format!(
        "User sent a file: {file_name} (saved at {local_path})\n\n{caption}"
    );

    // Use aggregation.
    let key = session_key(&msg);
    let is_first = aggregator.push(key, MessagePart { text: prompt });

    if !is_first {
        return Ok(()); // Another handler instance will drain the batch.
    }

    // Wait for the aggregation window.
    tokio::time::sleep(aggregator.window()).await;

    // Drain the batch.
    let combined = match aggregator.take_if_ready(&key) {
        Some(parts) => MessageAggregator::combine(&parts),
        None => {
            // Deadline extended; wait a bit more.
            tokio::time::sleep(aggregator.window()).await;
            match aggregator.take_if_ready(&key) {
                Some(parts) => MessageAggregator::combine(&parts),
                None => return Ok(()),
            }
        }
    };

    let (session, is_new) = match sessions.get_or_create(key).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create session: {e}");
            super::message::send_reply(
                &bot,
                &msg,
                &format!("❌ Could not start gemini-cli: {e}"),
            )
            .await?;
            return Ok(());
        }
    };

    if is_new {
        if let Err(e) = super::warm_up_with_indicator(&bot, &msg, &session).await {
            error!("Session warm-up failed: {e}");
            return Ok(());
        }
    }

    let (tx, rx) = mpsc::channel::<String>(64);
    let session_clone = session.clone();
    tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        if let Err(e) = sess.query(&combined, tx).await {
            error!("Session query error: {e}");
        }
    });

    super::stream_response_with_drafts(&bot, &msg, &config, rx).await?;

    Ok(())
}
