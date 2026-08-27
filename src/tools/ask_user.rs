//! Ask the user a question and end the turn waiting on the answer.
//!
//! Mechanically this is `say_to_user` with a different intent: both are terminal
//! tools, both go out through the session's [`ReplySink`], and the conversation
//! already survives between turns (`AgentState::continue_with` appends), so the
//! user's answer arrives as the next turn with the whole thread intact. The
//! agent could always have asked a question — it just never did, because every
//! instruction it had pushed it toward acting on a guess.
//!
//! It is a separate tool anyway, for three reasons that a shared one cannot
//! give: a client can render a question differently from an answer (the
//! `awaiting_reply` marker and tappable `options`), the plan gate can let a
//! question through while it is still refusing a final answer, and "when may I
//! ask?" can be written as policy about one tool rather than as a caveat
//! attached to the reply tool.
//!
//! [`ReplySink`]: crate::tools::ReplySink

use async_trait::async_trait;

use crate::tools::{ReplyEnvelope, ReplySink};

pub struct AskUserTool {
    sink: Option<ReplySink>,
}

impl AskUserTool {
    pub fn new(sink: Option<ReplySink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl metalcraft::Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user one clarifying question and end your turn waiting for the answer. \
         Their reply arrives as the next message in this conversation, with everything \
         you have done so far still in context. \
         Use it when two readings of the request would lead to materially different work \
         AND guessing wrong would waste more than one delegation — e.g. \"check X is \
         accurate\" could mean report the drift or fix it. \
         Do NOT use it for anything you could find out yourself: which repo, which file, \
         what the code says are research questions — delegate those. \
         Ask at most once per turn, and never on a request that is already unambiguous."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question, in the user's terms. Include the one line of context they need to answer it — they have not seen your tool calls or reasoning."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional. 2-4 concrete answers the user can pick from, each a short phrase (e.g. 'Just report the drift', 'Report it and fix the page'). They may still answer in their own words, so never make these exhaustive-looking."
                },
                "why": {
                    "type": "string",
                    "description": "Optional. One line on what you will do differently depending on the answer — shown to the user so the question does not read as stalling."
                }
            },
            "required": ["question"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| crate::tools::missing_param(self.name(), "question"))?;

        let options: Vec<String> = args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        // `why` is appended rather than sent as a field: every channel can render
        // a sentence, and a question that explains what turns on the answer reads
        // as progress instead of as the agent stalling.
        let mut text = question.to_string();
        if let Some(why) = args
            .get("why")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            text.push_str("\n\n");
            text.push_str(why);
        }

        match &self.sink {
            Some(sink) => {
                sink(ReplyEnvelope::question(text, options.clone()))
                    .await
                    .map_err(|e| metalcraft::GraphError::ToolCallFailed {
                        tool: "ask_user".into(),
                        message: format!("failed to deliver question: {e}"),
                    })?;
                Ok(serde_json::json!({
                    "delivered": true,
                    "awaiting_reply": true,
                    "options": options,
                }))
            }
            // No session sink (one-shot/flow): there is nobody to ask. Say so
            // plainly rather than pretending the question was delivered — a run
            // with no user must not stall waiting on an answer that cannot come.
            None => Ok(serde_json::json!({
                "delivered": false,
                "note": "no reply sink configured — this run has no user to ask; proceed on your best reading of the request",
            })),
        }
    }
}
