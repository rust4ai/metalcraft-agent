//! Write down the plan for this turn, so it can be held to.
//!
//! See [`crate::turn_plan`] for why a written plan exists at all: prose in an
//! assistant message binds nothing, and the turn ends the moment the model
//! decides it is finished. Steps recorded here are read by the reply tool's
//! gate, which will not let the turn close while any of them are open.
//!
//! The plan is replaced wholesale on every call rather than patched per step.
//! That costs a few tokens of repetition and buys the thing that matters: a step
//! the model has silently abandoned shows up as a deletion instead of sitting
//! `pending` forever, and there is never a question of which write won.

use async_trait::async_trait;

use crate::turn_plan::{PlanStep, SharedTurnPlan, StepStatus, lock};

pub struct UpdatePlanTool {
    plan: SharedTurnPlan,
}

impl UpdatePlanTool {
    pub fn new(plan: SharedTurnPlan) -> Self {
        Self { plan }
    }
}

fn parse_status(value: Option<&str>) -> StepStatus {
    match value.unwrap_or("pending") {
        "in_progress" => StepStatus::InProgress,
        "done" => StepStatus::Done,
        "skipped" => StepStatus::Skipped,
        _ => StepStatus::Pending,
    }
}

#[async_trait]
impl metalcraft::Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }

    fn description(&self) -> &str {
        "Record this turn's plan as a list of steps, and update it as you go. \
         Call it BEFORE your first delegation on any request that needs more than one \
         step, and again after each step finishes (mark it done, or skipped if it turned \
         out unnecessary). Pass the WHOLE list every time — it replaces the previous plan. \
         You cannot end the turn while a step is still pending or in_progress, so this is \
         also how you record that you have finished."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "description": "The complete plan, in dependency order. 2-7 steps. Investigating and changing are always separate steps.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {
                                "type": "string",
                                "description": "What this step does, in one line, concrete enough that you could tell whether it happened (name files, name what 'done' means)."
                            },
                            "persona": {
                                "type": "string",
                                "description": "The persona you intend to delegate this step to (e.g. 'research-agent', 'coding-agent'). Advisory — you may reroute later."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done", "skipped"],
                                "description": "Defaults to 'pending'. Use 'skipped' for a step an earlier result made unnecessary — that closes it without doing it."
                            }
                        },
                        "required": ["step"]
                    }
                }
            },
            "required": ["steps"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let raw = args
            .get("steps")
            .and_then(|v| v.as_array())
            .ok_or_else(|| crate::tools::missing_param(self.name(), "steps"))?;

        let steps: Vec<PlanStep> = raw
            .iter()
            .filter_map(|item| {
                let step = item.get("step")?.as_str()?.trim();
                if step.is_empty() {
                    return None;
                }
                Some(PlanStep {
                    step: step.to_string(),
                    persona: item
                        .get("persona")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                    status: parse_status(item.get("status").and_then(|v| v.as_str())),
                })
            })
            .collect();

        if steps.is_empty() {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: "update_plan".into(),
                message: "`steps` must contain at least one step with a non-empty `step` field."
                    .into(),
            });
        }

        let (rendered, open) = {
            let mut plan = lock(&self.plan);
            plan.set_steps(steps);
            let open = plan
                .steps()
                .iter()
                .filter(|s| {
                    matches!(s.status, StepStatus::Pending | StepStatus::InProgress)
                })
                .count();
            (plan.render(), open)
        };

        Ok(serde_json::json!({
            "plan": rendered,
            "open_steps": open,
            "note": if open == 0 {
                "No open steps — you may deliver your answer with say_to_user."
            } else {
                "Work the open steps before answering; say_to_user will not deliver until they are closed."
            },
        }))
    }
}
