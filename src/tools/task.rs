//! The tools a project tick uses to keep its plan.
//!
//! These replace the half of `project_scratchpad_write` that was never really
//! prose. A plan rewritten as markdown on every tick is a plan a model can drop
//! a row from — and the tick frame's answer to that was to *ask it not to*
//! ("never drop an unchecked step"). These tools make the asking unnecessary:
//! the list is a store, the model only ever names the row it means, and nothing
//! it does not mention can change.
//!
//! Each is bound to one project at registration, exactly like the other `goal_*`
//! tools, so the model never names which project it is writing to.
//!
//! The one rule worth knowing before reading the code: **`task_done` requires
//! evidence.** Not because prose is worthless, but because "verify before you
//! claim" as a sentence in a system prompt is a rule; as a required parameter it
//! is a fact.

use async_trait::async_trait;

use crate::project_tasks::{self, Evidence, EvidenceKind, NewTask, TaskPatch, TaskStatus};

fn err(tool: &str, message: impl Into<String>) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

/// The list as it stands, returned by every tool so the model always sees the
/// consequence of what it just did without spending a call to look.
fn state(project_id: &str) -> serde_json::Value {
    let tasks = project_tasks::list(project_id);
    serde_json::json!({
        "summary": project_tasks::summarize(&tasks),
        "plan": project_tasks::render(&tasks),
    })
}

// ── add ──────────────────────────────────────────────────────────────────────

pub struct TaskAddTool {
    project_id: String,
}

impl TaskAddTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for TaskAddTool {
    fn name(&self) -> &str {
        "task_add"
    }

