pub mod document;
pub mod message;
pub mod photo;
pub mod voice;

/// RAII guard that deletes a temporary file when dropped.
/// Ensures cleanup even on error/panic paths.
pub struct TempFileGuard(pub String);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        // Use sync fs to avoid panicking if Tokio runtime is shutting down.
        std::fs::remove_file(&self.0).ok();
    }
}

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InputFile, Message, ParseMode};
use tracing::error;

use crate::config::Config;
use crate::session::SessionKey;

/// Build the session key from a Telegram message.
///
/// Uses `(chat_id, thread_id)` so that each forum topic gets its own
/// gemini-cli session while still isolating chats from each other.
pub fn session_key(msg: &Message) -> SessionKey {
    // ThreadId(MessageId(i32)) — extract the inner i32
    (msg.chat.id.0, msg.thread_id.map(|t| t.0 .0))
}

/// Stream the gemini-cli response to the user, handling file attachments.
///
/// Edits a placeholder message in-place as lines arrive.
/// Final response committed with HTML formatting.
pub async fn stream_response_with_drafts(
    bot: &Bot,
    msg: &Message,
    _config: &Config,
    mut rx: tokio::sync::mpsc::Receiver<String>,
) -> Result<()> {
    let mut accumulated = String::new();
    let mut last_update = Instant::now();
    let mut last_typing = Instant::now();
    let mut last_line = String::new();
    const UPDATE_INTERVAL: Duration = Duration::from_millis(500);
    const TYPING_INTERVAL: Duration = Duration::from_secs(4);
    // Show typing indicator immediately.
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await
        .ok();

    // Send instant placeholder so the user sees activity right away.
    let mut placeholder_id: Option<teloxide::types::MessageId> = None;
    {
        let mut req = bot.send_message(msg.chat.id, "⏳");
        if let Some(tid) = msg.thread_id {
            req = req.message_thread_id(tid);
        }
        if let Ok(sent) = req.disable_notification(true).await {
            placeholder_id = Some(sent.id);
        }
    }

    while let Some(line) = rx.recv().await {
        // Intercept file attachments.
        if line.starts_with("ATTACH_FILE:") {
            let path_str = line.trim_start_matches("ATTACH_FILE:").trim();
            let path = Path::new(path_str);
            if path.exists() {
                let mut req = bot.send_document(msg.chat.id, InputFile::file(path));
                if let Some(tid) = msg.thread_id {
                    req = req.message_thread_id(tid);
                }
                if let Err(e) = req.await {
                    error!("Failed to send document: {e}");
                }
            }
            continue;
        }

        // Skip consecutive duplicate non-empty lines (gemini-cli --resume can replay history).
        if line == last_line && !line.is_empty() {
            continue;
        }
        last_line = line.clone();

        // Accumulate with newlines for paragraph separation.
        if !accumulated.is_empty() {
            accumulated.push('\n');
        }
        accumulated.push_str(&line);

        // Refresh typing indicator periodically.
        if last_typing.elapsed() >= TYPING_INTERVAL {
            bot.send_chat_action(msg.chat.id, ChatAction::Typing)
                .await
                .ok();
            last_typing = Instant::now();
        }

        // Stream updates by editing the placeholder message.
        if last_update.elapsed() >= UPDATE_INTERVAL && !accumulated.is_empty() {
            if let Some(pid) = placeholder_id {
                bot.edit_message_text(msg.chat.id, pid, &truncate_text(&accumulated))
                    .await
                    .ok();
            }
            last_update = Instant::now();
        }
    }

    // Final edit — commit the complete response as a persistent message.
    let final_text = if accumulated.is_empty() {
        "(no response)".to_string()
    } else {
        let html = markdown_to_telegram_html(&accumulated);
        truncate_text(&html)
    };

    if let Some(pid) = placeholder_id {
        let edit_res = bot
            .edit_message_text(msg.chat.id, pid, &final_text)
            .parse_mode(ParseMode::Html)
            .await;
        if let Err(e) = edit_res {
            // HTML parse error — fallback to plain text.
            tracing::warn!("HTML format failed: {e}, falling back to plain text");
            let plain = truncate_text(&accumulated);
            bot.edit_message_text(msg.chat.id, pid, &plain).await.ok();
        }
    } else {
        // No placeholder was ever sent.
        let send_res = bot
            .send_message(msg.chat.id, &final_text)
            .parse_mode(ParseMode::Html)
            .await;
        if let Err(_) = send_res {
            let plain = truncate_text(&accumulated);
            let mut req = bot.send_message(msg.chat.id, &plain);
            if let Some(tid) = msg.thread_id {
                req = req.message_thread_id(tid);
            }
            req.await.ok();
        }
    }

    Ok(())
}

