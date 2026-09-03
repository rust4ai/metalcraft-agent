//! The four tools a goal tick uses to write down where it got to.
//!
//! Each is bound to one goal at registration ([`ToolConfig::goal_id`]), so the
//! model never names which goal it is writing to — it cannot mistype it, and it
//! cannot write to another goal's scratchpad.
//!
//! The split is deliberate. [`GoalNoteTool`] is the cheap, frequent one: a line
//! of log, a blocker, a question. [`GoalScratchpadWriteTool`] replaces the whole
//! document and is the tick's *final* act — wholesale rather than patched, for
//! the same reason `update_plan` is: a step silently abandoned shows up as a
//! deletion instead of sitting there forever, and there is never a question of
//! which write won.
//!
//! [`GoalBlockTool`] and [`GoalCompleteTool`] are the two ways a goal stops. Both
//! are terminal for the *goal*, not merely for the turn, which is why they are
//! tools rather than something inferred from what the agent said.
//!
//! [`ToolConfig::goal_id`]: crate::tools::ToolConfig::goal_id

use async_trait::async_trait;

use crate::goals;

/// Sections a note may be appended to.
///
/// Not the full section list: `Goal` is immutable, and `Plan`/`State`/`Workspace`
/// are rewritten wholesale by the scratchpad write rather than appended to a line
/// at a time — appending to a plan is how a plan grows a second copy of itself.
const NOTE_SECTIONS: &[&str] = &["Log", "Blockers", "Questions for the human"];

fn err(tool: &str, message: impl Into<String>) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

pub struct GoalNoteTool {
    goal_id: String,
}

impl GoalNoteTool {
    pub fn new(goal_id: String) -> Self {
        Self { goal_id }
    }
}

#[async_trait]
impl metalcraft::Tool for GoalNoteTool {
    fn name(&self) -> &str {
        "goal_note"
    }

    fn description(&self) -> &str {
        "Append one line to your goal's scratchpad. Use `Log` for what you just did (start it \
         with what changed, not with what you intended), `Blockers` for something stopping \
         progress that you intend to work around, and `Questions for the human` for something \
         you want asked but are not blocking on. To rewrite the plan or the state, use \
         goal_scratchpad_write instead — this tool only appends."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "section": {
                    "type": "string",
                    "enum": NOTE_SECTIONS,
                    "description": "Which section to append to."
                },
                "text": {
                    "type": "string",
                    "description": "One line. Concrete enough to be useful to a future tick that remembers none of this — name files, branches, run ids."
                }
            },
            "required": ["section", "text"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let section = args["section"]
            .as_str()
            .ok_or_else(|| err("goal_note", "Missing required parameter: section"))?;
        if !NOTE_SECTIONS.contains(&section) {
            return Err(err(
                "goal_note",
                format!("Unknown section '{section}'. One of: {}", NOTE_SECTIONS.join(", ")),
            ));
        }
        let text = args["text"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| err("goal_note", "Missing required parameter: text"))?;

        let current = goals::read_scratchpad(&self.goal_id).unwrap_or_default();
        // One line means one line: a note carrying its own newlines would end up
        // as several list items, one of which is a fragment.
        let line = format!("- {}", text.replace('\n', " "));
        let updated = goals::append_to_section(&current, section, &line);
        goals::write_scratchpad(&self.goal_id, &updated)
            .map_err(|e| err("goal_note", format!("could not write scratchpad: {e}")))?;

        Ok(serde_json::json!({ "ok": true, "section": section }))
    }
}

pub struct GoalScratchpadWriteTool {
    goal_id: String,
}

impl GoalScratchpadWriteTool {
    pub fn new(goal_id: String) -> Self {
        Self { goal_id }
    }
}

#[async_trait]
impl metalcraft::Tool for GoalScratchpadWriteTool {
    fn name(&self) -> &str {
        "goal_scratchpad_write"
    }