    fn description(&self) -> &str {
        "Add tasks to your project's plan. Pass the whole plan in one call — a planning tick writes \
         its plan once, not a row at a time. Each task should be one tick's worth of work and \
         concrete enough that you could tell whether it happened. Use `deps` to say what must \
         land first: tasks with NO deps run in parallel, so leave deps empty wherever two tasks \
         are genuinely independent. Within one call, a dep may be the 0-based index of another \
         task in the same call (\"0\", \"1\") as well as an existing task id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "The tasks to add, in plan order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "One line, imperative — 'Wire the limiter into the middleware'."
                            },
                            "detail": {
                                "type": "string",
                                "description": "What a delegate is handed when this task runs. It will have NONE of your context, so carry every decision this task depends on: file paths, the chosen approach, what 'done' looks like."
                            },
                            "deps": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Task ids (or 0-based indices into this call) that must finish first. Leave empty for anything independent — that is what lets tasks run at the same time."
                            },
                            "assignee": {
                                "type": "string",
                                "description": "Persona to delegate this to when it runs. Omit to do it yourself."
                            },
                            "mutates_workspace": {
                                "type": "boolean",
                                "description": "Whether running this writes to the workspace (edits files, runs a build). Defaults to true. Set false ONLY for pure reading — research, reviewing, reading CI output. Read-only tasks can run alongside each other; writing ones cannot, because there is one workspace."
                            },
                            "gate": {
                                "type": "string",
                                "description": "Optional. A command that must exit 0 before this task may be marked done — 'cargo test --all'. Run it with buildr's test/build, record it with project_await_run, and the next tick will have the verdict."
                            }
                        },
                        "required": ["title"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let raw = args["tasks"]
            .as_array()
            .ok_or_else(|| err("task_add", "Missing required parameter: tasks (an array)"))?;
        if raw.is_empty() {
            return Err(err("task_add", "tasks is empty — nothing to add"));
        }
        let new: Vec<NewTask> = raw
            .iter()
            .map(|t| NewTask {
                title: t["title"].as_str().unwrap_or_default().to_string(),
                detail: t["detail"].as_str().unwrap_or_default().to_string(),
                deps: t["deps"]
                    .as_array()
                    .map(|d| {
                        d.iter()
                            .filter_map(|x| {
                                x.as_str()
                                    .map(str::to_string)
                                    // A model that writes deps as numbers rather
                                    // than strings means the same thing.
                                    .or_else(|| x.as_u64().map(|n| n.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                assignee: t["assignee"].as_str().map(str::to_string),
                mutates_workspace: t["mutates_workspace"].as_bool(),
                gate: t["gate"].as_str().map(str::to_string),
            })
            .collect();

        let added = project_tasks::add_many(&self.project_id, &new).map_err(|e| err("task_add", e))?;
        let ids: Vec<&str> = added.iter().map(|t| t.id.as_str()).collect();
        Ok(serde_json::json!({
            "ok": true,
            "added": ids,
            "state": state(&self.project_id),
        }))
    }
}

// ── update ───────────────────────────────────────────────────────────────────

pub struct TaskUpdateTool {
    project_id: String,
}

impl TaskUpdateTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "task_update"
    }

    fn description(&self) -> &str {
        "Re-scope, re-route or re-order one task. Use this when a task turns out to be bigger \
         than you thought (tighten it and task_add the remainder), when a different persona \
         should run it, or when a dependency you did not see turns up. Anything you do not pass \
         is left alone. To finish a task use task_done; to stop one use task_block."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The task id, e.g. 't3'." },
                "title": { "type": "string" },
                "detail": { "type": "string" },
                "deps": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Replaces the dependency list entirely. Existing task ids only."
                },
                "assignee": { "type": "string", "description": "Persona to delegate to. Pass an empty string to clear it." },
                "mutates_workspace": { "type": "boolean" },
                "gate": { "type": "string", "description": "Pass an empty string to clear the gate." },
                "reopen": {
                    "type": "boolean",
                    "description": "Put a done, blocked or dropped task back to todo — what a review tick uses when the evidence does not hold up."
                }
            },
            "required": ["id"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = args["id"]
            .as_str()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| err("task_update", "Missing required parameter: id"))?;

        let patch = TaskPatch {
            title: args["title"].as_str().map(str::to_string),
            detail: args["detail"].as_str().map(str::to_string),
            deps: args["deps"].as_array().map(|d| {
                d.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            }),
            assignee: args["assignee"].as_str().map(|a| Some(a.to_string())),
            mutates_workspace: args["mutates_workspace"].as_bool(),
            gate: args["gate"].as_str().map(|g| Some(g.to_string())),
            status: args["reopen"]
                .as_bool()
                .filter(|r| *r)
                .map(|_| TaskStatus::Todo),
            // Reopening clears the stale reason along with the status: a task
            // back in the pool must not still read as blocked.
            blocked_reason: args["reopen"].as_bool().filter(|r| *r).map(|_| None),
            pending_run: None,
            bump_attempts: false,
        };

        let task = project_tasks::update(&self.project_id, id, patch).map_err(|e| err("task_update", e))?;
        Ok(serde_json::json!({
            "ok": true,
            "id": task.id,
            "state": state(&self.project_id),
        }))
    }
}

// ── done ─────────────────────────────────────────────────────────────────────

pub struct TaskDoneTool {
    project_id: String,
}

impl TaskDoneTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for TaskDoneTool {
    fn name(&self) -> &str {
        "task_done"
    }

    fn description(&self) -> &str {
        "Mark one task finished, with proof. The proof is required and it is the point: a build \
         that compiles is not a feature that works, and a task is done when you have SEEN \
         something, not when it looks likely. Pass the commit you pushed, the run id and its \
         exit code, the finding id, or the file you produced. If a task has a gate, it cannot be \
         completed until that gate has run and exited 0."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The task id, e.g. 't3'." },
                "evidence_kind": {
                    "type": "string",
                    "enum": ["commit", "run", "finding", "file", "note"],
                    "description": "What kind of proof this is. Prefer 'commit' or 'run' — 'note' is for the rare task with nothing else to point at."
                },
                "evidence": {
                    "type": "string",
                    "description": "The proof itself: a commit sha, a run id and its exit code ('r_88 exit 0'), a finding id, a path, or one sentence for 'note'."
                }
            },
            "required": ["id", "evidence_kind", "evidence"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = args["id"]
            .as_str()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| err("task_done", "Missing required parameter: id"))?;
        let kind = match args["evidence_kind"].as_str().unwrap_or("note") {
            "commit" => EvidenceKind::Commit,
            "run" => EvidenceKind::Run,
            "finding" => EvidenceKind::Finding,
            "file" => EvidenceKind::File,
            _ => EvidenceKind::Note,
        };
        let value = args["evidence"]
            .as_str()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .ok_or_else(|| {
                err(
                    "task_done",
                    "Missing required parameter: evidence. A task is done when you have seen \
                     something — name the commit, the run, or the file.",
                )
            })?;

        let task = project_tasks::complete(&self.project_id, id, Evidence::new(kind, value))
            .map_err(|e| err("task_done", e))?;

        // The log is what a person reads and what the next tick skims. A task
        // landing is exactly the kind of thing that belongs there, and writing
        // it here means the model does not have to remember to.
        let current = crate::projects::read_scratchpad(&self.project_id).unwrap_or_default();
        let updated = crate::projects::append_to_section(
            &current,
            "Log",
            &format!("- **{}** done: {} ({value})", task.id, task.title),
        );
        let _ = crate::projects::write_scratchpad(&self.project_id, &updated);

        Ok(serde_json::json!({
            "ok": true,
            "id": task.id,
            "state": state(&self.project_id),
        }))
    }
}

