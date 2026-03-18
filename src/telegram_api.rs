use anyhow::{Context, Result};
use serde_json::json;
use tracing::debug;

static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

/// Call the `sendMessageDraft` Telegram Bot API method.
///
/// Streams a partial message to the user while it is being generated.
/// Changes of drafts with the same `draft_id` are animated on the client.
///
/// <https://core.telegram.org/bots/api#sendmessagedraft>
#[allow(dead_code)]
pub async fn send_message_draft(
    token: &str,
    chat_id: i64,
    draft_id: i64,
    text: &str,
    thread_id: Option<i32>,
) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessageDraft");

    let mut body = json!({
        "chat_id": chat_id,
        "draft_id": draft_id,
        "text": text,
    });

    if let Some(tid) = thread_id {
        body["message_thread_id"] = json!(tid);
    }

    let resp = HTTP_CLIENT
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("sendMessageDraft request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        debug!("sendMessageDraft failed ({status}): {body}");
        anyhow::bail!("sendMessageDraft returned {status}: {body}");
    }

    Ok(())
}