    fn description(&self) -> &str {
        "Replace your goal's whole scratchpad. This is the last thing you do in a tick, and the \
         only memory you carry to the next one — the tick that reads it will know nothing you \
         know now. Pass the complete document, keeping every '## ' section heading: Goal, \
         Workspace, Plan, State, Log, Blockers, Questions for the human. Never drop an unchecked \
         plan step, an unresolved blocker or an open question; never check a box you did not \
         verify. The previous version is snapshotted, so a mistake here is recoverable — but a \
         tick that inherits a wrong plan will act on it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "markdown": {
                    "type": "string",
                    "description": "The complete scratchpad, in markdown, with all its '## ' sections."
                }
            },
            "required": ["markdown"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let markdown = args["markdown"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .ok_or_else(|| err("goal_scratchpad_write", "Missing required parameter: markdown"))?;

        // The goal statement is the one thing a rewrite may not lose: a tick that
        // drops it leaves every later tick working towards nothing in particular,
        // and the loss is invisible because the document still looks well-formed.
        let mut markdown = markdown.to_string();
        if goals::section_body(&markdown, "Goal").is_none_or(str::is_empty) {
            let Some(goal) = goals::get(&self.goal_id) else {
                return Err(err("goal_scratchpad_write", "this goal no longer exists"));
            };
            markdown = goals::replace_section(&markdown, "Goal", goal.goal.trim());
        }

        goals::write_scratchpad(&self.goal_id, &markdown)
            .map_err(|e| err("goal_scratchpad_write", format!("could not write scratchpad: {e}")))?;

        let progress = goals::progress_of(&markdown);
        Ok(serde_json::json!({
            "ok": true,
            "bytes": markdown.len(),
            "plan_done": progress.done,
            "plan_total": progress.total,
        }))
    }
}

pub struct GoalBlockTool {
    goal_id: String,
}

impl GoalBlockTool {
    pub fn new(goal_id: String) -> Self {
        Self { goal_id }
    }
}

#[async_trait]
impl metalcraft::Tool for GoalBlockTool {
    fn name(&self) -> &str {
        "goal_block"
    }

    fn description(&self) -> &str {
        "Stop the heartbeat and put a question to the person who set this goal. Use it sparingly: \
         a blocked goal makes no progress until someone happens to look, which overnight is hours \
         of nothing. Block only when the call is irreversible (deleting data, force-pushing, \
         anything public), spends money, or would change what the goal means. For an ordinary \
         choice between reasonable options, decide it, write the decision and your reasoning into \
         the scratchpad's State, and keep going."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "What you need answered, phrased so it can be answered without reading the whole scratchpad. Offer the options you see."
                },
                "reason": {
                    "type": "string",
                    "description": "Why you cannot decide this yourself — which of irreversible / costs money / changes the goal it is."
                }
            },
            "required": ["question"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let question = args["question"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| err("goal_block", "Missing required parameter: question"))?;
        let reason = args["reason"].as_str().map(str::trim).unwrap_or_default();

        let mut goal = goals::get(&self.goal_id)
            .ok_or_else(|| err("goal_block", "this goal no longer exists"))?;
        goal.status = goals::GoalStatus::Blocked;
        goal.blocked_reason = Some(if reason.is_empty() {
            question.to_string()
        } else {
            format!("{question}\n\n({reason})")
        });
        goals::save(&goal).map_err(|e| err("goal_block", e))?;

        let current = goals::read_scratchpad(&self.goal_id).unwrap_or_default();
        let updated = goals::append_to_section(
            &current,
            "Questions for the human",
            &format!("- {}", question.replace('\n', " ")),
        );
        let _ = goals::write_scratchpad(&self.goal_id, &updated);

        Ok(serde_json::json!({
            "ok": true,
            "status": "blocked",
            "note": "The heartbeat is stopped until someone answers. Finish your scratchpad write, then end the tick."
        }))
    }
}

pub struct GoalCompleteTool {
    goal_id: String,
}

impl GoalCompleteTool {
    pub fn new(goal_id: String) -> Self {
        Self { goal_id }
    }
}

#[async_trait]
impl metalcraft::Tool for GoalCompleteTool {
    fn name(&self) -> &str {
        "goal_complete"
    }

    fn description(&self) -> &str {
        "Declare the goal met and stop the heartbeat. Only call this when every plan step is \
         genuinely done and verified — not when the remaining work merely looks small. If part \
         of it turned out to be impossible or unwanted, say so in the summary rather than \
         quietly leaving it out."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "What was achieved, and anything deliberately left undone. Written for someone who has read none of the ticks."
                }
            },
            "required": ["summary"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let summary = args["summary"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| err("goal_complete", "Missing required parameter: summary"))?;

        let mut goal = goals::get(&self.goal_id)
            .ok_or_else(|| err("goal_complete", "this goal no longer exists"))?;

        // A goal that says it is done while its own plan says otherwise is the
        // failure this whole design is arranged against, so it is refused rather
        // than recorded. Refusing returns control to the agent, which can either
        // finish the step or uncheck the claim.
        let scratchpad = goals::read_scratchpad(&self.goal_id).unwrap_or_default();
        let progress = goals::progress_of(&scratchpad);
        if progress.total > 0 && progress.done < progress.total {
            return Err(err(
                "goal_complete",
                format!(
                    "{} of {} plan steps are still unchecked. Either finish them, or — if they \
                     turned out to be unnecessary — rewrite the plan with goal_scratchpad_write \
                     saying so, then complete.",
                    progress.total - progress.done,
                    progress.total
                ),
            ));
        }

        goal.status = goals::GoalStatus::Done;
        goal.blocked_reason = None;
        goals::save(&goal).map_err(|e| err("goal_complete", e))?;

        let updated = goals::append_to_section(
            &scratchpad,
            "Log",
            &format!("- **Goal complete.** {}", summary.replace('\n', " ")),
        );
        let _ = goals::write_scratchpad(&self.goal_id, &updated);

        Ok(serde_json::json!({ "ok": true, "status": "done" }))
    }
}

