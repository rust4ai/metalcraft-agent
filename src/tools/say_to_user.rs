//! Channel-agnostic reply tool.
//!
//! `say_to_user` is the single way an agent delivers a user-facing reply,
//! regardless of channel. The session's I/O preset decides where the text goes:
//! a workshop chat streams it over SSE, a gateway session sends it out through
//! the bound adapter (gateway/Twilio). That routing lives in the injected
//! [`ReplySink`] — the tool itself is platform-agnostic and never names a
//! channel. It is also registered as a *terminal* tool (see
//! [`crate::runtime`]), so calling it ends the turn.
//!
//! When no sink is configured (one-shot/flow runs with no session context) the
//! tool simply acks, so personas that include it still run safely.

use async_trait::async_trait;

use crate::tools::ReplySink;

pub struct SayToUserTool {
    sink: Option<ReplySink>,
}

impl SayToUserTool {
    pub fn new(sink: Option<ReplySink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl metalcraft::Tool for SayToUserTool {
    fn name(&self) -> &str {
        "say_to_user"
    }
    fn description(&self) -> &str {
        "Send your reply to the user. This is the ONLY way to communicate with \
         the user — they do not see your other tool calls or reasoning. Call it \
         when you have the answer or need to ask the user something. Calling it \
         ends your turn."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message text to send to the user."
                }
            },
            "required": ["message"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| crate::tools::missing_param(self.name(), "message"))?;

        match &self.sink {
            Some(sink) => {
                sink(message.to_string()).await.map_err(|e| {
                    metalcraft::GraphError::ToolCallFailed {
                        tool: "say_to_user".into(),
                        message: format!("failed to deliver reply: {e}"),
                    }
                })?;
                Ok(serde_json::json!({ "delivered": true }))
            }
            // No session sink (one-shot/flow): acknowledge without delivery.
            None => {
                Ok(serde_json::json!({ "delivered": false, "note": "no reply sink configured" }))
            }
        }
    }
}
