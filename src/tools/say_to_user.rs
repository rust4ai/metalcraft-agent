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
//!
//! Because calling it ends the turn, this is also the only place that can hold a
//! turn open. When the session shares a [`TurnPlan`](crate::turn_plan::TurnPlan)
//! and that plan still has open steps — or a delegation reported back unfinished
//! — the call is refused instead of delivered. Since metalcraft 0.10 a *failed*
//! terminal tool no longer ends the turn, so the refusal returns control to the
//! model along with the list of what it still owes. The plan bounds its own
//! refusals (see `MAX_GATE_REFUSALS`), so this can slow a turn down but never
//! deadlock it.

use async_trait::async_trait;

use crate::tools::{ReplyEnvelope, ReplySink};
use crate::turn_plan::SharedTurnPlan;

pub struct SayToUserTool {
    sink: Option<ReplySink>,
    /// The turn's plan, when this session has one. `None` ⇒ no gate: a
    /// sub-agent, a flow node, or a one-shot run answers whenever it likes.
    turn_plan: Option<SharedTurnPlan>,
}

impl SayToUserTool {
    pub fn new(sink: Option<ReplySink>) -> Self {
        Self {
            sink,
            turn_plan: None,
        }
    }

    /// Hold this reply tool to the turn's plan.
    pub fn with_turn_plan(mut self, plan: Option<SharedTurnPlan>) -> Self {
        self.turn_plan = plan;
        self
    }

    /// The reason this reply must not be delivered yet, if there is one.
    ///
    /// Consults the plan and, when it is blocking, spends one of its bounded
    /// refusals. Once those run out the plan stops blocking and the turn is
    /// allowed to close with work outstanding — deliberately, because a rail
    /// that can trap a turn is worse than the behaviour it corrects.
    fn refusal(&self) -> Option<String> {
        let plan = self.turn_plan.as_ref()?;
        let mut plan = crate::turn_plan::lock(plan);
        let reason = plan.blocking_reason()?;
        if plan.note_refusal() {
            Some(reason)
        } else {
            log::info!("say_to_user gate: refusals exhausted, delivering with open plan steps");
            None
        }
    }
}

#[async_trait]
impl metalcraft::Tool for SayToUserTool {
    fn name(&self) -> &str {
        "say_to_user"
    }
    fn description(&self) -> &str {
        "Deliver your final answer to the user. This is the ONLY way they see anything \
         you have done — they do not see your tool calls or reasoning. Calling it ends \
         your turn, so call it when the work is FINISHED, not when the first delegation \
         comes back. To ask a question instead, use `ask_user`."
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

        // The gate runs before delivery: a refused reply must not reach the user.
        if let Some(reason) = self.refusal() {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: "say_to_user".into(),
                message: reason,
            });
        }

        match &self.sink {
            Some(sink) => {
                sink(ReplyEnvelope::message(message)).await.map_err(|e| {
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