pub struct GoalAwaitRunTool {
    goal_id: String,
}

impl GoalAwaitRunTool {
    pub fn new(goal_id: String) -> Self {
        Self { goal_id }
    }
}

#[async_trait]
impl metalcraft::Tool for GoalAwaitRunTool {
    fn name(&self) -> &str {
        "goal_await_run"
    }

    fn description(&self) -> &str {
        "Hand a long-running command back to the heartbeat instead of waiting for it. Call this \
         straight after starting a build or a test run that will outlive this tick (buildr's \
         build/test return a run id and keep going without you). Then finish your scratchpad and \
         end the tick: the next wake-up reads the result for you — without spending a model on \
         it — and hands you the outcome. Do not sit in a polling loop; that burns the tick on \
         waiting and the run finishes after you are gone either way."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace_id": {
                    "type": "string",
                    "description": "The buildr.space workspace the command is running in."
                },
                "run_id": {
                    "type": "string",
                    "description": "The run id the build/test call returned."
                },
                "what": {
                    "type": "string",
                    "description": "The command, for the log — 'cargo test', 'npm run build'."
                }
            },
            "required": ["workspace_id", "run_id"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let workspace_id = args["workspace_id"]
            .as_str()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .ok_or_else(|| err("goal_await_run", "Missing required parameter: workspace_id"))?;
        let run_id = args["run_id"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| err("goal_await_run", "Missing required parameter: run_id"))?;
        let what = args["what"].as_str().map(str::trim).unwrap_or("a command");

        let mut goal = goals::get(&self.goal_id)
            .ok_or_else(|| err("goal_await_run", "this goal no longer exists"))?;

        // One at a time. A goal that started three builds and remembered one
        // would wait on that one and silently lose the others — and a tick that
        // needs two commands at once can await the second one next tick, which
        // is the shape the heartbeat is for.
        if let Some(existing) = &goal.pending_run {
            return Err(err(
                "goal_await_run",
                format!(
                    "Already waiting on `{}` (run {}). Let that one land first — the next tick \
                     will hand you its result.",
                    existing.what, existing.run_id
                ),
            ));
        }

        goal.pending_run = Some(goals::PendingRun {
            workspace_id: workspace_id.to_string(),
            run_id: run_id.to_string(),
            what: what.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        goals::save(&goal).map_err(|e| err("goal_await_run", e))?;

        let current = goals::read_scratchpad(&self.goal_id).unwrap_or_default();
        let updated = goals::append_to_section(
            &current,
            "Log",
            &format!("- Started `{what}` (run {run_id}); handed to the next tick."),
        );
        let _ = goals::write_scratchpad(&self.goal_id, &updated);

        Ok(serde_json::json!({
            "ok": true,
            "note": "Recorded. Finish your scratchpad and end the tick — the result will be waiting for the next one."
        }))
    }
}

// ── the audit ledger ─────────────────────────────────────────────────────────

pub struct GoalFindingTool {
    goal_id: String,
}

impl GoalFindingTool {
    pub fn new(goal_id: String) -> Self {
        Self { goal_id }
    }
}

fn severity_of(raw: &str) -> Result<crate::goal_findings::Severity, metalcraft::GraphError> {
    use crate::goal_findings::Severity;
    match raw {
        "high" => Ok(Severity::High),
        "medium" => Ok(Severity::Medium),
        "low" => Ok(Severity::Low),
        other => Err(err(
            "goal_finding",
            format!("Unknown severity '{other}'. One of: high, medium, low."),
        )),
    }
}

#[async_trait]
impl metalcraft::Tool for GoalFindingTool {
    fn name(&self) -> &str {
        "goal_finding"
    }

    fn description(&self) -> &str {
        "Record something you found, in the goal's findings ledger. The ledger is what stops you \
         re-reporting on tick 9 what you already opened a PR for on tick 4 — so record every \
         finding here as you find it, even the ones you do not intend to fix. A finding you have \
         already recorded comes back with its existing id and state instead of being added twice."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "One line: what is wrong. Specific enough that another sweep would recognise it as the same thing."
                },
                "file": {
                    "type": "string",
                    "description": "Where, as `path/to/file.rs:42`. Strongly preferred — it is half the dedupe key."
                },
                "severity": {
                    "type": "string",
                    "enum": ["high", "medium", "low"],
                    "description": "high = wrong and will bite (bug, security, data loss). medium = worth fixing. low = tidying."
                },
                "detail": {
                    "type": "string",
                    "description": "The evidence: what the code does, and what it should do. This becomes the PR body, so write it for a reviewer who has not read the file."
                }
            },
            "required": ["title", "severity"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let title = args["title"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| err("goal_finding", "Missing required parameter: title"))?;
        let severity = severity_of(args["severity"].as_str().unwrap_or("medium"))?;
        let file = args["file"].as_str().map(str::trim).filter(|f| !f.is_empty());
        let detail = args["detail"].as_str().unwrap_or_default();

        let (finding, already) =
            crate::goal_findings::add(&self.goal_id, title, file, severity, detail)
                .map_err(|e| err("goal_finding", e))?;

        Ok(serde_json::json!({
            "id": finding.id,
            "already_known": already,
            "state": finding.state,
            "link": finding.link,
            "note": if already {
                "You already found this. Do not open a second PR for it."
            } else {
                "Recorded."
            },
        }))
    }
}

