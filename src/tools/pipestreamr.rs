//! Native PipeStreamr adapter for the `pipestreamr` gateway channel type.
//!
//! PipeStreamr is a unified messaging *passthru*: we send a message to it with a
//! single Bearer API key and it relays to the underlying platform (WhatsApp via
//! Twilio, etc.) without us holding the platform secrets. This adapter sits
//! behind the generic `gateway_send_message` tool (see [`crate::tools::gateway`]).
//!
//! Two halves:
//!   * [`send`] — POSTs to PipeStreamr's `/api/v1/messages/send`.
//!   * Inbound helpers ([`parse_inbound`], [`validate_signature`]) used by the
//!     daemon's `/webhook/pipestreamr` route to accept PipeStreamr's JSON
//!     `message.created` webhooks and verify they really came from PipeStreamr.
//!
//! Credentials / config come from the key store / environment (see
//! [`crate::key_store`]):
//!   * `PIPESTREAMR_API_KEY`        — Bearer key for the send API (`ps_live_…`)
//!   * `PIPESTREAMR_WEBHOOK_SECRET` — HMAC-SHA256 key for inbound signatures
//!   * `PIPESTREAMR_BASE_URL`       — optional override of the API base URL

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_BASE: &str = "https://pipestreamr.com";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// The PipeStreamr deployment base URL. Defaults to the hosted service; override
/// with the `PIPESTREAMR_BASE_URL` key for a self-hosted deployment.
fn base_url() -> String {
    crate::key_store::lookup("PIPESTREAMR_BASE_URL")
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// Loose check that a string is shaped like a UUID (8-4-4-4-12 hex). Used to
/// avoid forwarding a stray `from` value as PipeStreamr's `project_id` (a
/// `Uuid`), which would 400; a non-UUID is dropped so PipeStreamr falls back to
/// the account's default integration.
fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Send a message through PipeStreamr. `channel_id` is the recipient (a phone
/// number for the WhatsApp passthru); `integration_id`, when given, selects which
/// PipeStreamr integration sends it (otherwise PipeStreamr uses the account's
/// default integration). Returns a small JSON receipt or an error string. Called
/// by the generic gateway send tool.
pub async fn send(
    channel_id: &str,
    content: &str,
    integration_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let api_key = crate::key_store::lookup("PIPESTREAMR_API_KEY")
        .filter(|s| !s.is_empty())
        .ok_or("PIPESTREAMR_API_KEY is not set (add it in the workshop's keys, or export it)")?;

    let url = format!("{}/api/v1/messages/send", base_url());
    let mut body = serde_json::json!({ "to": channel_id, "body": content });
    if let Some(iid) = integration_id.map(str::trim).filter(|s| !s.is_empty()) {
        if looks_like_uuid(iid) {
            body["integration_id"] = serde_json::Value::String(iid.to_string());
        } else {
            log::warn!("pipestreamr: ignoring non-UUID integration_id '{iid}'; using default integration");
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .user_agent("metalcraft-agent/0.4 (pipestreamr)")
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request to PipeStreamr failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = extract_error(&text).unwrap_or_else(|| crate::tools::truncate_output(text.trim(), 500));
        return Err(format!("PipeStreamr returned HTTP {} — {detail}", status.as_u16()));
    }

    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    Ok(serde_json::json!({
        "to": channel_id,
        "sent": true,
        "id": parsed.get("id"),
        "message_sid": parsed.get("sid"),
        "status": parsed.get("status"),
    }))
}

/// Pull a human-readable message out of a PipeStreamr JSON error body.
fn extract_error(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("error")
        .or_else(|| v.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

// ── Inbound webhook helpers ──────────────────────────────────────────────

/// A message extracted from a PipeStreamr `message.created` webhook.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Sender id (e.g. phone number) — the reply target.
    pub from: String,
    /// Recipient id (our number) — informational; routing uses `source_id`.
    pub to: String,
    /// PipeStreamr integration UUID that received the message (`data.source_id`)
    /// — the stable key we route to a channel instance by.
    pub source_id: Option<String>,
    /// Message text.
    pub body: String,
    /// PipeStreamr/platform message id, if present.
    pub external_id: Option<String>,
    /// Sender display name, if present.
    pub from_name: Option<String>,
}

/// Parse a PipeStreamr webhook payload `{ event, data, timestamp }`. Returns
/// `None` for anything that isn't an inbound `message.created` with a body —
/// including our own outbound echoes (guarded via `data.attributes.direction`).
pub fn parse_inbound(payload: &serde_json::Value) -> Option<InboundMessage> {
    if payload.get("event").and_then(|e| e.as_str()) != Some("message.created") {
        return None;
    }
    let data = payload.get("data")?;
    // Skip our own outbound sends if PipeStreamr ever fans those out.
    if data.get("attributes").and_then(|a| a.get("direction")).and_then(|d| d.as_str()) == Some("outbound") {
        return None;
    }
    let body = data.get("body").and_then(|b| b.as_str()).filter(|s| !s.is_empty())?;
    let from = data.get("from_id").and_then(|f| f.as_str()).filter(|s| !s.is_empty())?;
    let to = data.get("to_id").and_then(|t| t.as_str()).unwrap_or_default();
    Some(InboundMessage {
        from: from.to_string(),
        to: to.to_string(),
        source_id: data.get("source_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string),
        body: body.to_string(),
        external_id: data.get("external_id").and_then(|v| v.as_str()).map(str::to_string),
        from_name: data
            .get("from_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Validate PipeStreamr's `X-PipeStreamr-Signature` header: hex-encoded
/// HMAC-SHA256 of the raw request body, keyed by the webhook secret. The body
/// MUST be the exact bytes received (PipeStreamr signs its serialized payload).
pub fn validate_signature(secret: &str, body: &[u8], signature_hex: &str) -> bool {
    match sign(secret, body) {
        Some(expected) => expected.as_bytes().ct_eq(signature_hex.as_bytes()),
        None => false,
    }
}

fn sign(secret: &str, body: &[u8]) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body);
    Some(hex::encode(mac.finalize().into_bytes()))
}

/// Constant-time byte comparison (mirrors the helper in the twilio adapter).
trait CtEq {
    fn ct_eq(&self, other: &[u8]) -> bool;
}
impl CtEq for [u8] {
    fn ct_eq(&self, other: &[u8]) -> bool {
        if self.len() != other.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in self.iter().zip(other.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inbound_extracts_fields_and_skips_non_messages() {
        let payload = serde_json::json!({
            "event": "message.created",
            "data": {
                "from_id": "+15551112222",
                "to_id": "+14155238886",
                "source_id": "550e8400-e29b-41d4-a716-446655440000",
                "body": "hello",
                "external_id": "SM123",
                "from_name": "Alice"
            },
            "timestamp": "2026-06-06T00:00:00Z"
        });
        let m = parse_inbound(&payload).unwrap();
        assert_eq!(m.from, "+15551112222");
        assert_eq!(m.to, "+14155238886");
        assert_eq!(m.source_id.as_deref(), Some("550e8400-e29b-41d4-a716-446655440000"));
        assert_eq!(m.body, "hello");
        assert_eq!(m.external_id.as_deref(), Some("SM123"));
        assert_eq!(m.from_name.as_deref(), Some("Alice"));

        // Wrong event type, missing body, and outbound echo all yield None.
        assert!(parse_inbound(&serde_json::json!({"event":"log.created","data":{}})).is_none());
        assert!(parse_inbound(&serde_json::json!({"event":"message.created","data":{"from_id":"+1"}})).is_none());
        let outbound = serde_json::json!({
            "event": "message.created",
            "data": { "from_id": "+1", "body": "x", "attributes": { "direction": "outbound" } }
        });
        assert!(parse_inbound(&outbound).is_none());
    }

    #[test]
    fn looks_like_uuid_accepts_only_uuid_shape() {
        assert!(looks_like_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!looks_like_uuid("proj-123"));
        assert!(!looks_like_uuid("+14155238886"));
        assert!(!looks_like_uuid("550e8400e29b41d4a716446655440000")); // no dashes
    }

    #[test]
    fn validate_signature_round_trips_and_rejects_tampering() {
        let secret = "whsec_pipestreamr";
        let body = br#"{"event":"message.created","data":{"body":"hi"}}"#;
        let good = sign(secret, body).unwrap();
        assert!(validate_signature(secret, body, &good));
        assert!(!validate_signature(secret, body, "deadbeef"));
        assert!(!validate_signature("other", body, &good));
        assert!(!validate_signature(secret, br#"{"tampered":true}"#, &good));
    }
}
