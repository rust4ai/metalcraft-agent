//! Inbound gateway webhooks.
//!
//! The messaging gateway relays inbound messages (WhatsApp/SMS, etc.) to the pod
//! as JSON `message.created` webhooks on the daemon's `/webhook/pipestreamr`
//! route. This module parses those payloads ([`parse_inbound`]) and verifies
//! their HMAC-SHA256 signature ([`validate_signature`]) so we only accept
//! messages that really came from the gateway.
//!
//! Outbound sending lives in [`crate::channels`] — a channel is just a
//! connection (`{ url, secret }`); this module is inbound-only.
//!
//! The inbound HMAC secret is channel-scoped (`WEBHOOK_SECRET` on the channel
//! instance the message routes to). Legacy global `PIPESTREAMR_*` keys from
//! pre-scoped installs are migrated into channel scope on boot (see
//! [`crate::metalcraft_gateway::migrate_legacy_keys`]).

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::gateway_channels::ChannelInstance;

type HmacSha256 = Hmac<Sha256>;

/// The inbound HMAC secret for `channel`: its `WEBHOOK_SECRET` secret. `None`
/// when unconfigured.
pub fn channel_webhook_secret(channel: &ChannelInstance) -> Option<String> {
    crate::key_store::lookup_scoped(Some(&channel.id), "WEBHOOK_SECRET")
        .filter(|s| !s.is_empty())
}

// ── Inbound webhook helpers ──────────────────────────────────────────────

/// A message extracted from an inbound `message.created` webhook.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Sender id (e.g. phone number) — the reply target.
    pub from: String,
    /// Recipient id (our number) — informational; routing uses `source_id`.
    pub to: String,
    /// The gateway integration UUID that received the message (`data.source_id`)
    /// — the stable key we route to a channel instance by.
    pub source_id: Option<String>,
    /// Message text.
    pub body: String,
    /// Gateway/platform message id, if present.
    pub external_id: Option<String>,
    /// The gateway's own message UUID (`messages.id`), if included
    /// (`data.gateway_message_id`). Preferred dedup key — always present per real
    /// inbound and stable across re-delivery, unlike `external_id` (carrier SID,
    /// which can be absent). See [`crate::inbound_dedup`].
    pub gateway_message_id: Option<String>,
    /// Sender display name, if present.
    pub from_name: Option<String>,
}

/// Parse a webhook payload `{ event, data, timestamp }`. Returns `None` for
/// anything that isn't an inbound `message.created` with a body — including our
/// own outbound echoes (guarded via `data.attributes.direction`).
pub fn parse_inbound(payload: &serde_json::Value) -> Option<InboundMessage> {
    if payload.get("event").and_then(|e| e.as_str()) != Some("message.created") {
        return None;
    }
    let data = payload.get("data")?;
    // Skip our own outbound sends if the gateway ever fans those out.
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

/// Validate the `X-PipeStreamr-Signature` header: hex-encoded HMAC-SHA256 of the
/// raw request body, keyed by the webhook secret. The body MUST be the exact
/// bytes received (the gateway signs its serialized payload).
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
    fn validate_signature_round_trips_and_rejects_tampering() {
        let secret = "whsec_gateway";
        let body = br#"{"event":"message.created","data":{"body":"hi"}}"#;
        let good = sign(secret, body).unwrap();
        assert!(validate_signature(secret, body, &good));
        assert!(!validate_signature(secret, body, "deadbeef"));
        assert!(!validate_signature("other", body, &good));
        assert!(!validate_signature(secret, br#"{"tampered":true}"#, &good));
    }
}
