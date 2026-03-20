use std::sync::Arc;

use anyhow::Result;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::Message;
use tracing::info;

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

    if is_first {
        super::spawn_drain_task(bot, msg, config, sessions, aggregator, key);
    }

    Ok(())
}
