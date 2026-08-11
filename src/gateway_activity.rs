//! Gateway activity log — a persistent, append-only record of inbound and
//! outbound gateway traffic.
//!
//! Inbound webhooks and outbound `gateway_send_message` calls are otherwise
//! fire-and-forget (no diagnostics session is created for them), so without this
//! there is no way to see what a channel has received/sent — or, crucially, to
//! see inbound messages that matched *no* channel and were silently dropped.
//!
//! Records are appended as newline-delimited JSON to
//! `<DATA_DIR>/gateway_activity.jsonl`. Reads return the most recent records,
//! optionally filtered to a single channel. Recording is best-effort: a failure
//! to write the log must never break message handling, so errors are logged and
//! swallowed.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};

use serde::{Deserialize, Serialize};

use crate::paths::data_dir;

/// Max characters of message body kept in the log. Bodies are truncated so the
/// activity log stays compact and never stores large payloads.
const MAX_BODY_CHARS: usize = 500;

/// One inbound or outbound gateway event.
///
/// `channel_id` is `None` when an inbound message matched no enabled channel —
/// these "unrouted" records are what make the global Network view useful for
/// diagnosing misconfiguration (e.g. a wrong `integration_id`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GatewayEvent {
    /// RFC3339 timestamp; filled in by [`record`] if left empty.
    pub ts: String,
    /// `"inbound"` or `"outbound"`.
    pub direction: String,
    /// Delivery kind, e.g. `"apns"`, `"whatsapp"`, or `"text"`.
    pub platform: String,
    /// Sender identifier (phone number for WhatsApp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Human-readable sender name, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    /// Recipient identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Message body (truncated to [`MAX_BODY_CHARS`]).
    #[serde(default)]
    pub body: String,
    /// Upstream gateway integration UUID (`source_id`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// The matched gateway channel's id, or `None` if unrouted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// The matched gateway channel's name, for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    /// Outcome: `routed`, `no_matching_channel`, `signature_rejected`,
    /// `sent`, `send_failed`.
    pub outcome: String,
    /// Extra context — persona used, error message, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Truncate a message body to a bounded length (on a char boundary), appending
/// an ellipsis when shortened.
pub fn truncate_body(body: &str) -> String {
    if body.chars().count() <= MAX_BODY_CHARS {
        return body.to_string();
    }
    let mut out: String = body.chars().take(MAX_BODY_CHARS).collect();
    out.push('…');
    out
}

fn log_path() -> std::path::PathBuf {
    data_dir().join("gateway_activity.jsonl")
}

/// Append an event to the activity log. Best-effort: never panics, logs on error.
pub fn record(mut event: GatewayEvent) {
    if event.ts.is_empty() {
        event.ts = now_rfc3339();
    }
    if let Err(e) = append(&event) {
        log::warn!("Failed to record gateway activity: {e}");
    }
}

fn append(event: &GatewayEvent) -> std::io::Result<()> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")
}

/// Return the most recent events, newest first, capped at `limit`. When
/// `channel_id` is `Some`, only events for that channel are returned; when
/// `None`, all events are returned (the global Network view).
pub fn list(channel_id: Option<&str>, limit: usize) -> Vec<GatewayEvent> {
    let path = log_path();
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let mut events: Vec<GatewayEvent> = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<GatewayEvent>(&l).ok())
        .filter(|e| match channel_id {
            Some(id) => e.channel_id.as_deref() == Some(id),
            None => true,
        })
        .collect();
    // Newest first, then cap.
    events.reverse();
    events.truncate(limit);
    events
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_bodies() {
        assert_eq!(truncate_body("hello"), "hello");
    }

    #[test]
    fn truncate_shortens_long_bodies() {
        let long = "x".repeat(MAX_BODY_CHARS + 50);
        let out = truncate_body(&long);
        assert_eq!(out.chars().count(), MAX_BODY_CHARS + 1); // + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn record_and_list_round_trip() {
        // Isolate the data dir so the test doesn't touch real state.
        let tmp = std::env::temp_dir().join(format!("mc-gw-act-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: single-threaded test; sets the data-dir override before use.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &tmp);
        }
        // paths::data_dir() caches via OnceLock, so this round-trip only works
        // if no prior test in this binary initialized it. Guard by skipping the
        // assertion when the cached dir differs from our temp dir.
        if data_dir() != tmp {
            return;
        }

        record(GatewayEvent {
            direction: "inbound".into(),
            platform: "pipestreamr".into(),
            from: Some("+15551230000".into()),
            body: "hi".into(),
            channel_id: Some("chan-1".into()),
            outcome: "routed".into(),
            ..Default::default()
        });
        record(GatewayEvent {
            direction: "inbound".into(),
            platform: "pipestreamr".into(),
            source_id: Some("missing-int".into()),
            channel_id: None,
            outcome: "no_matching_channel".into(),
            ..Default::default()
        });

        let all = list(None, 100);
        assert_eq!(all.len(), 2);
        // Newest first.
        assert_eq!(all[0].outcome, "no_matching_channel");

        let for_chan = list(Some("chan-1"), 100);
        assert_eq!(for_chan.len(), 1);
        assert_eq!(for_chan[0].outcome, "routed");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
