//! The four tools a project tick uses to write down where it got to.
//!
//! Each is bound to one project at registration ([`ToolConfig::project_id`]), so the
//! model never names which project it is writing to — it cannot mistype it, and it
//! cannot write to another project's scratchpad.
//!
//! The split is deliberate. [`ProjectNoteTool`] is the cheap, frequent one: a line
//! of log, a blocker, a question. [`ProjectScratchpadWriteTool`] replaces the whole
//! document and is the tick's *final* act — wholesale rather than patched, for
//! the same reason `update_plan` is: a step silently abandoned shows up as a
//! deletion instead of sitting there forever, and there is never a question of
//! which write won.
//!
//! [`ProjectBlockTool`] and [`ProjectCompleteTool`] are the two ways a project stops. Both
//! are terminal for the *project*, not merely for the turn, which is why they are
//! tools rather than something inferred from what the agent said.
//!
//! [`ToolConfig::project_id`]: crate::tools::ToolConfig::project_id

use async_trait::async_trait;

use crate::projects;

/// Sections a note may be appended to.
///
/// Not the full section list: `Project` is immutable, and `Plan`/`State`/`Workspace`
/// are rewritten wholesale by the scratchpad write rather than appended to a line
/// at a time — appending to a plan is how a plan grows a second copy of itself.
const NOTE_SECTIONS: &[&str] = &["Log", "Blockers", "Questions for the human"];

fn err(tool: &str, message: impl Into<String>) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

pub struct ProjectNoteTool {
    project_id: String,
}

impl ProjectNoteTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ProjectNoteTool {
    fn name(&self) -> &str {
        "project_note"
    }

    fn description(&self) -> &str {
        "Append one line to your project's scratchpad. Use `Log` for what you just did (start it \
         with what changed, not with what you intended), `Blockers` for something stopping \
         progress that you intend to work around, and `Questions for the human` for something \
         you want asked but are not blocking on. To rewrite the plan or the state, use \
         project_scratchpad_write instead — this tool only appends."
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
            .ok_or_else(|| err("project_note", "Missing required parameter: section"))?;
        if !NOTE_SECTIONS.contains(&section) {
            return Err(err(
                "project_note",
                format!("Unknown section '{section}'. One of: {}", NOTE_SECTIONS.join(", ")),
            ));
        }
        let text = args["text"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| err("project_note", "Missing required parameter: text"))?;

        let current = projects::read_scratchpad(&self.project_id).unwrap_or_default();
        // One line means one line: a note carrying its own newlines would end up
        // as several list items, one of which is a fragment.
        let line = format!("- {}", text.replace('\n', " "));
        let updated = projects::append_to_section(&current, section, &line);
        projects::write_scratchpad(&self.project_id, &updated)
            .map_err(|e| err("project_note", format!("could not write scratchpad: {e}")))?;

        Ok(serde_json::json!({ "ok": true, "section": section }))
    }
}

pub struct ProjectScratchpadWriteTool {
    project_id: String,
}

impl ProjectScratchpadWriteTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ProjectScratchpadWriteTool {
    fn name(&self) -> &str {
        "project_scratchpad_write"
    }

    fn description(&self) -> &str {
        "Replace your project's whole scratchpad. This is the last thing you do in a tick, and the \
         only memory you carry to the next one — the tick that reads it will know nothing you \
         know now. Pass the complete document, keeping every '## ' section heading: Project, \
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
            .ok_or_else(|| err("project_scratchpad_write", "Missing required parameter: markdown"))?;

        // The project statement is the one thing a rewrite may not lose: a tick that
        // drops it leaves every later tick working towards nothing in particular,
        // and the loss is invisible because the document still looks well-formed.
        let mut markdown = markdown.to_string();
        if projects::section_body(&markdown, "Goal").is_none_or(str::is_empty) {
            let Some(project) = projects::get(&self.project_id) else {
                return Err(err("project_scratchpad_write", "this project no longer exists"));
            };
            markdown = projects::replace_section(&markdown, "Goal", project.goal.trim());
        }

        projects::write_scratchpad(&self.project_id, &markdown)
            .map_err(|e| err("project_scratchpad_write", format!("could not write scratchpad: {e}")))?;

        // From the task list when there is one. Counting checkboxes in the
        // document reported 0/0 for every project whose plan is records, which
        // is worse than saying nothing: it told the model its plan was empty
        // immediately after it had worked through it.
        let progress = if crate::project_tasks::exists(&self.project_id) {
            crate::project_tasks::progress(&crate::project_tasks::list(&self.project_id))
        } else {
            projects::progress_of(&markdown)
        };
        Ok(serde_json::json!({
            "ok": true,
            "bytes": markdown.len(),
            "plan_done": progress.done,
            "plan_total": progress.total,
        }))
    }
}

