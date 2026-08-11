//! Generic gateway send tool.
//!
//! `gateway_send_message` is the single tool an agent uses to send on *any*
//! gateway channel. A **channel** is a named connection to a gateway
//! (`{ slug, url, secret }` — see [`crate::channels`]); the send picks one by
//! slug via the optional `channel` arg and defaults to the built-in `metalcraft`
//! channel (the first-party gateway on the pod's token), so a bare send Just
//! Works with no setup. `platform` names the *delivery kind* (`"apns"` for a push,
//! `"whatsapp"` for text) — orthogonal to which channel carries it.

use async_trait::async_trait;

fn err(message: impl std::fmt::Display) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: "gateway_send_message".into(),
        message: message.to_string(),
    }
}

pub struct GatewaySendMessageTool;

#[async_trait]
impl metalcraft::Tool for GatewaySendMessageTool {
    fn name(&self) -> &str {
        "gateway_send_message"
    }
    fn description(&self) -> &str {
        "Send a message to a user through a gateway channel. `platform` is the delivery kind — \"whatsapp\" for SMS/WhatsApp text, or \"apns\" to deliver as a push notification. `channel_id` is the recipient on that platform (for WhatsApp, their phone number in E.164 format, e.g. +15551234567; ignored for push, which fans out over the user's registered devices). `content` is the message text. Optionally pass `channel` (a channel slug) to send through a specific gateway connection — defaults to \"metalcraft\", the built-in first-party gateway. Optionally pass `from` (an integration id) to choose which of your gateway's sender accounts/numbers sends it."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "platform": {
                    "type": "string",
                    "description": "The delivery kind: \"whatsapp\" for SMS/WhatsApp text or \"apns\" to deliver as a push notification."
                },
                "channel_id": {
                    "type": "string",
                    "description": "Recipient identifier for the delivery kind (a phone number in E.164 format for WhatsApp; ignored for push)."
                },
                "content": {
                    "type": "string",
                    "description": "The message text to send."
                },
                "channel": {
                    "type": "string",
                    "description": "Optional channel slug — which gateway connection to send through. Defaults to \"metalcraft\" (the built-in first-party gateway)."
                },
                "from": {
                    "type": "string",
                    "description": "Optional sender integration id, to pick a specific sender account/number. Defaults to the gateway account's primary integration for the delivery kind."
                }
            },
            "required": ["platform", "channel_id", "content"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let platform = args
            .get("platform")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| crate::tools::missing_param(self.name(), "platform"))?;
        let channel_id = args
            .get("channel_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| crate::tools::missing_param(self.name(), "channel_id"))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| crate::tools::missing_param(self.name(), "content"))?;
        let channel_slug = args.get("channel").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());
        let from = args.get("from").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());

        // Delivery kind sent to the gateway (None → a generic text send routed to
        // the account's default integration), and the label recorded for the
        // activity feed ("apns" → shown as "Outbound · APNs", never a protocol
        // name).
        let kind = delivery_kind(platform);
        let event_kind = kind.unwrap_or("text");

        let result = match crate::channels::resolve_channel(channel_slug) {
            Ok(channel) => crate::channels::send(&channel, channel_id, content, kind, from).await,
            Err(e) => Err(e),
        };

        record_outbound(channel_slug, event_kind, channel_id, content, from, &result);

        result.map_err(err)
    }
}

/// Map a caller-supplied `platform` to the gateway's delivery-kind hint. `"apns"`
/// (or `"push"`) is a push; a generic/legacy transport name (`"gateway"`,
/// `"pipestreamr"`, `"text"`, empty) resolves to the account's default
/// integration (`None`); anything else (e.g. `"whatsapp"`, `"sms"`) is passed
/// through as the integration kind.
fn delivery_kind(platform: &str) -> Option<&str> {
    match platform.trim().to_ascii_lowercase().as_str() {
        "apns" | "push" => Some("apns"),
        "" | "text" | "gateway" | "pipestreamr" => None,
        _ => Some(platform),
    }
}

/// Record an outbound send in the gateway activity log. Best-effort and never
/// affects the send result. Files under the channel it was sent through, tagged
/// with the delivery *kind* (`apns`/`whatsapp`/`text`) rather than any transport.
fn record_outbound(
    channel_slug: Option<&str>,
    kind: &str,
    recipient: &str,
    content: &str,
    from: Option<&str>,
    result: &Result<serde_json::Value, String>,
) {
    let channel = crate::channels::get_channel(channel_slug.unwrap_or(crate::channels::DEFAULT_SLUG));
    let (outcome, detail) = match result {
        Ok(_) => ("sent", None),
        Err(e) => ("send_failed", Some(e.clone())),
    };
    crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
        direction: "outbound".into(),
        platform: kind.to_string(),
        from: from.map(str::to_string),
        to: Some(recipient.to_string()),
        body: crate::gateway_activity::truncate_body(content),
        source_id: from.map(str::to_string),
        channel_id: channel.as_ref().map(|c| c.slug.clone()),
        channel_name: channel.as_ref().map(|c| c.name.clone()),
        outcome: outcome.into(),
        detail,
        ..Default::default()
    });
}
