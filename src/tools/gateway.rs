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
        "Send a message to a user on a gateway channel. `platform` is the channel type (e.g. \"whatsapp\"). `channel_id` is the recipient on that platform (for WhatsApp, their phone number in E.164 format, e.g. +15551234567). `content` is the message text. Optionally pass `from` to choose which of your channel's accounts/numbers sends it."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "platform": {
                    "type": "string",
                    "description": "The gateway channel type to send through, e.g. \"whatsapp\"."
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
            "pipestreamr" => crate::tools::pipestreamr::send(channel_id, content, from).await,
            "twilio" => crate::tools::twilio::send_whatsapp(channel_id, content, from).await,
            other => Err(format!(
                "no send adapter for platform '{platform}' (adapter '{other}'). Enable a gateway channel of a supported type."
            )),
        };
        result.map_err(err)
    }
}