// ── block / drop ─────────────────────────────────────────────────────────────

pub struct TaskBlockTool {
    project_id: String,
}

impl TaskBlockTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for TaskBlockTool {
    fn name(&self) -> &str {
        "task_block"
    }

    fn description(&self) -> &str {
        "Stop ONE task on something you cannot resolve, and keep working on the rest. This is \
         not project_block: the project keeps ticking and its other tasks keep moving — only this row \
         waits. Use it for a missing credential, a decision you need from a person, or an \
         upstream that is down. Say concretely what would unblock it.\n\n\
         If the task is not blocked but simply should not be open — the work turned out to be \
         covered by another task, or the approach changed — say exactly that as the reason. You \
         cannot retire a task yourself; the plan belongs to the conductor, and it reads this. A \
         reason that says what happened gets the row dropped on the next tick; one that only says \
         'cannot proceed' gets it put in front of a person."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "reason": {
                    "type": "string",
                    "description": "What is stopping it, and what would unblock it. Written for a person who has not read the scratchpad."
                }
            },
            "required": ["id", "reason"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = args["id"]
            .as_str()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| err("task_block", "Missing required parameter: id"))?;
        let reason = args["reason"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| err("task_block", "Missing required parameter: reason"))?;

        let patch = TaskPatch {
            status: Some(TaskStatus::Blocked),
            blocked_reason: Some(Some(reason.to_string())),
            ..Default::default()
        };
        let task = project_tasks::update(&self.project_id, id, patch).map_err(|e| err("task_block", e))?;

        // Surfaced in the scratchpad too: a blocked task is something a person
        // reading the project should see without opening the task list.
        let current = crate::projects::read_scratchpad(&self.project_id).unwrap_or_default();
        let updated = crate::projects::append_to_section(
            &current,
            "Blockers",
            &format!("- **{}** {}: {reason}", task.id, task.title),
        );
        let _ = crate::projects::write_scratchpad(&self.project_id, &updated);

        Ok(serde_json::json!({
            "ok": true,
            "id": task.id,
            "note": "That task is parked. Its siblings are unaffected — keep working.",
            "state": state(&self.project_id),
        }))
    }
}

pub struct TaskDropTool {
    project_id: String,
}

impl TaskDropTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for TaskDropTool {
    fn name(&self) -> &str {
        "task_drop"
    }

    fn description(&self) -> &str {
        "Retire a task reality has made pointless — the feature was cut, the bug was fixed \
         upstream, the approach was abandoned. The row is kept rather than deleted so a later \
         tick does not re-derive it from the same reasoning that produced it. Anything waiting \
         on it stops waiting. This is a review tick's pruning verb; do not use it to skip work \
         that is merely hard."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "why": { "type": "string", "description": "One line: what changed that made this unnecessary." }
            },
            "required": ["id", "why"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = args["id"]
            .as_str()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| err("task_drop", "Missing required parameter: id"))?;
        let why = args["why"]
            .as_str()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .ok_or_else(|| err("task_drop", "Missing required parameter: why"))?;

        let patch = TaskPatch {
            status: Some(TaskStatus::Dropped),
            blocked_reason: Some(Some(why.to_string())),
            ..Default::default()
        };
        let task = project_tasks::update(&self.project_id, id, patch).map_err(|e| err("task_drop", e))?;
        Ok(serde_json::json!({
            "ok": true,
            "id": task.id,
            "state": state(&self.project_id),
        }))
    }
}