pub struct ProjectBlockTool {
    project_id: String,
}

impl ProjectBlockTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ProjectBlockTool {
    fn name(&self) -> &str {
        "project_block"
    }

    fn description(&self) -> &str {
        "Stop the heartbeat and put a question to the person who set this project. Use it sparingly: \
         a blocked project makes no progress until someone happens to look, which overnight is hours \
         of nothing. Block only when the call is irreversible (deleting data, force-pushing, \
         anything public), spends money, or would change what the project means. For an ordinary \
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
                    "description": "Why you cannot decide this yourself — which of irreversible / costs money / changes the project it is."
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
            .ok_or_else(|| err("project_block", "Missing required parameter: question"))?;
        let reason = args["reason"].as_str().map(str::trim).unwrap_or_default();

        let mut project = projects::get(&self.project_id)
            .ok_or_else(|| err("project_block", "this project no longer exists"))?;
        project.status = projects::ProjectStatus::Blocked;
        project.blocked_reason = Some(if reason.is_empty() {
            question.to_string()
        } else {
            format!("{question}\n\n({reason})")
        });
        projects::save(&project).map_err(|e| err("project_block", e))?;

        let current = projects::read_scratchpad(&self.project_id).unwrap_or_default();
        let updated = projects::append_to_section(
            &current,
            "Questions for the human",
            &format!("- {}", question.replace('\n', " ")),
        );
        let _ = projects::write_scratchpad(&self.project_id, &updated);

        Ok(serde_json::json!({
            "ok": true,
            "status": "blocked",
            "note": "The heartbeat is stopped until someone answers. Finish your scratchpad write, then end the tick."
        }))
    }
}

pub struct ProjectCompleteTool {
    project_id: String,
}

impl ProjectCompleteTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ProjectCompleteTool {
    fn name(&self) -> &str {
        "project_complete"
    }

    fn description(&self) -> &str {
        "Declare the project met and stop the heartbeat. Only call this when every plan step is \
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
            .ok_or_else(|| err("project_complete", "Missing required parameter: summary"))?;

        let mut project = projects::get(&self.project_id)
            .ok_or_else(|| err("project_complete", "this project no longer exists"))?;

        // A project that says it is done while its own plan says otherwise is the
        // failure this whole design is arranged against, so it is refused rather
        // than recorded. Refusing returns control to the agent, which can either
        // finish the step or uncheck the claim.
        let scratchpad = projects::read_scratchpad(&self.project_id).unwrap_or_default();
        // `Project::progress` reads the task list when there is one and falls back
        // to the scratchpad's checkboxes when there is not, so this one check
        // covers both kinds of project.
        let progress = project.progress();
        if progress.total > 0 && progress.done < progress.total {
            let how = if crate::project_tasks::exists(&self.project_id) {
                "Either finish them, or — if they turned out to be unnecessary — `task_drop` \
                 each one saying what changed, then complete."
            } else {
                "Either finish them, or — if they turned out to be unnecessary — rewrite the \
                 plan with project_scratchpad_write saying so, then complete."
            };
            return Err(err(
                "project_complete",
                format!(
                    "{} of {} plan steps are still open. {how}",
                    progress.total - progress.done,
                    progress.total
                ),
            ));
        }

        project.status = projects::ProjectStatus::Done;
        project.blocked_reason = None;
        projects::save(&project).map_err(|e| err("project_complete", e))?;

        let updated = projects::append_to_section(
            &scratchpad,
            "Log",
            &format!("- **Project complete.** {}", summary.replace('\n', " ")),
        );
        let _ = projects::write_scratchpad(&self.project_id, &updated);

        Ok(serde_json::json!({ "ok": true, "status": "done" }))
    }
}

