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
