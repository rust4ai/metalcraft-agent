//! How a delegated sub-agent is *made* to finish in a shape the parent can act on.
//!
//! The problem this replaces: a sub-agent that did 40% of the job and one that
//! finished both return prose, so the orchestrator reads both as done, answers,
//! and the turn ends three steps short. The first fix asked the child to end
//! its reply with a ```handoff JSON fence and parsed the last fence out of the
//! free text. That works right up until the model does not do it — and then it
//! degrades *silently*, because "no fence" and "nothing outstanding" are the
//! same observation. Every miss looked like success.
//!
//! The fix is to stop asking. `yield_result` is registered in the child's
//! registry as a *terminal* tool and the child runs with
//! [`metalcraft::ToolChoice::Required`], so free text is not a way its turn can
//! end: the loop only reaches `END` when this tool has been called and
//! succeeded. The handoff contract is the tool's JSON Schema, which means the
//! provider validates it instead of a regex.
//!
//! The model can still end a turn without calling it — by running out of steps,
//! or by calling something else forever — so the caller reminds and retries
//! (see `MAX_YIELD_ATTEMPTS` in [`crate::tools::sub_agent`]), and the final
//! attempt hands the child a registry containing nothing but this tool. Only if
//! *that* fails does the parent fall back to the child's trailing prose, and
//! the result is then marked `unreconciled` so the parent can see the contract
//! was not met rather than reading an unverified summary as a report.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

/// The name the child sees, and the name the caller passes as a terminal tool.
pub const YIELD_TOOL_NAME: &str = "yield_result";

/// What a sub-agent reported about its own work.
///
/// `completed` is prose rather than a boolean because it is *both* halves of
/// the old contract at once: the answer the parent reads and the claim the
/// parent checks. Whether the delegation is finished is not a separate field
/// the model can contradict itself on — it is exactly `not_done.is_empty()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffReport {
    /// What the delegate actually did and found, in its own words.
    pub completed: String,
    /// Concrete one-liners naming what is still outstanding.
    pub not_done: Vec<String>,
    /// The persona best placed to pick the rest up.
    pub suggest_persona: Option<String>,
}

impl HandoffReport {
    /// Nothing outstanding. The single source of truth for "is it done".
    pub fn is_complete(&self) -> bool {
        self.not_done.is_empty()
    }
}

/// Where a child's yielded report lands.
///
/// A slot rather than a return value because the tool is called from inside the
/// nested executor, several frames below the code that wants the answer.
pub type ReportSlot = Arc<Mutex<Option<HandoffReport>>>;

pub fn slot() -> ReportSlot {
    Arc::new(Mutex::new(None))
}

pub struct YieldResultTool {
    slot: ReportSlot,
}

impl YieldResultTool {
    pub fn new(slot: ReportSlot) -> Self {
        Self { slot }
    }
}

#[async_trait]
impl metalcraft::Tool for YieldResultTool {
    fn name(&self) -> &str {
        YIELD_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Finish the delegation. This is the ONLY way to end your turn — your reply is not \
         delivered as free text, it is delivered here. Call it exactly once, last.\n\n\
         `completed` is your whole answer to the agent that delegated to you: what you did, what \
         you found, the file paths and names it needs. `not_done` is every part of the task that \
         is still outstanding — including work you could not do because you lack the tools for it \
         (a read-only delegate cannot edit files; a delegate without an integration's tools \
         cannot call it). Reporting honestly that work remains is worth far more than a tidy \
         answer: the orchestrator will delegate the rest."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "completed": {
                    "type": "string",
                    "description": "Your findings and what you did, written for the agent that delegated this. Concrete: name files, commands, ids."
                },
                "not_done": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One concrete line per outstanding item, naming files or targets. Empty array if you genuinely finished everything."
                },
                "suggest_persona": {
                    "type": "string",
                    "description": "Slug of the persona best suited to finish what is left. Omit if nothing is left or you do not know one."
                }
            },
            "required": ["completed", "not_done"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let report = parse_report(&args)?;
        let outstanding = report.not_done.len();
        // A poisoned slot is a panic in a sibling tool, not a reason to refuse
        // the one call that ends the turn — refusing here would loop the child
        // until its step limit.
        let mut guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(report);
        Ok(serde_json::json!({
            "delivered": true,
            "outstanding": outstanding,
        }))
    }
}

