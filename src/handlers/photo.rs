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

use super::message::send_reply;
use super::session_key;

/// Handle a photo message: download the image, aggregate with album siblings,
/// and pass to gemini-cli for analysis.
pub async fn handle_photo(
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

    let photos = match msg.photo() {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(()),
    };

    // Pick the largest resolution photo.
    let photo = photos.last().unwrap();
    let caption = msg.caption().unwrap_or("What's in this image?");

    // Download the photo from Telegram.
    let file = bot.get_file(&photo.file.id).await?;
    let mut photo_bytes = Vec::new();
    bot.download_file(&file.path, &mut photo_bytes).await?;

    // Determine extension from the file path.
    let ext = file.path.rsplit('.').next().unwrap_or("jpg");

    // Save to ~/.gemini/tmp/toodles (gemini-cli allowed temp directory).
    let home_dir = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let temp_dir = home_dir.join(".gemini/tmp/toodles");
    tokio::fs::create_dir_all(&temp_dir).await.ok();
    let file_name = format!("toodles_photo_{}.{ext}", msg.id.0);
    let file_path = temp_dir.join(file_name).to_string_lossy().to_string();
    tokio::fs::write(&file_path, &photo_bytes).await?;

    info!("Saved photo to {file_path} ({} bytes)", photo_bytes.len());

    // Wrap in Arc so the guard can be shared via the aggregator.
    let guard = Arc::new(super::TempFileGuard(file_path.clone()));

    let prompt = caption.to_string();

    // Use aggregation to batch album photos into a single query.
    let key = session_key(&msg);
    let is_first = aggregator.push(key, MessagePart {
        text: prompt,
        files: vec![file_path.clone()],
        _guards: vec![guard],
    });

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
            send_reply(
                &bot,
                &msg,
                &format!("❌ Could not start gemini-cli: {e}"),
            )
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

    // Stream the response.
    super::stream_response_with_drafts(&bot, &msg, &config, rx).await?;

    Ok(())
}
