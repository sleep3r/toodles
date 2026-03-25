use std::sync::Arc;

use anyhow::Result;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::Message;
use tracing::info;

use crate::aggregator::{MessageAggregator, MessagePart};
use crate::config::Config;
use crate::session::SessionManager;

use super::message::send_reply;
use super::{session_key, CancelRegistry};

/// Handle a photo message: download the image, aggregate with album siblings,
/// and pass to gemini-cli for analysis.
pub async fn handle_photo(
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
    let is_first = aggregator.push(
        key,
        MessagePart {
            text: prompt,
            files: vec![file_path.clone()],
            _guards: vec![guard],
        },
    );

    if is_first {
        super::spawn_drain_task(bot, msg, config, sessions, aggregator, key, cancel_registry);
    }

    Ok(())
}
