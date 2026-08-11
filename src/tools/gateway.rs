//! Generic gateway send tool.
//!
//! `gateway_send_message` is the single tool an agent uses to reply on *any*
//! gateway channel. It is platform-agnostic: the caller names the `platform`
//! (the channel type id, e.g. `"whatsapp"`), and the tool dispatches to that
//! type's native `adapter` (declared in its `channel_type.json`). Today only the
//! `twilio` adapter exists; adding Discord/Slack means adding a match arm here
//! (and a channel type manifest), not a new tool.

use async_trait::async_trait;

use crate::gateway_channels;

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
        "Send a message to a user on a gateway channel. `platform` is the channel type/kind (e.g. \"whatsapp\" for SMS/WhatsApp, or \"apns\" to deliver as a push notification). `channel_id` is the recipient on that platform (for WhatsApp, their phone number in E.164 format, e.g. +15551234567; ignored for push, which fans out over the user's registered devices). `content` is the message text. Optionally pass `from` to choose which of your channel's accounts/numbers sends it."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "platform": {
                    "type": "string",
                    "description": "The gateway channel type/kind to send through, e.g. \"whatsapp\" for SMS/WhatsApp or \"apns\" to deliver as a push notification."
                },
                "channel_id": {
                    "type": "string",
                    "description": "Recipient identifier on that platform (a phone number in E.164 format for WhatsApp)."
                },
                "content": {
                    "type": "string",
                    "description": "The message text to send."
                },
                "from": {
                    "type": "string",
                    "description": "Optional sender identity (e.g. your WhatsApp number). Defaults to the platform's configured sender."
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
        let from = args.get("from").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());

        // Resolve the adapter from the channel type's manifest — this is the
        // data-driven dispatch key. Fall back to treating `platform` as the
        // adapter name directly if no type is installed under that id.
        let adapter = gateway_channels::find_type(platform)
            .map(|t| t.adapter)
            .unwrap_or_else(|| platform.to_string());

        let result = match adapter.as_str() {
            // PipeStreamr passthru: `from` carries the optional project_id selector.
            // Credentials are channel-scoped, so resolve the sending channel first
            // and build its config.
            "pipestreamr" => match resolve_pipestreamr_channel(from) {
                Ok(channel) => match crate::tools::pipestreamr::PipeCfg::for_channel(&channel) {
                    Ok(cfg) => {
                        crate::tools::pipestreamr::send(channel_id, content, from, None, &cfg).await
                    }
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
            // Push (APNs): the caller is declaring a destination *kind*, not a
            // specific sender. Ride the same gateway-wire transport as pipestreamr
            // (the pod's connected gateway channel carries the credentials), but
            // tell the gateway to route through the account's default/primary apns
            // integration. `from`/`channel_id` don't apply — a push fans out over
            // the owner's registered devices server-side.
            "apns" => match resolve_pipestreamr_channel(None) {
                Ok(channel) => match crate::tools::pipestreamr::PipeCfg::for_channel(&channel) {
                    // A push can only be delivered by a connected Metalcraft
                    // gateway. If the channel has no BASE_URL it would target the
                    // public pipestreamr.com, which doesn't handle push — fail
                    // loud instead of POSTing an apns request into a black hole
                    // (the failure would otherwise be invisible on both ends).
                    Ok(cfg) if cfg.is_public_default() => Err(
                        "APNs push requires a connected Metalcraft gateway, but this channel has no \
                         BASE_URL set — it would POST to the public pipestreamr.com, which does not \
                         deliver push. Connect the pod to your gateway (or set the channel's BASE_URL)."
                            .to_string(),
                    ),
                    Ok(cfg) => {
                        crate::tools::pipestreamr::send(channel_id, content, None, Some("apns"), &cfg)
                            .await
                    }
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
            "twilio" => crate::tools::twilio::send_whatsapp(channel_id, content, from).await,
            other => Err(format!(
                "no send adapter for platform '{platform}' (adapter '{other}'). Enable a gateway channel of a supported type."
            )),
        };

        record_outbound(&adapter, channel_id, content, from, &result);

        result.map_err(err)
    }
}

/// Resolve which pipestreamr-adapter channel a send should flow through. With a
/// `from` selector, match it against a channel's `integration_id` (then `from`)
/// setting. Without one, use the single enabled pipestreamr channel — ambiguity
/// (>1) or absence (0) is a clear error, since each channel now carries its own
/// credentials and we can't guess which to bill.
fn resolve_pipestreamr_channel(
    from: Option<&str>,
) -> Result<gateway_channels::ChannelInstance, String> {
    if let Some(f) = from {
        return gateway_channels::resolve_by_setting("integration_id", f)
            .or_else(|| gateway_channels::resolve_by_setting("from", f))
            .ok_or_else(|| format!("no enabled channel matches from='{f}'"));
    }
    let mut candidates: Vec<_> = gateway_channels::enabled_instances()
        .into_iter()
        .filter(|c| {
            gateway_channels::find_type(&c.type_id).map(|t| t.adapter).as_deref() == Some("pipestreamr")
        })
        .collect();
    match candidates.len() {
        0 => Err("no enabled pipestreamr channel configured".to_string()),
        1 => Ok(candidates.remove(0)),
        _ => Err(
            "multiple enabled pipestreamr channels — pass `from` (the integration_id) to pick one"
                .to_string(),
        ),
    }
}

/// Record an outbound send in the gateway activity log. Best-effort and never
/// affects the send result. The originating channel is resolved from `from`
/// (the PipeStreamr `integration_id`, or a Twilio sender number) so the reply
/// files under the same channel as the inbound message that prompted it.
fn record_outbound(
    adapter: &str,
    recipient: &str,
    content: &str,
    from: Option<&str>,
    result: &Result<serde_json::Value, String>,
) {
    let channel = from.and_then(|f| {
        gateway_channels::resolve_by_setting("integration_id", f)
            .or_else(|| gateway_channels::resolve_by_setting("from", f))
    });
    let (outcome, detail) = match result {
        Ok(_) => ("sent", None),
        Err(e) => ("send_failed", Some(e.clone())),
    };
    crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
        direction: "outbound".into(),
        platform: adapter.to_string(),
        from: from.map(str::to_string),
        to: Some(recipient.to_string()),
        body: crate::gateway_activity::truncate_body(content),
        source_id: from.map(str::to_string),
        channel_id: channel.as_ref().map(|c| c.id.clone()),
        channel_name: channel.as_ref().map(|c| c.name.clone()),
        outcome: outcome.into(),
        detail,
        ..Default::default()
    });
}
