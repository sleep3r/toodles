use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::session::SessionKey;

/// A single part of an aggregated message batch.
pub struct MessagePart {
    pub text: String,
    pub files: Vec<String>,
    /// RAII guards keeping temp files alive until the batch is processed.
    pub _guards: Vec<Arc<crate::handlers::TempFileGuard>>,
}

/// Buffered state for a pending aggregation.
struct AggregationState {
    parts: Vec<MessagePart>,
    deadline: Instant,
}

/// Aggregates sequential messages arriving within a short window
/// before forwarding them to gemini-cli as a single prompt.
///
/// This handles:
/// - Forwarded message batches
/// - Long messages split by Telegram into multiple parts
pub struct MessageAggregator {
    pending: DashMap<SessionKey, AggregationState>,
    window: Duration,
}

impl MessageAggregator {
    pub fn new(window: Duration) -> Self {
        Self {
            pending: DashMap::new(),
            window,
        }
    }

    /// Push a message part into the aggregation buffer.
    ///
    /// Returns `true` if this is the first part (caller should spawn the
    /// drain task), or `false` if appended to an existing batch.
    pub fn push(&self, key: SessionKey, part: MessagePart) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.pending.entry(key) {
            Entry::Occupied(mut e) => {
                let state = e.get_mut();
                state.parts.push(part);
                state.deadline = Instant::now() + self.window;
                false
            }
            Entry::Vacant(e) => {
                e.insert(AggregationState {
                    parts: vec![part],
                    deadline: Instant::now() + self.window,
                });
                true
            }
        }
    }


    /// Take the aggregated parts if the deadline has passed.
    /// Returns `None` if there's still time left or no batch exists.
    pub fn take_if_ready(&self, key: &SessionKey) -> Option<Vec<MessagePart>> {
        self.pending
            .remove_if(key, |_, state| Instant::now() >= state.deadline)
            .map(|(_, state)| state.parts)
    }

    /// Return the remaining time until the aggregation deadline for a key.
    /// Returns `None` if no batch exists for this key.
    pub fn wait_deadline(&self, key: &SessionKey) -> Option<Duration> {
        self.pending.get(key).map(|state| {
            let now = Instant::now();
            if state.deadline > now {
                state.deadline - now
            } else {
                Duration::ZERO
            }
        })
    }

    /// Combine message parts into a single prompt string, file paths, and guards.
    pub fn combine(parts: &[MessagePart]) -> (String, Vec<String>, Vec<Arc<crate::handlers::TempFileGuard>>) {
        let text = if parts.len() == 1 {
            parts[0].text.clone()
        } else {
            parts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        let files: Vec<String> = parts.iter().flat_map(|p| p.files.clone()).collect();
        let guards: Vec<Arc<crate::handlers::TempFileGuard>> = parts.iter().flat_map(|p| p._guards.clone()).collect();
        (text, files, guards)
    }

    /// The aggregation window duration.
    #[allow(dead_code)]
    pub fn window(&self) -> Duration {
        self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_part(text: &str) -> MessagePart {
        MessagePart {
            text: text.to_string(),
            files: vec![],
            _guards: vec![],
        }
    }

    #[test]
    fn push_first_returns_true() {
        let agg = MessageAggregator::new(Duration::from_secs(1));
        let key: SessionKey = (123, None);
        assert!(agg.push(key, make_part("hello")));
    }

    #[test]
    fn push_second_returns_false() {
        let agg = MessageAggregator::new(Duration::from_secs(1));
        let key: SessionKey = (123, None);
        assert!(agg.push(key, make_part("hello")));
        assert!(!agg.push(key, make_part("world")));
    }

    #[test]
    fn take_before_deadline_returns_none() {
        let agg = MessageAggregator::new(Duration::from_secs(60));
        let key: SessionKey = (123, None);
        agg.push(key, make_part("hello"));
        assert!(agg.take_if_ready(&key).is_none());
    }

    #[tokio::test]
    async fn take_after_deadline_returns_parts() {
        let agg = MessageAggregator::new(Duration::from_millis(50));
        let key: SessionKey = (123, None);
        agg.push(key, make_part("hello"));
        agg.push(key, make_part("world"));

        tokio::time::sleep(Duration::from_millis(100)).await;

        let parts = agg.take_if_ready(&key).expect("should be ready");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text, "hello");
        assert_eq!(parts[1].text, "world");
    }

    #[tokio::test]
    async fn take_clears_pending_entry() {
        let agg = MessageAggregator::new(Duration::from_millis(50));
        let key: SessionKey = (123, None);
        agg.push(key, make_part("hello"));

        tokio::time::sleep(Duration::from_millis(100)).await;
        agg.take_if_ready(&key);

        // Second take should return None (entry removed).
        assert!(agg.take_if_ready(&key).is_none());
    }

    #[test]
    fn take_nonexistent_key_returns_none() {
        let agg = MessageAggregator::new(Duration::from_millis(50));
        let key: SessionKey = (999, Some(42));
        assert!(agg.take_if_ready(&key).is_none());
    }

    #[test]
    fn combine_single_part() {
        let parts = vec![make_part("only one")];
        let (text, files, guards) = MessageAggregator::combine(&parts);
        assert_eq!(text, "only one");
        assert!(files.is_empty());
        assert!(guards.is_empty());
    }

    #[test]
    fn combine_multiple_parts() {
        let parts = vec![
            make_part("first"),
            make_part("second"),
            make_part("third"),
        ];
        let (text, files, guards) = MessageAggregator::combine(&parts);
        assert_eq!(text, "first\n\nsecond\n\nthird");
        assert!(files.is_empty());
        assert!(guards.is_empty());
    }

    #[test]
    fn different_keys_are_isolated() {
        let agg = MessageAggregator::new(Duration::from_secs(60));
        let key_a: SessionKey = (100, None);
        let key_b: SessionKey = (200, None);

        assert!(agg.push(key_a, make_part("a1")));
        assert!(agg.push(key_b, make_part("b1")));
        // Second push to key_a should not be first.
        assert!(!agg.push(key_a, make_part("a2")));
    }

    #[test]
    fn thread_id_creates_separate_keys() {
        let agg = MessageAggregator::new(Duration::from_secs(60));
        let key_no_thread: SessionKey = (100, None);
        let key_with_thread: SessionKey = (100, Some(42));

        assert!(agg.push(key_no_thread, make_part("no thread")));
        assert!(agg.push(key_with_thread, make_part("with thread")));
    }

    #[tokio::test]
    async fn deadline_extends_with_new_parts() {
        let agg = MessageAggregator::new(Duration::from_millis(100));
        let key: SessionKey = (123, None);

        agg.push(key, make_part("first"));

        // After 60ms, push another part (extends deadline by another 100ms).
        tokio::time::sleep(Duration::from_millis(60)).await;
        agg.push(key, make_part("second"));

        // At 80ms from first push, original deadline would have passed,
        // but the extended deadline has not.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(agg.take_if_ready(&key).is_none());

        // After the extended deadline.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let parts = agg.take_if_ready(&key).expect("should be ready now");
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn window_returns_configured_duration() {
        let agg = MessageAggregator::new(Duration::from_millis(1500));
        assert_eq!(agg.window(), Duration::from_millis(1500));
    }
}
