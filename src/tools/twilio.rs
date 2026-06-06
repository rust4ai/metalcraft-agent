//! Native Twilio adapter for the WhatsApp gateway channel type.
//!
//! This is the `twilio` adapter behind the generic `gateway_send_message` tool
//! (see [`crate::tools::gateway`]). Two halves:
//!   * [`send_whatsapp`] — POSTs a WhatsApp message to the Twilio REST API
//!     directly (HTTP Basic auth from the key store), so the agent talks to
//!     Twilio with no external gateway service in between.
//!   * Inbound helpers ([`parse_inbound`], [`validate_signature`]) used by the
//!     daemon's `/webhook/twilio` route to accept Twilio's form-encoded message
//!     webhooks and verify they really came from Twilio.
//!
//! Credentials come from the key store / environment (see [`crate::key_store`]):
//!   * `TWILIO_ACCOUNT_SID`   — the account SID (also the REST API username)
//!   * `TWILIO_AUTH_TOKEN`    — the auth token (REST password + webhook HMAC key)
//!   * `TWILIO_WHATSAPP_FROM` — default sender number, used when a send doesn't
//!     specify `from` (per-channel `from` is normally passed in by the webhook).

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Ensure a number carries the `whatsapp:` channel prefix Twilio expects.
fn whatsapp_addr(number: &str) -> String {
    let n = number.trim();
    if n.starts_with("whatsapp:") {
        n.to_string()
    } else {
        format!("whatsapp:{n}")
    }
}

/// Send a WhatsApp message through Twilio. `channel_id` is the recipient's
/// E.164 number; `from` is the sender number (defaults to the
/// `TWILIO_WHATSAPP_FROM` key when `None`). Returns a small JSON receipt or a
/// human-readable error string. Called by the generic gateway send tool.
pub async fn send_whatsapp(
    channel_id: &str,
    content: &str,
    from: Option<&str>,
) -> Result<serde_json::Value, String> {
    let account_sid = crate::key_store::lookup("TWILIO_ACCOUNT_SID")
        .filter(|s| !s.is_empty())
        .ok_or("TWILIO_ACCOUNT_SID is not set (add it in the workshop's keys, or export it)")?;
    let auth_token = crate::key_store::lookup("TWILIO_AUTH_TOKEN")
        .filter(|s| !s.is_empty())
        .ok_or("TWILIO_AUTH_TOKEN is not set (add it in the workshop's keys, or export it)")?;
    let from = from
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| crate::key_store::lookup("TWILIO_WHATSAPP_FROM").filter(|s| !s.is_empty()))
        .ok_or("no sender number: pass `from` or set the TWILIO_WHATSAPP_FROM key")?;

    let url = format!("{TWILIO_API_BASE}/Accounts/{account_sid}/Messages.json");
    let params = [
        ("From", whatsapp_addr(&from)),
        ("To", whatsapp_addr(channel_id)),
        ("Body", content.to_string()),
    ];

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .user_agent("metalcraft-agent/0.4 (twilio)")
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client
        .post(&url)
        .basic_auth(&account_sid, Some(&auth_token))
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("request to Twilio failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = extract_twilio_error(&body).unwrap_or_else(|| crate::tools::truncate_output(body.trim(), 500));
        return Err(format!("Twilio returned HTTP {} — {detail}", status.as_u16()));
    }

    let sid = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("sid").and_then(|s| s.as_str()).map(String::from));
    Ok(serde_json::json!({
        "to": channel_id,
        "from": from,
        "sent": true,
        "message_sid": sid,
    }))
}

/// Pull a human-readable message out of a Twilio JSON error body, if present.
fn extract_twilio_error(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let msg = v.get("message")?.as_str()?;
    let code = v.get("code").and_then(|c| c.as_i64());
    Some(match code {
        Some(c) => format!("{msg} (code {c})"),
        None => msg.to_string(),
    })
}

// ── Inbound webhook helpers ──────────────────────────────────────────────

/// A WhatsApp message extracted from a Twilio inbound webhook form body.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Sender's number (E.164, `whatsapp:` prefix stripped) — the reply target.
    pub from: String,
    /// Our number that received the message (`whatsapp:` prefix stripped) —
    /// used to route to a channel instance.
    pub to: String,
    /// Message text.
    pub body: String,
    /// Twilio message SID, if present.
    pub message_sid: Option<String>,
    /// Sender's WhatsApp profile name, if present.
    pub profile_name: Option<String>,
}

