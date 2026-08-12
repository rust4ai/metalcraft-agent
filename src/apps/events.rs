//! [`AppEventHub`] — the per-app publish/subscribe hub.
//!
//! Drives WebSocket push to an app's embedded web UI (e.g. a note upserted in
//! one browser tab appears live in another). Because a managed pod serves one
//! user, this is a single in-process `broadcast` channel — no per-user keying,
//! no Redis, no cross-process fan-out.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::sync::broadcast;

/// Capacity of the broadcast ring buffer. Slow subscribers that lag past this
/// receive a `RecvError::Lagged` and resync from REST — acceptable for a UI hub.
const CAPACITY: usize = 256;

/// Process-global hubs keyed by app id, so an app's mutating tools and its
/// `/ws` subscribers share one channel even though they build separate
/// [`super::AppContext`]s. Without this, a tool write would never reach a
/// WebSocket opened through the app's router.
static HUBS: OnceLock<Mutex<HashMap<String, AppEventHub>>> = OnceLock::new();

/// A cloneable handle to one app's event stream.
#[derive(Clone)]
pub struct AppEventHub {
    tx: broadcast::Sender<serde_json::Value>,
}

impl AppEventHub {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CAPACITY);
        Self { tx }
    }

    /// Publish an event to all current subscribers. No-op if there are none.
    pub fn publish(&self, event: serde_json::Value) {
        let _ = self.tx.send(event);
    }

    /// Subscribe to the event stream (e.g. from a WebSocket handler).
    pub fn subscribe(&self) -> broadcast::Receiver<serde_json::Value> {
        self.tx.subscribe()
    }

    /// The process-global hub for `app_id` (created on first use). All callers
    /// for the same app share one channel.
    pub fn shared(app_id: &str) -> AppEventHub {
        let map = HUBS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
        guard.entry(app_id.to_string()).or_insert_with(AppEventHub::new).clone()
    }
}

impl Default for AppEventHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_reaches_subscriber() {
        let hub = AppEventHub::new();
        let mut rx = hub.subscribe();
        hub.publish(serde_json::json!({"kind": "ping"}));
        let got = rx.recv().await.unwrap();
        assert_eq!(got["kind"], "ping");
    }

    #[test]
    fn publish_with_no_subscribers_is_ok() {
        let hub = AppEventHub::new();
        hub.publish(serde_json::json!({"kind": "noop"}));
    }
}