pub struct GoalFindingUpdateTool {
    goal_id: String,
}

impl GoalFindingUpdateTool {
    pub fn new(goal_id: String) -> Self {
        Self { goal_id }
    }
}

#[async_trait]
impl metalcraft::Tool for GoalFindingUpdateTool {
    fn name(&self) -> &str {
        "goal_finding_update"
    }

    fn description(&self) -> &str {
        "Move a finding along: you opened a PR for it (`pr_open`, with the PR url), filed an issue \
         instead (`issue_open`), it merged (`merged`), or it turned out not to be worth doing \
         (`rejected`). Recording a rejection matters as much as recording a fix — without it the \
         next sweep finds the same thing again and argues for it again."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The finding id, e.g. 'f3'." },
                "state": {
                    "type": "string",
                    "enum": ["open", "pr_open", "issue_open", "merged", "rejected"],
                    "description": "Where it has got to."
                },
                "link": {
                    "type": "string",
                    "description": "The PR or issue url, when there is one."
                }
            },
            "required": ["id", "state"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        use crate::goal_findings::FindingState;

        let id = args["id"]
            .as_str()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| err("goal_finding_update", "Missing required parameter: id"))?;
        let state = match args["state"].as_str().unwrap_or("open") {
            "open" => FindingState::Open,
            "pr_open" => FindingState::PrOpen,
            "issue_open" => FindingState::IssueOpen,
            "merged" => FindingState::Merged,
            "rejected" => FindingState::Rejected,
            other => {
                return Err(err(
                    "goal_finding_update",
                    format!("Unknown state '{other}'."),
                ));
            }
        };
        let link = args["link"].as_str().map(str::trim).filter(|l| !l.is_empty());

        // The open-PR cap, enforced here rather than asked for in a prompt.
        // Twenty simultaneous bot PRs is how a repo learns to ignore the bot,
        // and a rail that only exists as advice is not a rail.
        if state.holds_a_pr_slot() {
            let goal = goals::get(&self.goal_id)
                .ok_or_else(|| err("goal_finding_update", "this goal no longer exists"))?;
            let already = crate::goal_findings::list(&self.goal_id)
                .iter()
                .filter(|f| f.id != id && f.state.holds_a_pr_slot())
                .count();
            if already >= goal.rails.max_open_prs as usize {
                return Err(err(
                    "goal_finding_update",
                    format!(
                        "{already} of this goal's PRs are already open, which is its limit. Keep \
                         sweeping and recording findings; open the next PR once one of those is \
                         merged or closed.",
                    ),
                ));
            }
        }

        let finding = crate::goal_findings::set_state(&self.goal_id, id, state, link)
            .map_err(|e| err("goal_finding_update", e))?;

        Ok(serde_json::json!({
            "ok": true,
            "id": finding.id,
            "state": finding.state,
            "open_prs": crate::goal_findings::open_prs(&self.goal_id),
        }))
    }
}
