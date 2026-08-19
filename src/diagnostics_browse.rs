//! Read-only browsing of the diagnostics sessions that [`crate::diagnostics`]
//! writes under `sessions/<timestamp>/`. Shared by the workshop HTTP API and
//! the `diagnostics_*` meta tools so the GUI and the prompt-driven path
//! reconstruct a run identically. Pure readers — they never mutate session
//! data.

use serde::Serialize;

use crate::paths;

/// One session in the listing (metadata + turn count, no per-turn payloads).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DiagnosticsSessionSummary {
    pub id: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona_slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// "session" for a normal run, "flow" for a flow run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Present (and `kind == "flow"`) when produced by a flow run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    /// The agent this session belongs to. Absent on sessions written before agents
    /// existed, and on CLI runs, which have no agent.
    ///
    /// This is what lets a Sessions list answer "which agent produced this?" — the
    /// question that matters most for a background agent, whose failures arrive here
    /// with nobody watching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Count of `turn_NNN.json` files — how far the run actually got.
    pub turn_count: usize,
}

/// A fully reconstructed session: its `session_info.json` plus an ordered
/// timeline of every other JSON event file in the directory.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DiagnosticsSession {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_info: Option<serde_json::Value>,
    pub timeline: Vec<TimelineEvent>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TimelineEvent {
    pub kind: String,
    pub file: String,
    pub data: serde_json::Value,
}

/// List every diagnostics session, newest first.
pub fn list_diagnostics_sessions() -> Vec<DiagnosticsSessionSummary> {
    let dir = paths::sessions_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return vec![],
    };

    let mut sessions: Vec<DiagnosticsSessionSummary> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let dir_name = path.file_name()?.to_str()?.to_string();

            let info_path = path.join("session_info.json");
            let (persona_slug, model_name, kind, flow_id, instance_id) =
                if let Ok(content) = std::fs::read_to_string(&info_path) {
                    let info: serde_json::Value =
                        serde_json::from_str(&content).unwrap_or_default();
                    let field = |k: &str| {
                        info.get(k).and_then(|v| v.as_str()).map(String::from)
                    };
                    (
                        field("persona_slug"),
                        field("model_name"),
                        field("kind"),
                        field("flow_id"),
                        field("instance_id"),
                    )
                } else {
                    (None, None, None, None, None)
                };

            let turn_count = std::fs::read_dir(&path)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| {
                            e.file_name()
                                .to_str()
                                .map(|n| n.starts_with("turn_") && n.ends_with(".json"))
                                .unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0);

            Some(DiagnosticsSessionSummary {
                id: dir_name.clone(),
                timestamp: dir_name,
                persona_slug,
                model_name,
                kind,
                flow_id,
                instance_id,
                turn_count,
            })
        })
        .collect();

    sessions.sort_by(|a, b| b.id.cmp(&a.id)); // newest first
    sessions
}

/// Reconstruct one session by id. Returns `None` when no such session dir
/// exists.
pub fn read_diagnostics_session(id: &str) -> Option<DiagnosticsSession> {
    let session_dir = paths::sessions_dir().join(id);
    if !session_dir.is_dir() {
        return None;
    }

    let session_info = std::fs::read_to_string(session_dir.join("session_info.json"))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok());

    let mut timeline = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&session_dir) {
        let mut files: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        files.sort_by_key(|e| e.file_name());

        for entry in files {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "session_info.json" || !name.ends_with(".json") {
                continue;
            }

            let kind = if name.starts_with("turn_") {
                "turn"
            } else if name.starts_with("llm_request_") {
                "llm_request"
            } else if name.contains("compaction") {
                "compaction"
            } else if name.starts_with("error_") {
                "error"
            } else {
                "config_change"
            };

            let data = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|c| serde_json::from_str(&c).ok())
                .unwrap_or(serde_json::Value::Null);

            timeline.push(TimelineEvent {
                kind: kind.to_string(),
                file: name,
                data,
            });
        }
    }

    Some(DiagnosticsSession {
        id: id.to_string(),
        session_info,
        timeline,
    })
}
