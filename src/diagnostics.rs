//! Diagnostics logger for full LLM call logging.
//!
//! When `--diagnostics` is passed, creates a timestamped session directory under
//! `logs/` and writes:
//! - `session_info.json` — persona, model, tools, skills, system prompt, cwd
//! - `turn_NNN.json` — full message array after each agent step

use metalcraft::{AgentMessage, AgentState, LlmCallSnapshot};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Logger that writes full diagnostics to a session directory.
pub struct DiagnosticsLogger {
    session_dir: PathBuf,
    turn_counter: AtomicUsize,
}

impl DiagnosticsLogger {
    /// Create a new diagnostics logger. Creates the session directory immediately.
    pub fn new() -> std::io::Result<Self> {
        let timestamp = chrono_timestamp();
        let session_dir = PathBuf::from("logs").join(&timestamp);
        std::fs::create_dir_all(&session_dir)?;
        Ok(Self {
            session_dir,
            turn_counter: AtomicUsize::new(0),
        })
    }

    /// Returns the session directory path.
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Write session_info.json with startup configuration.
    pub fn log_session_info(
        &self,
        persona_name: &str,
        persona_slug: &str,
        model_name: &str,
        cwd: &str,
        system_prompt: &str,
        tools: &[String],
        skills: &[String],
        auto_approve: bool,
    ) {
        let info = json!({
            "timestamp": chrono_timestamp(),
            "persona_name": persona_name,
            "persona_slug": persona_slug,
            "model_name": model_name,
            "cwd": cwd,
            "system_prompt": system_prompt,
            "tools": tools,
            "skills": skills,
            "auto_approve": auto_approve,
        });
        let path = self.session_dir.join("session_info.json");
        if let Err(e) = write_json(&path, &info) {
            eprintln!("diagnostics: failed to write session_info.json: {e}");
        }
    }

    /// Log the full agent state after a step.
    pub fn log_turn(&self, state: &AgentState) {
        let turn = self.turn_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let messages: Vec<serde_json::Value> = state
            .messages
            .iter()
            .map(serialize_message)
            .collect();

        let turn_data = json!({
            "turn": turn,
            "message_count": messages.len(),
            "messages": messages,
        });

        let filename = format!("turn_{:03}.json", turn);
        let path = self.session_dir.join(&filename);
        if let Err(e) = write_json(&path, &turn_data) {
            eprintln!("diagnostics: failed to write {filename}: {e}");
        }
    }

    /// Log the raw LLM request context (system prompt + messages + tools).
    pub fn log_llm_request(&self, snapshot: &LlmCallSnapshot) {
        let turn = self.turn_counter.load(Ordering::Relaxed) + 1;
        let filename = format!("llm_request_{:03}.json", turn);
        let path = self.session_dir.join(&filename);
        let value = serde_json::to_value(snapshot).unwrap_or_default();
        if let Err(e) = write_json(&path, &value) {
            eprintln!("diagnostics: failed to write {filename}: {e}");
        }
    }

    /// Log a configuration change (persona switch, model switch, etc.).
    pub fn log_config_change(&self, event: &str, details: serde_json::Value) {
        let turn = self.turn_counter.load(Ordering::Relaxed);
        let data = json!({
            "event": event,
            "after_turn": turn,
            "details": details,
        });
        let filename = format!("{}_after_turn_{:03}.json", event, turn);
        let path = self.session_dir.join(&filename);
        if let Err(e) = write_json(&path, &data) {
            eprintln!("diagnostics: failed to write {filename}: {e}");
        }
    }

    /// Log a context compaction event.
    pub fn log_compaction(&self, before_tokens: usize, after_tokens: usize) {
        let turn = self.turn_counter.load(Ordering::Relaxed);
        let data = json!({
            "event": "compaction",
            "after_turn": turn,
            "before_tokens": before_tokens,
            "after_tokens": after_tokens,
        });
        let filename = format!("compaction_after_turn_{:03}.json", turn);
        let path = self.session_dir.join(&filename);
        if let Err(e) = write_json(&path, &data) {
            eprintln!("diagnostics: failed to write {filename}: {e}");
        }
    }
}

fn serialize_message(msg: &AgentMessage) -> serde_json::Value {
    match msg {
        AgentMessage::User(text) => json!({
            "role": "user",
            "content": text,
        }),
        AgentMessage::Assistant(text) => json!({
            "role": "assistant",
            "content": text,
        }),
        AgentMessage::ToolCall { id, call_id, name, args } => json!({
            "role": "tool_call",
            "id": id,
            "call_id": call_id,
            "name": name,
            "args": args,
        }),
        AgentMessage::ToolResult { id, call_id, name, result } => json!({
            "role": "tool_result",
            "id": id,
            "call_id": call_id,
            "name": name,
            "result": result,
            "is_error": result.starts_with("ERROR:"),
        }),
    }
}

fn write_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let json_str = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json_str)
}

fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Format as ISO-ish timestamp suitable for directory names
    let (s, m, h) = (secs % 60, (secs / 60) % 60, (secs / 3600) % 24);
    let days = secs / 86400;
    // Simple date calculation from epoch days
    let (y, mo, d) = epoch_days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{m:02}-{s:02}")
}

fn epoch_days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil days from epoch algorithm
    let era_days = days + 719468;
    let era = era_days / 146097;
    let doe = era_days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
