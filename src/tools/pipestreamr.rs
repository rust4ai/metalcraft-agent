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
//! Credentials / config are **channel-scoped** — resolved from the channel
//! instance that owns them via [`PipeCfg::for_channel`] and
//! [`channel_webhook_secret`], not from global keys:
//!   * `API_KEY`        — Bearer key for the send API. For a `metalcraft-gateway`
//!     provisioner it is *derived* from `METALCRAFT_TOKEN` at call time (never
//!     stored); for a manual pipestreamr channel it is the channel's `API_KEY`
//!     secret.
//!   * `WEBHOOK_SECRET` — HMAC-SHA256 key for inbound signatures.
//!   * `BASE_URL`       — optional override of the API base URL.
//!
//! Legacy global `PIPESTREAMR_*` keys from pre-scoped installs are migrated into
//! channel scope on boot (see [`crate::metalcraft_gateway::migrate_legacy_keys`]);
//! the runtime no longer reads them.

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::gateway_channels::ChannelInstance;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_BASE: &str = "https://pipestreamr.com";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Resolved send config for one pipestreamr-adapter channel.
pub struct PipeCfg {
    /// Bearer key for the send API.
    pub api_key: String,
    /// API base URL (no trailing slash).
    pub base_url: String,
}

impl PipeCfg {
    /// Resolve the send config for `channel`. The `api_key` for a
    /// `metalcraft-gateway` provisioner is derived from `METALCRAFT_TOKEN`
    /// (never persisted); otherwise it comes from the channel's `API_KEY` secret.
    /// `base_url` comes from the channel's `BASE_URL` secret, defaulting to the
    /// hosted service.
    pub fn for_channel(channel: &ChannelInstance) -> Result<Self, String> {
        let provisioner = crate::gateway_channels::find_type(&channel.type_id)
            .and_then(|t| t.provisioner);
        let api_key = if provisioner.as_deref() == Some("metalcraft-gateway") {
            // Prefer the audience-scoped connection token adopted at connect;
            // fall back to deriving from the pod's broad METALCRAFT_TOKEN (the
            // pre-broker path, and a safety net if the token is ever missing).
            crate::key_store::lookup_scoped(Some(&channel.id), "API_KEY")
                .filter(|s| !s.is_empty())
                .or_else(|| crate::key_store::lookup("METALCRAFT_TOKEN").filter(|s| !s.is_empty()))
                .ok_or("METALCRAFT_TOKEN is not set — this pod isn't linked to a Metalcraft ID account")?
        } else {
            crate::key_store::lookup_scoped(Some(&channel.id), "API_KEY")
                .filter(|s| !s.is_empty())
                .ok_or("no API key configured for this channel (add its API_KEY secret)")?
        };
        let base_url = crate::key_store::lookup_scoped(Some(&channel.id), "BASE_URL")
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        Ok(Self { api_key, base_url })
    }

    /// True when no `BASE_URL` was configured, so this config targets the hosted
    /// public `pipestreamr.com`. First-party push (apns) is a Metalcraft-gateway
    /// feature the public service can't fulfil — callers delivering a push must
    /// reject this rather than silently POST into a black hole.
    pub fn is_public_default(&self) -> bool {
        self.base_url == DEFAULT_BASE
    }
}

/// The inbound HMAC secret for `channel`: its `WEBHOOK_SECRET` secret. `None`
/// when unconfigured.
pub fn channel_webhook_secret(channel: &ChannelInstance) -> Option<String> {
    crate::key_store::lookup_scoped(Some(&channel.id), "WEBHOOK_SECRET")
        .filter(|s| !s.is_empty())
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

/// Send a message through the gateway. `channel_id` is the recipient (a phone
/// number for the WhatsApp passthru; ignored for push kinds). `integration_id`,
/// when given, selects which integration sends it. `target_platform`, when given
/// (e.g. `"apns"`), tells the gateway to route through the account's
/// default/primary integration of that *kind* instead — this is how a caller
/// says "deliver this as a push" without knowing the integration UUID.
/// `cfg` carries the sending channel's resolved API key + base URL (see
/// [`PipeCfg::for_channel`]). Returns a small JSON receipt or an error string.
/// Called by the generic gateway send tool.
pub async fn send(
    channel_id: &str,
    content: &str,
    integration_id: Option<&str>,
    target_platform: Option<&str>,
    cfg: &PipeCfg,
) -> Result<serde_json::Value, String> {
    let url = format!("{}/api/v1/messages/send", cfg.base_url);
    let mut body = serde_json::json!({ "to": channel_id, "body": content });
    if let Some(iid) = integration_id.map(str::trim).filter(|s| !s.is_empty()) {
        if looks_like_uuid(iid) {
            body["integration_id"] = serde_json::Value::String(iid.to_string());
        } else {
            log::warn!("pipestreamr: ignoring non-UUID integration_id '{iid}'; using default integration");
        }
    }
    if let Some(kind) = target_platform.map(str::trim).filter(|s| !s.is_empty()) {
        body["platform"] = serde_json::Value::String(kind.to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .user_agent("metalcraft-agent/0.4 (pipestreamr)")
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
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
    /// The gateway's own message UUID (`messages.id`), if the gateway included it
    /// (`data.gateway_message_id`). Preferred dedup key — always present per real
    /// inbound and stable across re-delivery, unlike `external_id` (carrier SID,
    /// which can be absent). See [`crate::inbound_dedup`].
    pub gateway_message_id: Option<String>,
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
        gateway_message_id: data
            .get("gateway_message_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
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
                "gateway_message_id": "11111111-2222-3333-4444-555555555555",
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
        // The gateway's dedup key must survive parsing (the cross-repo contract).
        assert_eq!(m.gateway_message_id.as_deref(), Some("11111111-2222-3333-4444-555555555555"));
        assert_eq!(m.from_name.as_deref(), Some("Alice"));

        // Absent gateway_message_id (older gateway / non-gateway source) ⇒ None,
        // so dedup falls back to external_id.
        let no_gw = serde_json::json!({
            "event": "message.created",
            "data": { "from_id": "+1", "body": "hi", "external_id": "SM9" }
        });
        assert_eq!(parse_inbound(&no_gw).unwrap().gateway_message_id, None);

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
