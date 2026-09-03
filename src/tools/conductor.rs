//! The two tools the conductor uses to keep its own memory.
//!
//! The conductor already has `mem_*` — instance memory, ranked recall, distilled
//! nightly. This is the other half, and the difference matters: recall is fuzzy
//! and returns what looks relevant, while a conductor needs a document that is
//! *complete and verbatim every tick* — its bearing, what it has tried, what it
//! is watching. That is what the scratchpad is to the worker, and the reason it
//! is a document rather than a memory is the same reason.
//!
//! Bound to one project at registration, like every other `project_*` tool, so
//! the model never names which ledger it is writing to.

use async_trait::async_trait;

use crate::project_conductor as ledger;

fn err(tool: &str, message: impl Into<String>) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

pub struct ConductorWriteTool {
    project_id: String,
}

impl ConductorWriteTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ConductorWriteTool {
    fn name(&self) -> &str {
        "conductor_write"
    }

    fn description(&self) -> &str {
        "Replace your ledger — your own memory of this project, which is the only thing you carry \
         between ticks. Pass the whole document, keeping every '## ' heading: Bearing, Learned, \
         Tried, Watching.\n\n\
         What belongs in each: **Bearing** is what you currently believe this project should do \
         next and why — your running thesis, rewritten as it changes. **Learned** is what you now \
         know about working THIS project: how it builds, what its tests need, which delegate is \
         reliable at what. **Tried** is what has been attempted and how it went, so a later tick \
         does not re-run a dead end; the runner appends the worker's results here for you, and \
         your job is to fold the old ones into Learned rather than let the list grow. **Watching** \
         is what would change your mind.\n\n\
         Write for a stranger: the next tick is one. Anything that outlives this project entirely \
         belongs in `mem_remember` instead."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "markdown": {
                    "type": "string",
                    "description": "The complete ledger, every section heading kept."
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
            .ok_or_else(|| err("conductor_write", "Missing required parameter: markdown"))?;

        // A rewrite that dropped a section would take the frame's own vocabulary
        // with it — the tick prompt refers to these by name. Refuse rather than
        // silently accept a document nothing can act on.
        let missing: Vec<&str> = ledger::SECTIONS
            .iter()
            .copied()
            .filter(|s| crate::projects::section_body(markdown, s).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(err(
                "conductor_write",
                format!(
                    "the ledger is missing {}. Keep every heading — an empty section says \
                     '(none)', it does not disappear.",
                    missing
                        .iter()
                        .map(|s| format!("`## {s}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }

        ledger::write(&self.project_id, markdown)
            .map_err(|e| err("conductor_write", format!("could not write the ledger: {e}")))?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

pub struct ConductorNoteTool {
    project_id: String,
}

impl ConductorNoteTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ConductorNoteTool {
    fn name(&self) -> &str {
        "conductor_note"
    }

    fn description(&self) -> &str {
        "Append one line to your ledger. Cheaper than rewriting it, for a single thing learned or \
         a single thing to watch. To reorganise the document — folding old attempts into what you \
         have learned — use conductor_write."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "section": {
                    "type": "string",
                    "enum": ledger::SECTIONS,
                    "description": "Which section to append to."
                },
                "text": {
                    "type": "string",
                    "description": "One line, concrete enough to be useful to a tick that remembers none of this."
                }
            },
            "required": ["section", "text"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let section = args["section"]
            .as_str()
            .ok_or_else(|| err("conductor_note", "Missing required parameter: section"))?;
        if !ledger::SECTIONS.contains(&section) {
            return Err(err(
                "conductor_note",
                format!("Unknown section '{section}'. One of: {}", ledger::SECTIONS.join(", ")),
            ));
        }
        let text = args["text"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| err("conductor_note", "Missing required parameter: text"))?;

        ledger::append(&self.project_id, section, &format!("- {}", text.replace('\n', " ")))
            .map_err(|e| err("conductor_note", format!("could not write the ledger: {e}")))?;
        Ok(serde_json::json!({ "ok": true, "section": section }))
    }
}

// ── pace ─────────────────────────────────────────────────────────────────────

/// Back the project off to a slower heartbeat — and only ever slower.
///
/// The conductor can see what a person cannot from outside: that the thing this
/// project is waiting on will not move today, that three ticks in a row have
/// found nothing to do, that the work has gone quiet. Waking every fifteen
/// minutes to confirm that is a cost with no return.
///
/// **It cannot speed up past what the person asked for.** That asymmetry is the
/// whole reason this is safe to give a model: a model deciding how often to spend
/// money is exactly the concern that got per-delegation model tiers deferred, and
/// it disappears when the only direction it can move is cheaper. Coming back to
/// the person's own pace is allowed — that is not spending more than was
/// authorised, it is stopping economising.
pub struct ConductorPaceTool {
    project_id: String,
}

impl ConductorPaceTool {
    pub fn new(project_id: String) -> Self {
        Self { project_id }
    }
}

#[async_trait]
impl metalcraft::Tool for ConductorPaceTool {
    fn name(&self) -> &str {
        "project_pace"
    }

    fn description(&self) -> &str {
        "Slow this project's heartbeat down, when waking as often as you do is buying nothing. \
         Good reasons: what you are waiting on will not move today; several ticks in a row have \
         found nothing to do; the work is genuinely paused on somebody else.\n\n\
         You can only slow DOWN. The interval a person set is the fastest this project may wake, \
         and passing that value (or a smaller one) simply returns to their pace, which is what to \
         do the moment there is real work again. Say why — it goes in the log, and a project that \
         quietly went hourly with no reason recorded looks broken."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "every_minutes": {
                    "type": "integer",
                    "description": "Minutes between ticks. Anything at or below the person's own setting means 'back to their pace'."
                },
                "why": {
                    "type": "string",
                    "description": "One line: what makes waking less often the right call now."
                }
            },
            "required": ["every_minutes", "why"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let minutes = args["every_minutes"]
            .as_u64()
            .ok_or_else(|| err("project_pace", "Missing required parameter: every_minutes"))?
            as u32;
        let why = args["why"]
            .as_str()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .ok_or_else(|| err("project_pace", "Missing required parameter: why"))?;

        let mut project = crate::projects::get(&self.project_id)
            .ok_or_else(|| err("project_pace", "this project no longer exists"))?;
        let asked_for = project.heartbeat.every_minutes;

        if minutes > crate::projects::MAX_CONDUCTOR_BACKOFF_MINUTES {
            return Err(err(
                "project_pace",
                format!(
                    "{minutes} minutes is slower than a day, which is not backing off — it is \
                     stopping without saying so. If this project should not be running, \
                     project_block and say what it is waiting for."
                ),
            ));
        }

        // At or below the person's pace means "stop economising", not "go
        // faster": their number is the ceiling on frequency either way.
        project.heartbeat.conductor_minutes = (minutes > asked_for).then_some(minutes);
        crate::projects::save(&project).map_err(|e| err("project_pace", e))?;

        let effective = project.tick_interval_minutes();
        let line = if project.heartbeat.conductor_minutes.is_some() {
            format!("- Slowed to every {effective} min: {why}")
        } else {
            format!("- Back to the {effective} min the project was set to: {why}")
        };
        let current = crate::projects::read_scratchpad(&self.project_id).unwrap_or_default();
        let _ = crate::projects::write_scratchpad(
            &self.project_id,
            &crate::projects::append_to_section(&current, "Log", &line),
        );

        Ok(serde_json::json!({
            "ok": true,
            "every_minutes": effective,
            "note": if minutes > asked_for {
                "Backed off. Pass the project's own interval to come back the moment there is work."
            } else {
                "Back to the pace the project was set to."
            },
        }))
    }
}