pub struct ProjectAwaitRunTool {
    project_id: String,
}

impl ProjectAwaitRunTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ProjectAwaitRunTool {
    fn name(&self) -> &str {
        "project_await_run"
    }

    fn description(&self) -> &str {
        "Hand a long-running command back to the heartbeat instead of waiting for it. Call this \
         straight after starting a build or a test run that will outlive this tick (buildr's \
         build/test return a run id and keep going without you). Then finish your scratchpad and \
         end the tick: the next wake-up reads the result for you — without spending a model on \
         it — and hands you the outcome. Do not sit in a polling loop; that burns the tick on \
         waiting and the run finishes after you are gone either way.\n\n\
         Name the `task_id` the run belongs to whenever there is one. A run recorded against a \
         task parks only that task — every other task keeps moving, and several runs can be in \
         flight at once. Without a task_id the run is the whole project's, and the project may only \
         have one at a time."
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
                },
                "task_id": {
                    "type": "string",
                    "description": "The task this run belongs to, e.g. 't3'. Parks that task only; its siblings keep working, and other tasks may have their own runs in flight at the same time."
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
            .ok_or_else(|| err("project_await_run", "Missing required parameter: workspace_id"))?;
        let run_id = args["run_id"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| err("project_await_run", "Missing required parameter: run_id"))?;
        let what = args["what"].as_str().map(str::trim).unwrap_or("a command");

        let pending = projects::PendingRun {
            workspace_id: workspace_id.to_string(),
            run_id: run_id.to_string(),
            what: what.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };

        // A run that names a task belongs to that task: it parks that row and
        // nothing else, which is what lets a project have three builds going at
        // once. The project-level slot below stays single because a run nobody
        // owns has nothing to keep separate.
        if let Some(task_id) = args["task_id"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let tasks = crate::project_tasks::list(&self.project_id);
            if let Some(existing) = crate::project_tasks::get(&tasks, task_id)
                && let Some(run) = existing.pending_run
            {
                return Err(err(
                    "project_await_run",
                    format!(
                        "task '{task_id}' is already waiting on `{}` (run {}). Let that land \
                         first, or record this run against a different task.",
                        run.what, run.run_id
                    ),
                ));
            }
            crate::project_tasks::update(
                &self.project_id,
                task_id,
                crate::project_tasks::TaskPatch {
                    status: Some(crate::project_tasks::TaskStatus::Waiting),
                    pending_run: Some(Some(pending)),
                    ..Default::default()
                },
            )
            .map_err(|e| err("project_await_run", e))?;

            let current = projects::read_scratchpad(&self.project_id).unwrap_or_default();
            let updated = projects::append_to_section(
                &current,
                "Log",
                &format!("- **{task_id}** started `{what}` (run {run_id}); handed to the heartbeat."),
            );
            let _ = projects::write_scratchpad(&self.project_id, &updated);

            return Ok(serde_json::json!({
                "ok": true,
                "task_id": task_id,
                "note": "Recorded against that task. Its siblings are unaffected — carry on with anything else that is ready, then end the tick."
            }));
        }

        let mut project = projects::get(&self.project_id)
            .ok_or_else(|| err("project_await_run", "this project no longer exists"))?;

        // One at a time. A project that started three builds and remembered one
        // would wait on that one and silently lose the others — and a tick that
        // needs two commands at once can await the second one next tick, which
        // is the shape the heartbeat is for.
        if let Some(existing) = &project.pending_run {
            return Err(err(
                "project_await_run",
                format!(
                    "Already waiting on `{}` (run {}). Let that one land first — the next tick \
                     will hand you its result.",
                    existing.what, existing.run_id
                ),
            ));
        }

        project.pending_run = Some(pending);
        projects::save(&project).map_err(|e| err("project_await_run", e))?;

        let current = projects::read_scratchpad(&self.project_id).unwrap_or_default();
        let updated = projects::append_to_section(
            &current,
            "Log",
            &format!("- Started `{what}` (run {run_id}); handed to the next tick."),
        );
        let _ = projects::write_scratchpad(&self.project_id, &updated);

        Ok(serde_json::json!({
            "ok": true,
            "note": "Recorded. Finish your scratchpad and end the tick — the result will be waiting for the next one."
        }))
    }
}