/// Parse the fields of a Twilio WhatsApp inbound webhook (form-encoded). Returns
/// `None` if the mandatory `From`/`Body` fields are absent (e.g. a status
/// callback rather than an inbound message).
pub fn parse_inbound(form: &HashMap<String, String>) -> Option<InboundMessage> {
    let strip = |s: &str| s.trim().trim_start_matches("whatsapp:").to_string();
    let from = form.get("From").map(|s| strip(s)).filter(|s| !s.is_empty())?;
    let body = form.get("Body").cloned()?;
    let to = form.get("To").map(|s| strip(s)).unwrap_or_default();
    Some(InboundMessage {
        from,
        to,
        body,
        message_sid: form.get("MessageSid").or_else(|| form.get("SmsSid")).cloned(),
        profile_name: form.get("ProfileName").cloned().filter(|s| !s.is_empty()),
    })
}

/// Validate Twilio's `X-Twilio-Signature` header. Twilio signs the full request
/// URL followed by each POST parameter (sorted by name) concatenated as
/// `name+value`, HMAC-SHA1 with the account auth token, base64-encoded. See
/// <https://www.twilio.com/docs/usage/security#validating-requests>.
pub fn validate_signature(
    auth_token: &str,
    url: &str,
    params: &HashMap<String, String>,
    signature: &str,
) -> bool {
    match sign(auth_token, url, params) {
        Some(expected) => expected.as_bytes().ct_eq(signature.as_bytes()),
        None => false,
    }
}

/// Compute the expected `X-Twilio-Signature` for a request: base64(HMAC-SHA1(
/// auth_token, url + concat(name+value for each param sorted by name))).
/// Returns `None` only if the token is somehow an invalid HMAC key.
fn sign(auth_token: &str, url: &str, params: &HashMap<String, String>) -> Option<String> {
    let mut sorted: Vec<(&String, &String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut data = url.to_string();
    for (k, v) in sorted {
        data.push_str(k);
        data.push_str(v);
    }
    let mut mac = HmacSha1::new_from_slice(auth_token.as_bytes()).ok()?;
    mac.update(data.as_bytes());
    Some(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

/// Minimal constant-time byte comparison so signature checks don't leak length
/// via early-exit timing. (Avoids pulling in a crypto-eq crate for one use.)
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
    fn whatsapp_addr_adds_prefix_once() {
        assert_eq!(whatsapp_addr("+15551234567"), "whatsapp:+15551234567");
        assert_eq!(whatsapp_addr("whatsapp:+15551234567"), "whatsapp:+15551234567");
    }

    #[test]
    fn parse_inbound_strips_prefix_and_requires_body() {
        let mut form = HashMap::new();
        form.insert("From".into(), "whatsapp:+15551112222".into());
        form.insert("To".into(), "whatsapp:+14155238886".into());
        form.insert("Body".into(), "hello".into());
        form.insert("MessageSid".into(), "SM123".into());
        let m = parse_inbound(&form).unwrap();
        assert_eq!(m.from, "+15551112222");
        assert_eq!(m.to, "+14155238886");
        assert_eq!(m.body, "hello");
        assert_eq!(m.message_sid.as_deref(), Some("SM123"));

        let mut no_body = HashMap::new();
        no_body.insert("From".into(), "whatsapp:+1".into());
        assert!(parse_inbound(&no_body).is_none());
    }

    /// Round-trip: a signature produced by `sign` must validate, a tampered
    /// one must not, and reordering the params map must not change the result
    /// (canonicalization sorts by name).
    #[test]
    fn validate_signature_round_trips_and_rejects_tampering() {
        let url = "https://mycompany.com/webhook/twilio";
        let mut params = HashMap::new();
        params.insert("To".into(), "+18005551212".into());
        params.insert("From".into(), "+14158675309".into());
        params.insert("Body".into(), "hi there".into());
        let token = "secret-auth-token";

        let good = sign(token, url, &params).unwrap();
        assert!(validate_signature(token, url, &params, &good));
        assert!(!validate_signature(token, url, &params, "wrong-signature"));
        assert!(!validate_signature("other-token", url, &params, &good));

        // Insertion order must not matter — signing sorts by param name.
        let mut reordered = HashMap::new();
        reordered.insert("Body".into(), "hi there".into());
        reordered.insert("To".into(), "+18005551212".into());
        reordered.insert("From".into(), "+14158675309".into());
        assert_eq!(sign(token, url, &reordered).unwrap(), good);
    }
}