// ── dispatch ─────────────────────────────────────────────────────────────────

/// Run several ready tasks at once, each in its own sub-agent.
///
/// This is where a project actually gets parallel. A tick picks the rows that have
/// nothing left to wait for and hands them out together; three ninety-second
/// surveys take ninety seconds rather than four and a half minutes, and a tick
/// that would have overrun doing them one after another fits.
///
/// **The runner folds the bad news, the orchestrator closes the good news.** A
/// delegate that comes back unfinished has its `not_done` written into the
/// task's detail and its suggested persona set as the assignee, automatically —
/// that is the bookkeeping a model reliably forgets. A delegate that says it
/// finished does **not** close the task: the orchestrator has to look at what
/// came back and call `task_done` with real evidence. Letting a delegate's own
/// prose close a row would give back exactly the "it looks done" problem the
/// evidence requirement exists to remove.
pub struct TaskDispatchTool {
    project_id: String,
    api_key: String,
    model_name: String,
    system_prompt: String,
    preset_personas: Option<Vec<String>>,
    instance_id: Option<String>,
    depth: u32,
    interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl TaskDispatchTool {
    pub fn new(project_id: String, cfg: &crate::tools::ToolConfig) -> Self {
        Self {
            project_id,
            api_key: cfg.api_key.clone(),
            model_name: cfg.model_name.clone(),
            system_prompt: cfg.system_prompt.clone(),
            preset_personas: cfg.preset_personas.clone(),
            instance_id: cfg.instance_id.clone(),
            depth: cfg.sub_agent_depth,
            interrupt: cfg.interrupt.clone(),
        }
    }

    fn delegate(&self) -> crate::tools::sub_agent::SubAgentTool {
        crate::tools::sub_agent::SubAgentTool::new(
            self.api_key.clone(),
            self.model_name.clone(),
            self.system_prompt.clone(),
        )
        .with_depth(self.depth)
        .with_preset_personas(self.preset_personas.clone())
        .with_instance(self.instance_id.clone())
        .with_interrupt(self.interrupt.clone())
        // Deliberately no turn plan: a dispatched delegate's unfinished work is
        // recorded on its *task*, which outlives the turn. Recording it in both
        // places would hold the turn open over an obligation already durable.
    }
}

/// What one delegate is told. It has none of the tick's context, so the task's
/// own detail is the whole briefing — which is why `task_add` insists on one.
fn briefing(task: &project_tasks::Task, project: &str) -> String {
    let detail = if task.detail.trim().is_empty() {
        "(no detail was recorded for this task — do what the title says, and say what you \
         needed that you did not have.)"
    } else {
        task.detail.trim()
    };
    format!(
        "You are one step of a longer project: {project}\n\nYour task ({}): {}\n\n{detail}",
        task.id, task.title
    )
}

#[async_trait]
impl metalcraft::Tool for TaskDispatchTool {
    fn name(&self) -> &str {
        "task_dispatch"
    }

