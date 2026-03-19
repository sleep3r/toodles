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

    let home_dir = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let temp_dir = home_dir.join(".gemini/tmp/toodles");
    tokio::fs::create_dir_all(&temp_dir).await.ok();

    let local_path = temp_dir
        .join(format!("toodles_{unique_id}_{file_name}"))
        .to_string_lossy()
        .to_string();

    // Download the file from Telegram.
    info!("Downloading document: {file_name} → {local_path}");
    let file = bot.get_file(&document.file.id).await?;
    let mut dst = tokio::fs::File::create(&local_path).await?;
    bot.download_file(&file.path, &mut dst).await?;
    info!("Document saved: {local_path}");

    // Wrap in Arc so the guard can be shared via the aggregator.
    let guard = Arc::new(super::TempFileGuard(local_path.clone()));

    // Build the prompt with file path and caption.
    let caption = msg
        .caption()
        .unwrap_or("No caption provided. Describe the file contents and ask what I should do.");
    let prompt = format!("User sent a file: {file_name} (saved at {local_path})\n\n{caption}");

    // Use aggregation.
    let key = session_key(&msg);
    let is_first = aggregator.push(
        key,
        MessagePart {
            text: prompt,
            files: vec![local_path.clone()],
            _guards: vec![guard],
        },
    );

    if !is_first {
        return Ok(()); // Another handler instance will drain the batch.
    }

    let combined = loop {
        if let Some(parts) = aggregator.take_if_ready(&key) {
            break MessageAggregator::combine(&parts);
        }
        match aggregator.wait_deadline(&key) {
            Some(d) if !d.is_zero() => tokio::time::sleep(d).await,
            Some(_) => continue,
            None => return Ok(()),
        }
    };
    let (combined, combined_files, _guards) = combined;

    let (session, _is_new) = match sessions.get_or_create(key).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create session: {e}");
            super::message::send_reply(&bot, &msg, &format!("❌ Could not start gemini-cli: {e}"))
                .await?;
            return Ok(());
        }
    };

    let (tx, rx) = mpsc::channel::<String>(64);
    let session_clone = session.clone();
    tokio::spawn(async move {
        let _g = _guards; // Keep all temp file guards alive.
        let mut sess = session_clone.lock().await;
        if let Err(e) = sess.query_with_files(&combined, &combined_files, tx).await {
            error!("Session query error: {e}");
        }
    });

    super::stream_response_with_drafts(&bot, &msg, &config, rx).await?;

    Ok(())
}
