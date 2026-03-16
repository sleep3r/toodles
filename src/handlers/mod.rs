pub mod message;
pub mod voice;

use teloxide::types::Message;

use crate::session::SessionKey;

/// Build the session key from a Telegram message.
///
/// Uses `(chat_id, thread_id)` so that each forum topic gets its own
/// gemini-cli session while still isolating chats from each other.
pub fn session_key(msg: &Message) -> SessionKey {
    // ThreadId(MessageId(i32)) — extract the inner i32
    (msg.chat.id.0, msg.thread_id.map(|t| t.0 .0))
}

/// Truncate text to Telegram's 4096-character message limit, appending an
/// ellipsis when the content is cut.
pub fn truncate_for_telegram(text: &str) -> String {
    const MAX: usize = 4000; // leave headroom for markdown escaping
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}