/// Convert standard Markdown (as emitted by gemini-cli) to Telegram-compatible HTML.
///
/// Telegram supports: `<b>`, `<i>`, `<code>`, `<pre>`, `<a href="...">`, `<s>`, `<blockquote>`.
/// Standard Markdown features like `###` headers or `* ` bullets are NOT supported natively.
fn markdown_to_telegram_html(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 256);
    let mut in_code_block = false;
    #[allow(unused_assignments)]
    let mut code_lang = String::new();

    for line in md.split('\n') {
        // Toggle fenced code blocks (``` or ```rust)
        if line.starts_with("```") {
            if in_code_block {
                out.push_str("</code></pre>\n");
                in_code_block = false;
            } else {
                code_lang = line.trim_start_matches('`').trim().to_string();
                if code_lang.is_empty() {
                    out.push_str("<pre><code>");
                } else {
                    out.push_str(&format!("<pre><code class=\"language-{code_lang}\">"));
                }
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            // Inside code blocks: only escape HTML entities, no formatting.
            out.push_str(&escape_html(line));
            out.push('\n');
            continue;
        }

        // Headers → bold text
        let processed = if let Some(header) = line.strip_prefix("### ") {
            format!("\n<b>{}</b>", escape_html(header.trim()))
        } else if let Some(header) = line.strip_prefix("## ") {
            format!("\n<b>{}</b>", escape_html(header.trim()))
        } else if let Some(header) = line.strip_prefix("# ") {
            format!("\n<b>{}</b>", escape_html(header.trim()))
        }
        // Bullet lists → •
        else if let Some(rest) = line.strip_prefix("* ") {
            format!("• {}", format_inline(&escape_html(rest)))
        } else if let Some(rest) = line.strip_prefix("- ") {
            format!("• {}", format_inline(&escape_html(rest)))
        }
        // Numbered lists — pass through with inline formatting
        else if line.chars().next().map_or(false, |c| c.is_ascii_digit()) && line.contains(". ") {
            format_inline(&escape_html(line))
        }
        // Horizontal rules
        else if line.trim() == "---" || line.trim() == "***" {
            "—————".to_string()
        }
        // Blockquotes
        else if let Some(rest) = line.strip_prefix("> ") {
            format!(
                "<blockquote>{}</blockquote>",
                format_inline(&escape_html(rest))
            )
        }
        // Regular text
        else {
            format_inline(&escape_html(line))
        };

        out.push_str(&processed);
        out.push('\n');
    }

    // Close unclosed code block
    if in_code_block {
        out.push_str("</code></pre>\n");
    }

    // Trim trailing newlines
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Escape HTML special characters.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Apply inline Markdown formatting to already-escaped HTML text.
/// Handles: **bold**, *italic*, `code`, [text](url)
fn format_inline(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Inline code: `text`
        if chars[i] == '`' {
            if let Some(end) = find_closing(&chars, i + 1, '`') {
                result.push_str("<code>");
                let code: String = chars[i + 1..end].iter().collect();
                result.push_str(&code);
                result.push_str("</code>");
                i = end + 1;
                continue;
            }
        }
        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_double_closing(&chars, i + 2, '*') {
                result.push_str("<b>");
                let inner: String = chars[i + 2..end].iter().collect();
                result.push_str(&inner);
                result.push_str("</b>");
                i = end + 2;
                continue;
            }
        }
        // Italic: *text* (single asterisk, not at start of bullet)
        if chars[i] == '*' && i + 1 < len && chars[i + 1] != ' ' {
            if let Some(end) = find_closing(&chars, i + 1, '*') {
                result.push_str("<i>");
                let inner: String = chars[i + 1..end].iter().collect();
                result.push_str(&inner);
                result.push_str("</i>");
                i = end + 1;
                continue;
            }
        }
        // Links: [text](url)
        if chars[i] == '[' {
            if let Some((text_end, url_start, url_end)) = find_link(&chars, i) {
                let link_text: String = chars[i + 1..text_end].iter().collect();
                let url: String = chars[url_start..url_end].iter().collect();
                result.push_str(&format!("<a href=\"{url}\">{link_text}</a>"));
                i = url_end + 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

fn find_closing(chars: &[char], start: usize, marker: char) -> Option<usize> {
    for i in start..chars.len() {
        if chars[i] == marker {
            return Some(i);
        }
    }
    None
}

fn find_double_closing(chars: &[char], start: usize, marker: char) -> Option<usize> {
    for i in start..chars.len().saturating_sub(1) {
        if chars[i] == marker && chars[i + 1] == marker {
            return Some(i);
        }
    }
    None
}

fn find_link(chars: &[char], start: usize) -> Option<(usize, usize, usize)> {
    // Find ]( after [
    let text_end = find_closing(chars, start + 1, ']')?;
    if text_end + 1 < chars.len() && chars[text_end + 1] == '(' {
        let url_start = text_end + 2;
        let url_end = find_closing(chars, url_start, ')')?;
        Some((text_end, url_start, url_end))
    } else {
        None
    }
}

/// Truncate text to Telegram's 4096-character limit.
fn truncate_text(text: &str) -> String {
    const MAX: usize = 4096;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        let text = "Hello, world!";
        assert_eq!(truncate_text(text), text);
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_text(""), "");
    }

    #[test]
    fn truncate_exactly_at_limit() {
        let text: String = "a".repeat(4096);
        assert_eq!(truncate_text(&text), text);
    }

    #[test]
    fn truncate_over_limit() {
        let text: String = "a".repeat(5000);
        let result = truncate_text(&text);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 4096);
    }

    #[test]
    fn truncate_unicode_characters() {
        let text: String = "🎉".repeat(5000);
        let result = truncate_text(&text);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 4096);
    }
}
