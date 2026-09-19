//! The model-facing half of the delegate lifecycle.
//!
//! [`crate::tools::sub_agent`] spawns children and
//! [`crate::tools::sub_agent_registry`] keeps them addressable; these three
//! tools are how the orchestrator actually uses that. Without them a revivable
//! registry is a data structure nobody can reach — the model has no way to know
//! a delegate is still there, so it delegates again and re-pays for every read
//! the first one already did.
//!
//! - `sub_agent_list` — what has been delegated, in what state, with a bounded
//!   preview of each result.
//! - `sub_agent_read` — page a delegate's full transcript, which the parent's
//!   tool result deliberately did not inline.
//! - `sub_agent_send` — ask a finished delegate a follow-up. This is the one
//!   that pays for the rest: the child answers with everything it already read
//!   still in its history.
//!
//! All three are scoped to the calling agent instance (see
//! [`crate::tools::sub_agent::SubAgentTool::scope`]) and are registered only
//! below [`crate::tools::sub_agent::MAX_SUB_AGENT_DEPTH`], for the same reason
//! `sub_agent` refuses there: an agent at the bottom of the tree has no
//! delegates of its own to list, read or revive.

use async_trait::async_trait;
use metalcraft::AgentState;

use crate::tools::sub_agent::SubAgentTool;
use crate::tools::sub_agent_registry as delegates;

/// The tools registration installs alongside `sub_agent`.
///
/// Exported because two places have to agree on this list: the registry that
/// installs them, and [`crate::persona::Persona::resolved_tool_names`], which
/// is what everything *else* asks for the agent's tool surface — the step
/// guard, tool disclosure, and the eval harness's allowlist. When those two
/// disagree the model calls a tool it genuinely has and an allowlist check
/// fails it for going out of bounds.
pub const DELEGATE_LIFECYCLE_TOOLS: [&str; 3] =
    ["sub_agent_list", "sub_agent_read", "sub_agent_send"];

/// Wraps the delegation machinery so a follow-up rebuilds the child exactly the
/// way the original `sub_agent` call did — same credentials, same roster, same
/// depth, same stop flag.
fn missing(tool: &str, field: &str) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: format!("Missing required parameter: {field}"),
    }
}

pub struct SubAgentListTool {
    scope: String,
}

impl SubAgentListTool {
    pub fn new(instance_id: Option<String>) -> Self {
        Self {
            scope: instance_id.unwrap_or_else(|| "pod".to_string()),
        }
    }
}

#[async_trait]
impl metalcraft::Tool for SubAgentListTool {
    fn name(&self) -> &str {
        "sub_agent_list"
    }

    fn description(&self) -> &str {
        "List the sub-agents you have delegated to in this process, with their state and a \
         short preview of what each returned. `idle` and `parked` delegates can be given \
         follow-up work with `sub_agent_send`, which is far cheaper than a fresh delegation \
         because they still hold everything they read. `aborted` ones cannot be revived. \
         Note: this directory is lost when the pod restarts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let rows: Vec<serde_json::Value> = delegates::list(&self.scope)
            .into_iter()
            .map(|d| {
                let mut row = serde_json::json!({
                    "id": d.id,
                    "ran_as": d.label,
                    "state": d.state.label(),
                    "task": d.task,
                    "turns": d.turns,
                    "revivable": d.state.is_revivable(),
                    "preview": d.preview,
                    "transcript_bytes": d.transcript_bytes,
                });
                if d.age_secs > 0 {
                    row["idle_for_secs"] = serde_json::json!(d.age_secs);
                }
                if !d.not_done.is_empty() {
                    row["not_done"] = serde_json::json!(d.not_done);
                }
                if d.unreconciled {
                    row["unreconciled"] = serde_json::json!(true);
                }
                if let Some(note) = d.note {
                    row["note"] = serde_json::json!(note);
                }
                row
            })
            .collect();
        Ok(serde_json::json!({
            "count": rows.len(),
            "delegates": rows,
        }))
    }
}

pub struct SubAgentReadTool {
    scope: String,
}

impl SubAgentReadTool {
    pub fn new(instance_id: Option<String>) -> Self {
        Self {
            scope: instance_id.unwrap_or_else(|| "pod".to_string()),
        }
    }
}

#[async_trait]
impl metalcraft::Tool for SubAgentReadTool {
    fn name(&self) -> &str {
        "sub_agent_read"
    }

