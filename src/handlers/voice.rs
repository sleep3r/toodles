use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::Message;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::config::Config;
use crate::session::SessionManager;
use crate::transcription::LocalTranscriber;

use super::message::send_reply;
use super::session_key;

/// Handle a voice message: download the OGG file, transcribe it either
/// locally (Parakeet) or via OpenAI Whisper, and forward the transcription
/// to the gemini-cli session.
pub async fn handle_voice(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    sessions: Arc<SessionManager>,
    local_transcriber: Option<Arc<Mutex<LocalTranscriber>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = match msg.from.as_ref() {
        Some(u) => u.id.0,
        None => return Ok(()),
    };

    if !config.is_user_allowed(user_id) {
        send_reply(&bot, &msg, "⛔ You are not authorised to use this bot.").await?;
        return Ok(());
    }

    let voice = match msg.voice() {
        Some(v) => v,
        None => return Ok(()),
    };

    // Notify the user that we are processing the audio.
    let status = send_reply(&bot, &msg, "🎙 Transcribing voice message…").await?;

    // Download the voice file from Telegram.
    let file = bot.get_file(&voice.file.id).await?;
    let mut audio_bytes = Vec::new();
    bot.download_file(&file.path, &mut audio_bytes).await?;

    // Transcribe: try local first, then fallback to OpenAI Whisper.
    let transcript = if let Some(ref transcriber) = local_transcriber {
        match transcribe_locally(transcriber, &audio_bytes).await {
            Ok(t) => {
                info!("Local transcription succeeded ({} chars)", t.len());
                t
            }
            Err(e) => {
                error!("Local transcription failed, falling back to Whisper: {e}");
                // Try Whisper as fallback
                match &config.openai_api_key {
                    Some(key) => transcribe_with_whisper(key, audio_bytes).await?,
                    None => {
                        bot.edit_message_text(
                            msg.chat.id,
                            status.id,
                            format!("❌ Local transcription failed: {e}"),
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        }
    } else {
        // No local transcriber — use Whisper API
        let api_key = match &config.openai_api_key {
            Some(k) => k.clone(),
            None => {
                send_reply(
                    &bot,
                    &msg,
                    "🎙 Voice messages require either local transcription or `OPENAI_API_KEY`.",
                )
                .await?;
                return Ok(());
            }
        };

        match transcribe_with_whisper(&api_key, audio_bytes).await {
            Ok(t) => t,
            Err(e) => {
                error!("Whisper transcription failed: {e}");
                bot.edit_message_text(
                    msg.chat.id,
                    status.id,
                    format!("❌ Transcription failed: {e}"),
                )
                .await?;
                return Ok(());
            }
        }
    };

    if transcript.is_empty() {
        bot.edit_message_text(
            msg.chat.id,
            status.id,
            "🎙 Could not transcribe audio (empty result).",
        )
        .await?;
        return Ok(());
    }

    // Update status with the transcript, then forward to gemini-cli.
    bot.edit_message_text(
        msg.chat.id,
        status.id,
        format!("🎙 Transcript: {transcript}"),
    )
    .await
    .ok();

    // Reuse the text handler logic by obtaining the session and querying.
    let key = session_key(&msg);
    let session = match sessions.get_or_create(key).await {
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

    let placeholder = send_reply(&bot, &msg, "⏳ Thinking…").await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let session_clone = session.clone();
    let transcript_clone = transcript.clone();
    tokio::spawn(async move {
        let mut sess = session_clone.lock().await;
        if let Err(e) = sess.query_streaming(&transcript_clone, tx).await {
            error!("Session query error: {e}");
        }
    });

    let mut accumulated = String::new();
    let mut last_edit = std::time::Instant::now();
    const MIN_EDIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

    while let Some(line) = rx.recv().await {
        if !line.is_empty() {
            if !accumulated.is_empty() {
                accumulated.push('\n');
            }
            accumulated.push_str(&line);
        }
        if last_edit.elapsed() >= MIN_EDIT_INTERVAL && !accumulated.is_empty() {
            let preview = super::truncate_for_telegram(&accumulated);
            bot.edit_message_text(msg.chat.id, placeholder.id, &preview)
                .await
                .ok();
            last_edit = std::time::Instant::now();
        }
    }

    let final_text = if accumulated.is_empty() {
        "_(no response)_".to_string()
    } else {
        super::truncate_for_telegram(&accumulated)
    };
    bot.edit_message_text(msg.chat.id, placeholder.id, final_text)
        .await
        .ok();

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Transcription backends
// ──────────────────────────────────────────────────────────────────────────────

/// Transcribe using the local Parakeet engine.
async fn transcribe_locally(
    transcriber: &Arc<Mutex<LocalTranscriber>>,
    ogg_bytes: &[u8],
) -> Result<String> {
    // Decode OGG → f32 16kHz (CPU-bound, run in blocking thread)
    let bytes = ogg_bytes.to_vec();
    let samples = tokio::task::spawn_blocking(move || {
        crate::transcription::decode_ogg_to_f32_16khz(&bytes)
    })
    .await
    .context("Decode task panicked")??;

    // Run inference (CPU-bound)
    let transcriber = transcriber.clone();
    let text = tokio::task::spawn_blocking(move || {
        let mut engine = transcriber.blocking_lock();
        engine.transcribe(samples)
    })
    .await
    .context("Transcription task panicked")??;

    Ok(text)
}

#[derive(Deserialize)]
struct WhisperResponse {
    text: String,
}

/// Call the OpenAI Whisper transcriptions endpoint and return the transcript.
async fn transcribe_with_whisper(api_key: &str, audio_bytes: Vec<u8>) -> Result<String> {
    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("voice.ogg")
        .mime_str("audio/ogg")
        .context("Failed to build multipart part")?;

    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {api_key}"))
        .multipart(form)
        .send()
        .await
        .context("Whisper API request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Whisper API returned {status}: {body}");
    }

    let whisper: WhisperResponse = response
        .json()
        .await
        .context("Failed to parse Whisper API response")?;

    Ok(whisper.text.trim().to_string())
}