// ── the audit ledger ─────────────────────────────────────────────────────────

pub struct ProjectFindingTool {
    project_id: String,
}

impl ProjectFindingTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

fn severity_of(raw: &str) -> Result<crate::project_findings::Severity, metalcraft::GraphError> {
    use crate::project_findings::Severity;
    match raw {
        "high" => Ok(Severity::High),
        "medium" => Ok(Severity::Medium),
        "low" => Ok(Severity::Low),
        other => Err(err(
            "project_finding",
            format!("Unknown severity '{other}'. One of: high, medium, low."),
        )),
    }
}

#[async_trait]
impl metalcraft::Tool for ProjectFindingTool {
    fn name(&self) -> &str {
        "project_finding"
    }

    fn description(&self) -> &str {
        "Record something you found, in the project's findings ledger. The ledger is what stops you \
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
            .ok_or_else(|| err("project_finding", "Missing required parameter: title"))?;
        let severity = severity_of(args["severity"].as_str().unwrap_or("medium"))?;
        let file = args["file"].as_str().map(str::trim).filter(|f| !f.is_empty());
        let detail = args["detail"].as_str().unwrap_or_default();

        let (finding, already) =
            crate::project_findings::add(&self.project_id, title, file, severity, detail)
                .map_err(|e| err("project_finding", e))?;

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

pub struct ProjectFindingUpdateTool {
    project_id: String,
}

impl ProjectFindingUpdateTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ProjectFindingUpdateTool {
    fn name(&self) -> &str {
        "project_finding_update"
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
        use crate::project_findings::FindingState;

        let id = args["id"]
            .as_str()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| err("project_finding_update", "Missing required parameter: id"))?;
        let state = match args["state"].as_str().unwrap_or("open") {
            "open" => FindingState::Open,
            "pr_open" => FindingState::PrOpen,
            "issue_open" => FindingState::IssueOpen,
            "merged" => FindingState::Merged,
            "rejected" => FindingState::Rejected,
            other => {
                return Err(err(
                    "project_finding_update",
                    format!("Unknown state '{other}'."),
                ));
            }
        };
        let link = args["link"].as_str().map(str::trim).filter(|l| !l.is_empty());

        // The open-PR cap, enforced here rather than asked for in a prompt.
        // Twenty simultaneous bot PRs is how a repo learns to ignore the bot,
        // and a rail that only exists as advice is not a rail.
        if state.holds_a_pr_slot() {
            let project = projects::get(&self.project_id)
                .ok_or_else(|| err("project_finding_update", "this project no longer exists"))?;
            let already = crate::project_findings::list(&self.project_id)
                .iter()
                .filter(|f| f.id != id && f.state.holds_a_pr_slot())
                .count();
            if already >= project.rails.max_open_prs as usize {
                return Err(err(
                    "project_finding_update",
                    format!(
                        "{already} of this project's PRs are already open, which is its limit. Keep \
                         sweeping and recording findings; open the next PR once one of those is \
                         merged or closed.",
                    ),
                ));
            }
        }

        let finding = crate::project_findings::set_state(&self.project_id, id, state, link)
            .map_err(|e| err("project_finding_update", e))?;

        Ok(serde_json::json!({
            "ok": true,
            "id": finding.id,
            "state": finding.state,
            "open_prs": crate::project_findings::open_prs(&self.project_id),
        }))
    }
}