    fn description(&self) -> &str {
        "Read a delegate's full transcript — every tool call it made and every answer it gave. \
         A delegation's tool result only carries a bounded preview, because an unbounded one \
         would be replayed into every later request for the rest of the conversation; this is \
         where the rest of it lives. Pass the `next_offset` from the previous call to page on."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The `delegate_id` a sub_agent call returned (also shown by sub_agent_list)."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte offset to read from. Omit for the start; pass the previous call's `next_offset` to continue."
                }
            },
            "required": ["id"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = args["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| missing("sub_agent_read", "id"))?;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;

        let Some(chunk) = delegates::read(&self.scope, id, offset) else {
            return Ok(serde_json::json!({
                "error": true,
                "result": format!("{}", delegates::ReviveError::Unknown),
            }));
        };
        let mut out = serde_json::json!({
            "id": id,
            "state": chunk.state.label(),
            "offset": chunk.offset,
            "total_bytes": chunk.total_bytes,
            "transcript": chunk.text,
        });
        if let Some(next) = chunk.next_offset {
            out["next_offset"] = serde_json::json!(next);
            out["truncated"] = serde_json::json!(true);
        }
        Ok(out)
    }
}

pub struct SubAgentSendTool {
    delegate: SubAgentTool,
}

impl SubAgentSendTool {
    pub fn new(delegate: SubAgentTool) -> Self {
        Self { delegate }
    }
}

#[async_trait]
impl metalcraft::Tool for SubAgentSendTool {
    fn name(&self) -> &str {
        "sub_agent_send"
    }

    fn description(&self) -> &str {
        "Give a finished delegate more work. Prefer this over a fresh `sub_agent` call whenever \
         the follow-up is about something a delegate already looked at: an `idle` delegate \
         answers with its whole history intact, so it does not re-read the files, re-run the \
         searches or re-discover the layout you already paid for. A `parked` delegate is revived \
         from its transcript instead — it remembers what it found, not every step of finding it. \
         An `aborted` delegate is refused: there is nothing behind it to resume."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The `delegate_id` to follow up with (see sub_agent_list)."
                },
                "message": {
                    "type": "string",
                    "description": "The follow-up task, written as if continuing the conversation you already had with this delegate."
                }
            },
            "required": ["id", "message"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = args["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| missing("sub_agent_send", "id"))?
            .to_string();
        let message = args["message"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| missing("sub_agent_send", "message"))?
            .to_string();

        // Same guard as a fresh delegation: reviving a delegate is still
        // starting a whole agent run inside one tool call, and a stop pressed
        // before it began must not be answered by starting it.
        if self.delegate.stopped() {
            return Ok(serde_json::json!({
                "result": "Follow-up not sent: stopped by the user.",
                "stopped": true,
                "error": true,
            }));
        }

        let scope = self.delegate.scope();
        let revival = match delegates::check_out(&scope, &id) {
            Ok(revival) => revival,
            Err(e) => {
                return Ok(serde_json::json!({
                    "error": true,
                    "delegate_id": id,
                    "result": format!("{e}"),
                }));
            }
        };

        let (spawn_args, state) = match revival {
            delegates::Revival::Resume {
                spawn_args,
                messages,
            } => (
                spawn_args,
                crate::tools::sub_agent::continue_from(messages, &message),
            ),
            delegates::Revival::Reseed {
                spawn_args,
                transcript,
            } => (spawn_args, AgentState::new(reseed_prompt(&transcript, &message))),
        };

        // Containment is re-checked here, not assumed from the first run: a
        // persona dropped from the roster since then must not come back through
        // the follow-up door.
        let plan = match self.delegate.prepare_child(&spawn_args) {
            Ok(plan) => plan,
            Err(refusal) => {
                // `check_out` already flipped the record to `Running`. Leaving
                // it there would make one bad follow-up permanently un-revive
                // a delegate that is perfectly fine.
                delegates::park(&scope, &id, "follow-up refused before it started", "");
                return match refusal {
                    crate::tools::sub_agent::Refusal::Reply(value) => Ok(value),
                    crate::tools::sub_agent::Refusal::Fail(error) => Err(error),
                };
            }
        };

        delegates::note_followup(&scope, &id, &message);
        let outcome = self.delegate.run_child(&plan, state).await;
        Ok(self.delegate.settle_outcome(&id, &plan, outcome))
    }
}