    fn description(&self) -> &str {
        "Run ready tasks in sub-agents. Pass several ids to run them AT THE SAME TIME — that is \
         how a tick does a week's reading in one wake-up. Each delegate is given only that \
         task's title and detail, so the detail has to carry everything it needs.\n\n\
         Only tasks marked `[ready]` can be dispatched. Tasks that change the workspace must be \
         dispatched ONE at a time: there is one workspace, and two agents editing it at once \
         overwrite each other.\n\n\
         What comes back is each delegate's report. Work that came back unfinished is folded \
         into its task for you. Work that came back finished is NOT closed for you — read what \
         it says, check it, and call `task_done` with the commit, the run, or the file."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task ids to run, e.g. [\"t2\", \"t3\"]. Several run at once; up to 3."
                }
            },
            "required": ["ids"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let ids: Vec<String> = args["ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(err("task_dispatch", "Missing required parameter: ids"));
        }

        let project = crate::projects::get(&self.project_id)
            .ok_or_else(|| err("task_dispatch", "this project no longer exists"))?;
        let tasks = project_tasks::list(&self.project_id);

        let mut chosen: Vec<project_tasks::Task> = Vec::new();
        for id in &ids {
            let task = project_tasks::get(&tasks, id)
                .ok_or_else(|| err("task_dispatch", format!("no task '{id}'")))?;
            if !project_tasks::is_ready(&task, &tasks) {
                return Err(err(
                    "task_dispatch",
                    format!(
                        "'{id}' is not ready ({}). Only tasks marked [ready] can run — the \
                         others are waiting on something.",
                        match task.status {
                            TaskStatus::Todo => "its dependencies have not landed".to_string(),
                            other => format!("{other:?}").to_lowercase(),
                        }
                    ),
                ));
            }
            chosen.push(task);
        }

        // One workspace: writers go one at a time. Refused here as well as in
        // the batch path, so the message names the task rather than the tool set.
        let writers: Vec<&str> = chosen
            .iter()
            .filter(|t| t.mutates_workspace)
            .map(|t| t.id.as_str())
            .collect();
        if writers.len() > 1 {
            return Err(err(
                "task_dispatch",
                format!(
                    "{} all change the workspace, and there is only one — two agents editing it \
                     at the same time overwrite each other without either noticing. Dispatch one \
                     of them now (alongside any read-only tasks) and the rest after.",
                    writers.join(", ")
                ),
            ));
        }

        let delegate = self.delegate();
        let call_args = if chosen.len() == 1 {
            let t = &chosen[0];
            let mut a = serde_json::json!({ "task": briefing(t, &project.goal) });
            match t.assignee.as_deref() {
                Some(p) => a["persona"] = serde_json::json!(p),
                // Without a persona a delegate gets the read-only set by
                // default, which is wrong for a task that exists to change
                // something. Widen only for those.
                None if t.mutates_workspace => a["tool_set"] = serde_json::json!("all"),
                None => {}
            }
            a
        } else {
            serde_json::json!({
                "tasks": chosen
                    .iter()
                    .map(|t| {
                        let mut a = serde_json::json!({ "task": briefing(t, &project.goal) });
                        if let Some(p) = t.assignee.as_deref() {
                            a["persona"] = serde_json::json!(p);
                        }
                        a
                    })
                    .collect::<Vec<_>>()
            })
        };

        let raw = delegate.call(call_args).await?;
        let per_task: Vec<serde_json::Value> = match raw.get("results").and_then(|r| r.as_array()) {
            Some(list) => list.clone(),
            None => vec![raw.clone()],
        };

        // Fold what came back into the records. Only the bad news is written
        // automatically — see the type's docs.
        let mut reports: Vec<serde_json::Value> = Vec::new();
        for (task, result) in chosen.iter().zip(per_task.iter()) {
            let finished = result
                .get("completed")
                .and_then(|c| c.as_bool())
                .unwrap_or(false)
                && !result
                    .get("error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false);
            let not_done: Vec<String> = result
                .get("not_done")
                .and_then(|n| n.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();

            if !finished {
                let mut patch = project_tasks::TaskPatch {
                    bump_attempts: true,
                    ..Default::default()
                };
                if !not_done.is_empty() {
                    patch.detail = Some(format!(
                        "{}\n\nStill outstanding after attempt {}:\n{}",
                        task.detail.trim(),
                        task.attempts + 1,
                        not_done
                            .iter()
                            .map(|n| format!("- {n}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                // The delegate that just read the code is better placed than the
                // orchestrator to say who should finish it.
                if let Some(next) = result.get("suggest_persona").and_then(|p| p.as_str())
                    && !next.trim().is_empty()
                {
                    patch.assignee = Some(Some(next.to_string()));
                }
                let _ = project_tasks::update(&self.project_id, &task.id, patch);
            }

            reports.push(serde_json::json!({
                "id": task.id,
                "title": task.title,
                "reported_complete": finished,
                "result": result.get("result").cloned().unwrap_or(serde_json::Value::Null),
                "not_done": not_done,
            }));
        }

        Ok(serde_json::json!({
            "dispatched": chosen.len(),
            "reports": reports,
            "next": "Check what came back. Close what genuinely landed with task_done and its \
                     evidence; anything reported unfinished is already recorded on its task.",
            "state": state(&self.project_id),
        }))
    }
}