/// Read the report out of the call arguments, or say precisely what is wrong.
///
/// Returning an error rather than coercing matters here: a failed terminal tool
/// does **not** end the turn (metalcraft only routes to `END` on a successful
/// call), so the model gets the message back and can correct itself inside the
/// same run. Coercing a malformed payload into a plausible report would spend
/// that one free correction on inventing a status nobody wrote.
fn parse_report(args: &serde_json::Value) -> metalcraft::Result<HandoffReport> {
    let completed = args
        .get("completed")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: YIELD_TOOL_NAME.into(),
            message: "`completed` must be a non-empty string: what you did and found, written \
                      for the agent that delegated this."
                .into(),
        })?
        .to_string();

    // `not_done` is required by the schema, but a model that omits it has said
    // "nothing outstanding" as clearly as an empty array does, and failing the
    // call over a missing empty list would cost a whole extra turn to learn
    // nothing. A present-but-wrong-type value is different: that is a model
    // trying to say something the parent cannot read.
    let not_done = match args.get("not_done") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Some(_) => {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: YIELD_TOOL_NAME.into(),
                message: "`not_done` must be an array of strings (use [] if nothing is \
                          outstanding)."
                    .into(),
            });
        }
    };

    Ok(HandoffReport {
        completed,
        not_done,
        suggest_persona: args
            .get("suggest_persona")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft::Tool;

    #[tokio::test]
    async fn a_yielded_report_lands_in_the_slot() {
        let slot = slot();
        let tool = YieldResultTool::new(slot.clone());
        let out = tool
            .call(serde_json::json!({
                "completed": "Read Hero.tsx; four claims are stale.",
                "not_done": ["edit Hero.tsx to drop the 4 stale claims", "  "],
                "suggest_persona": " coding-agent "
            }))
            .await
            .expect("a well-formed yield succeeds, which is what ends the turn");
        assert_eq!(out["outstanding"], 1, "blank entries are not obligations");

        let report = slot.lock().unwrap().clone().expect("the slot holds it");
        assert!(!report.is_complete());
        assert_eq!(
            report.not_done,
            vec!["edit Hero.tsx to drop the 4 stale claims"]
        );
        assert_eq!(report.suggest_persona.as_deref(), Some("coding-agent"));
    }

    #[tokio::test]
    async fn an_empty_not_done_is_the_only_way_to_claim_completion() {
        let slot = slot();
        let tool = YieldResultTool::new(slot.clone());
        tool.call(serde_json::json!({ "completed": "Did it all.", "not_done": [] }))
            .await
            .unwrap();
        assert!(slot.lock().unwrap().as_ref().unwrap().is_complete());
    }

    /// A failed terminal tool does not end the turn, so a refusal here is the
    /// child's chance to fix itself before the parent has to reconcile prose.
    #[tokio::test]
    async fn a_malformed_yield_is_refused_rather_than_guessed_at() {
        let slot = slot();
        let tool = YieldResultTool::new(slot.clone());
        assert!(
            tool.call(serde_json::json!({ "not_done": [] })).await.is_err(),
            "no summary is not a report"
        );
        assert!(
            tool.call(serde_json::json!({ "completed": "   ", "not_done": [] }))
                .await
                .is_err()
        );
        assert!(
            tool.call(serde_json::json!({ "completed": "x", "not_done": "the tests" }))
                .await
                .is_err(),
            "a string where a list belongs is the model trying to say something unreadable"
        );
        assert!(
            slot.lock().unwrap().is_none(),
            "a refused call must not half-fill the slot"
        );

        // Omitted entirely is different from present-and-wrong: it reads as
        // nothing outstanding, and costs no extra turn.
        tool.call(serde_json::json!({ "completed": "done" }))
            .await
            .unwrap();
        assert!(slot.lock().unwrap().as_ref().unwrap().is_complete());
    }
}
