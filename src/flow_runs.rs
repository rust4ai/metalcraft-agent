//! Persisted flow-run records for the v2 executor's pause/resume durability.
//!
//! A [`FlowRun`] is written to `runs/{id}.json` when a flow pauses at an
//! `approval` or `wait` node, and updated as it resumes and eventually
//! terminates. Runs that never pause are not persisted here — they return their
//! summary directly. This mirrors the `flows/` fs backend; there is no database.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

use crate::flow_exec::FlowStep;

/// Why a run is paused and what will resume it.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PauseInfo {
    /// `"approval"` or `"wait"`.
    pub reason: String,
    /// For `approval`: the decision handles a human may choose. For `wait`:
    /// typically `["after"]`.
    pub resume_handles: Vec<String>,
    /// For `approval`: the (interpolated) prompt shown to the human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// For `wait`: RFC-3339 timestamp at/after which the run may resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<String>,
}

/// A persisted flow run.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FlowRun {
    /// Unique run id (the `runs/{id}.json` filename).
    pub id: String,
    /// The flow this run belongs to.
    pub flow_id: String,
    /// `running` | `paused` | `completed` | `failed`.
    pub status: String,
    /// The node the run is paused at (for `paused`) or last ran.
    pub current_node_id: String,
    /// The run's state (`variables`) at the checkpoint.
    #[schema(value_type = Object)]
    pub variables: Value,
    /// Pause details when `status == "paused"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<PauseInfo>,
    /// Persona/model/cwd needed to resume the run.
    pub persona: String,
    /// Model name to resume with.
    pub model: String,
    /// Working directory to resume in.
    pub cwd: String,
    /// The trace accumulated so far.
    pub steps: Vec<FlowStep>,
    /// Snapshot of the flow definition at pause time, so resume routes against
    /// the graph the run actually paused in — not a since-edited on-disk flow.
    /// Absent (`None`) on legacy records; resume then falls back to loading the
    /// current flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub flow: Option<metalcraft_flows::SavedFlow>,
    /// Non-fatal warnings computed when the run started — e.g. required packs/personas
    /// that aren't installed or enabled. Surfaced in flow-debug UIs so a run that can't
    /// fully work says why. Empty (and omitted) when the flow has everything it needs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// RFC-3339 creation timestamp.
    pub created_at: String,
    /// RFC-3339 last-update timestamp.
    pub updated_at: String,
}

fn run_path(dir: &Path, id: &str) -> std::path::PathBuf {
    dir.join(format!("{id}.json"))
}

/// Persist (create or overwrite) a run record.
pub fn save_run(dir: &Path, run: &FlowRun) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(run).map_err(std::io::Error::other)?;
    std::fs::write(run_path(dir, &run.id), json)
}

/// Load a run record by id.
pub fn load_run(dir: &Path, id: &str) -> Option<FlowRun> {
    let content = std::fs::read_to_string(run_path(dir, id)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Delete a run record. Returns whether a file was removed.
pub fn delete_run(dir: &Path, id: &str) -> bool {
    std::fs::remove_file(run_path(dir, id)).is_ok()
}

/// List all persisted runs (unordered).
pub fn list_runs(dir: &Path) -> Vec<FlowRun> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|c| serde_json::from_str::<FlowRun>(&c).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(id: &str) -> FlowRun {
        FlowRun {
            id: id.into(),
            flow_id: "f".into(),
            status: "paused".into(),
            current_node_id: "approve".into(),
            variables: json!({ "x": 1 }),
            pause: Some(PauseInfo {
                reason: "approval".into(),
                resume_handles: vec!["approve".into(), "reject".into()],
                message: Some("ok?".into()),
                wake_at: None,
            }),
            persona: "coding-agent".into(),
            model: "m".into(),
            cwd: ".".into(),
            steps: vec![],
            flow: None,
            warnings: vec![],
            created_at: "2026-07-27T00:00:00Z".into(),
            updated_at: "2026-07-27T00:00:00Z".into(),
        }
    }

    #[test]
    fn round_trip_and_list_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let run = sample("r1");
        save_run(dir.path(), &run).unwrap();

        let loaded = load_run(dir.path(), "r1").unwrap();
        assert_eq!(loaded.status, "paused");
        assert_eq!(loaded.pause.unwrap().resume_handles.len(), 2);

        assert_eq!(list_runs(dir.path()).len(), 1);
        assert!(delete_run(dir.path(), "r1"));
        assert!(load_run(dir.path(), "r1").is_none());
        assert!(list_runs(dir.path()).is_empty());
    }
}