/// The prompt a parked delegate is revived with.
///
/// A parked delegate's verbatim history is gone (see the registry's module
/// docs), so this re-seeds a fresh child with what it concluded. Saying plainly
/// that this is a resumption — rather than pasting the transcript as if the
/// child had written it this turn — is what stops the child from re-doing the
/// work it is reading about.
fn reseed_prompt(transcript: &str, message: &str) -> String {
    format!(
        "You are resuming work you already did. Your earlier run is transcribed below; treat it \
         as your own memory of it, and do NOT repeat work it already shows finished.\n\n\
         --- your earlier run ---\n{transcript}\n--- end ---\n\n\
         Follow-up task: {message}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft::Tool;

    fn tool() -> SubAgentSendTool {
        SubAgentSendTool::new(
            SubAgentTool::new("k".into(), "m".into(), "p".into())
                .with_instance(Some("inst".into())),
        )
    }

    #[tokio::test]
    async fn an_aborted_delegate_is_refused_rather_than_revived() {
        // The state exists so the model is told "no" instead of being handed a
        // revival that starts from nothing and looks like a resumption.
        let _exclusive = delegates::exclusive();
        delegates::reset();
        let id = delegates::open("inst", "slow-agent", "wait", serde_json::json!({}));
        delegates::abort("inst", &id, "timed out after 120 seconds");

        let out = tool()
            .call(serde_json::json!({ "id": id, "message": "carry on" }))
            .await
            .expect("the tool answers rather than failing");
        assert_eq!(out["error"], true, "{out}");
        assert!(
            out["result"].as_str().unwrap().contains("aborted"),
            "the model has to learn why: {out}"
        );
    }

    #[tokio::test]
    async fn an_unknown_id_says_the_directory_is_not_persisted() {
        let _exclusive = delegates::exclusive();
        delegates::reset();
        let out = tool()
            .call(serde_json::json!({ "id": "nobody", "message": "hi" }))
            .await
            .unwrap();
        assert_eq!(out["error"], true);
        assert!(out["result"].as_str().unwrap().contains("restarted"));
    }

    #[tokio::test]
    async fn a_stop_is_honoured_before_a_revival_starts() {
        let _exclusive = delegates::exclusive();
        delegates::reset();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let send = SubAgentSendTool::new(
            SubAgentTool::new("k".into(), "m".into(), "p".into())
                .with_instance(Some("inst".into()))
                .with_interrupt(Some(flag)),
        );
        let id = delegates::open("inst", "research-agent", "t", serde_json::json!({}));
        delegates::settle(
            "inst",
            &id,
            delegates::Outcome {
                summary: "done".into(),
                not_done: vec![],
                unreconciled: false,
                tools_used: vec![],
                turns: 1,
                transcript: "t".into(),
                messages: vec![],
            },
        );
        let out = send
            .call(serde_json::json!({ "id": id, "message": "more" }))
            .await
            .unwrap();
        assert_eq!(out["stopped"], true, "{out}");
        assert!(
            delegates::list("inst")[0].state.is_revivable(),
            "a refused follow-up must not consume the delegate"
        );
    }

    #[tokio::test]
    async fn the_list_shows_state_and_a_bounded_preview() {
        let _exclusive = delegates::exclusive();
        delegates::reset();
        let id = delegates::open("inst", "research-agent", "survey it", serde_json::json!({}));
        let long = "a finding. ".repeat(5_000);
        delegates::settle(
            "inst",
            &id,
            delegates::Outcome {
                summary: long.clone(),
                not_done: vec!["the edits".into()],
                unreconciled: false,
                tools_used: vec!["read_file".into()],
                turns: 4,
                transcript: long.clone(),
                messages: vec![],
            },
        );

        let out = SubAgentListTool::new(Some("inst".into()))
            .call(serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out["count"], 1);
        let row = &out["delegates"][0];
        assert_eq!(row["id"], id.as_str());
        assert_eq!(row["state"], "idle");
        assert_eq!(row["revivable"], true);
        assert_eq!(row["not_done"][0], "the edits");
        let preview = row["preview"].as_str().unwrap();
        assert!(
            preview.len() < long.len() / 4,
            "a listing that inlines every result is the context bug it exists to avoid"
        );
        assert!(row["transcript_bytes"].as_u64().unwrap() > preview.len() as u64);

        // …and the full text pages out through the read tool.
        let read = SubAgentReadTool::new(Some("inst".into()))
            .call(serde_json::json!({ "id": id }))
            .await
            .unwrap();
        assert_eq!(read["offset"], 0);
        assert!(read["transcript"].as_str().unwrap().contains("a finding."));
        assert!(
            read["total_bytes"].as_u64().unwrap()
                > read["transcript"].as_str().unwrap().len() as u64,
            "a long transcript pages rather than arriving whole"
        );
        assert!(read["next_offset"].is_number());
    }

    #[tokio::test]
    async fn reading_an_unknown_delegate_is_an_answer_not_a_failure() {
        let _exclusive = delegates::exclusive();
        delegates::reset();
        let out = SubAgentReadTool::new(Some("inst".into()))
            .call(serde_json::json!({ "id": "ghost" }))
            .await
            .expect("an unknown id is something the model can fix, not a tool failure");
        assert_eq!(out["error"], true);
    }
}
