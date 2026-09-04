//! Diagnostics logger for full LLM call logging.
//!
//! Creates a timestamped session directory under
//! `sessions/` and writes:
//! - `session_info.json` — persona, model, tools, skills, system prompt, cwd
//! - `turn_NNN.json` — the messages each agent step *added*, with `first_index`
//!   placing them in the history. Concatenating the files in order rebuilds the
//!   full message array; a file marked `rewritten` replaces what came before it
//!   rather than extending it (that is compaction).
//!
//! Every file is written under [`crate::resources::max_diagnostic_file_bytes`],
//! streamed rather than buffered, so a runaway context can't take the pod's
//! memory or disk with it.

use metalcraft::{AgentMessage, AgentState, LlmCallSnapshot};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Logger that writes full diagnostics to a session directory.
pub struct DiagnosticsLogger {
    session_dir: PathBuf,
    turn_counter: AtomicUsize,
    /// How many messages of the current context have already been written out,
    /// so [`DiagnosticsLogger::log_turn`] can record only what a step added.
    /// Reset to the new length whenever compaction makes the list shorter.
    logged_up_to: AtomicUsize,
}

/// What a session was started as.
///
/// `Default` gives the common shape (no flow, no agent), so a caller sets only what
/// it actually knows.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo<'a> {
    pub persona_name: &'a str,
    pub persona_slug: &'a str,
    pub model_name: &'a str,
    pub cwd: &'a str,
    pub system_prompt: &'a str,
    pub tools: &'a [String],
    pub skills: &'a [String],
    pub auto_approve: bool,
    /// Set for a flow run; also what makes `kind` read `"flow"`.
    pub flow_id: Option<&'a str>,
    /// The agent this session belongs to, so a background run's logs can be traced
    /// back to which agent produced them — the question Sessions could not answer.
    pub instance_id: Option<&'a str>,
}

impl DiagnosticsLogger {
    /// Create a new diagnostics logger. Creates the session directory immediately.
    ///
    /// The directory is named for the second the session started, so two sessions
    /// starting in the same second want the same name — and `create_dir_all`
    /// succeeded on an existing directory, so both got it. They then wrote
    /// `turn_001.json` over each other, and the operator reading that session
    /// afterwards saw one run's turns interleaved with another's with nothing to
    /// say so. Not hypothetical on a pod answering a messaging channel, where
    /// several conversations open on the same inbound burst.
    ///
    /// So the first session in a second takes the plain timestamp and any other
    /// takes a `-2`, `-3` suffix. `create_dir` rather than `create_dir_all` is
    /// what makes that a claim rather than a check: it fails if the directory
    /// already exists, so two threads racing here cannot both win.
    pub fn new() -> std::io::Result<Self> {
        let timestamp = chrono_timestamp();
        let sessions = crate::paths::sessions_dir();
        std::fs::create_dir_all(&sessions)?;

        let mut session_dir = sessions.join(&timestamp);
        for attempt in 2.. {
            match std::fs::create_dir(&session_dir) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    session_dir = sessions.join(format!("{timestamp}-{attempt}"));
                }
                Err(e) => return Err(e),
            }
        }

        Ok(Self {
            session_dir,
            turn_counter: AtomicUsize::new(0),
            logged_up_to: AtomicUsize::new(0),
        })
    }

    /// Returns the session directory path.
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Write session_info.json with startup configuration.
    ///
    /// Takes a struct rather than a positional list: this had grown to nine
    /// arguments, six of them `&str`, which is a swap waiting to happen — and the
    /// two optional ones (`flow_id`, `instance_id`) are exactly the pair a caller is
    /// most likely to get backwards.
    pub fn log_session_info(&self, info: SessionInfo<'_>) {
        let doc = json!({
            "timestamp": chrono_timestamp(),
            "persona_name": info.persona_name,
            "persona_slug": info.persona_slug,
            "model_name": info.model_name,
            "cwd": info.cwd,
            "system_prompt": info.system_prompt,
            "tools": info.tools,
            "skills": info.skills,
            "auto_approve": info.auto_approve,
            "kind": if info.flow_id.is_some() { "flow" } else { "session" },
            "flow_id": info.flow_id,
            "instance_id": info.instance_id,
        });
        let path = self.session_dir.join("session_info.json");
        if let Err(e) = write_json_capped(&path, &doc) {
            eprintln!("diagnostics: failed to write session_info.json: {e}");
        }
    }

    /// Log what a step *added* to the agent state.
    ///
    /// Called after every executor step, which is what makes the delta matter.
    /// Writing the whole message list each time made a session's diagnostics
    /// quadratic in its own length: step 500 of a long turn re-serialized the 499
    /// messages already on disk to record the one new one, so a thousand-step run
    /// wrote hundreds of megabytes to say a few hundred kilobytes' worth of
    /// things. The cost was paid twice over — once in the transient `Vec<Value>`
    /// and pretty-printed `String`, once on the pod's disk.
    ///
    /// So each file holds only the messages appended since the previous step,
    /// plus `first_index` to place them. A reader concatenates the files in
    /// order and has the full history back, exactly as before.
    ///
    /// The one case a delta can't describe is compaction, which *rewrites*
    /// `messages` rather than appending to it. That is detectable — the list got
    /// shorter — and handled by starting over: the file is marked `rewritten` and
    /// carries the whole new list, so a reader knows to discard what it had
    /// rather than append to it.
    pub fn log_turn(&self, state: &AgentState) {
        let turn = self.turn_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let total = state.messages.len();
        let previous = self.logged_up_to.swap(total, Ordering::Relaxed);
        let rewritten = previous > total;
        let first_index = if rewritten { 0 } else { previous };

        let messages: Vec<serde_json::Value> = state.messages[first_index.min(total)..]
            .iter()
            .map(serialize_message)
            .collect();

        let turn_data = json!({
            "turn": turn,
            "message_count": total,
            "first_index": first_index,
            "new_message_count": messages.len(),
            // Only ever true after a compaction rewrote the list; `messages` is
            // then the whole context rather than an addition to it.
            "rewritten": rewritten,
            "messages": messages,
        });

        let filename = format!("turn_{:03}.json", turn);
        let path = self.session_dir.join(&filename);
        if let Err(e) = write_json_capped(&path, &turn_data) {
            eprintln!("diagnostics: failed to write {filename}: {e}");
        }
    }

    /// Log the raw LLM request context (system prompt + messages + tools).
    ///
    /// This one is unavoidably the whole context — that is what a request *is* —
    /// so it is bounded rather than shrunk: [`write_json_capped`] serializes it
    /// straight into the file and abandons it at the ceiling, so neither the disk
    /// nor the heap sees the full size of a runaway context.
    pub fn log_llm_request(&self, snapshot: &LlmCallSnapshot) {
        let turn = self.turn_counter.load(Ordering::Relaxed) + 1;
        let filename = format!("llm_request_{:03}.json", turn);
        let path = self.session_dir.join(&filename);
        if let Err(e) = write_json_capped(&path, snapshot) {
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
        if let Err(e) = write_json_capped(&path, &data) {
            eprintln!("diagnostics: failed to write {filename}: {e}");
        }
    }

    /// Log a turn failure. Written to disk so the error is visible in the
    /// session timeline afterward — the SSE `done{status:"failed"}` event the
    /// client receives is ephemeral and was previously the *only* record of
    /// why a turn died.
    pub fn log_error(&self, message: &str) {
        let after_turn = self.turn_counter.load(Ordering::Relaxed);
        let data = json!({
            "event": "error",
            "after_turn": after_turn,
            "message": message,
        });
        let filename = format!("error_after_turn_{:03}.json", after_turn);
        let path = self.session_dir.join(&filename);
        if let Err(e) = write_json_capped(&path, &data) {
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
        if let Err(e) = write_json_capped(&path, &data) {
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
        AgentMessage::ToolCall {
            id,
            call_id,
            name,
            args,
        } => json!({
            "role": "tool_call",
            "id": id,
            "call_id": call_id,
            "name": name,
            "args": args,
        }),
        AgentMessage::ToolResult {
            id,
            call_id,
            name,
            result,
        } => json!({
            "role": "tool_result",
            "id": id,
            "call_id": call_id,
            "name": name,
            "result": result,
            "is_error": result.starts_with("ERROR:"),
        }),
        AgentMessage::Reasoning { id, .. } => json!({
            "role": "reasoning",
            "id": id,
            // The encrypted payload is large and opaque; record only its
            // presence, not its contents.
            "encrypted": true,
        }),
    }
}

/// Write a document to disk under a byte ceiling, streaming.
///
/// The ceiling is only half the point. The other half is *where* the bytes are
/// counted: this serializes straight into the file through a limiting writer, so
/// a payload that would blow the cap is abandoned at the cap rather than being
/// built in full as a `String` first and measured afterwards. Measuring
/// afterwards bounds the disk and leaves the memory spike exactly where it was,
/// which for an LLM request snapshot — the whole context, on every call — is the
/// spike that mattered.
///
/// A document that doesn't fit is replaced by a stub naming what was dropped, so
/// the session timeline keeps an entry at that step instead of a hole.
fn write_json_capped<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let limit = crate::resources::max_diagnostic_file_bytes();
    let tmp = path.with_extension("json.partial");

    let written = {
        let file = std::fs::File::create(&tmp)?;
        let mut writer = LimitWriter::new(std::io::BufWriter::new(file), limit);
        match serde_json::to_writer_pretty(&mut writer, value) {
            Ok(()) => {
                use std::io::Write;
                writer.flush()?;
                Some(writer.written())
            }
            // Either the cap tripped or the value wasn't serializable. Both end
            // the same way: no partial file, a stub in its place.
            Err(_) => None,
        }
    };

    match written {
        Some(bytes) => {
            std::fs::rename(&tmp, path)?;
            crate::resources::record_diagnostic_write(bytes as u64);
            Ok(())
        }
        None => {
            let _ = std::fs::remove_file(&tmp);
            let stub = json!({
                "truncated": true,
                "limit_bytes": limit,
                "note": "This diagnostics record exceeded MAX_DIAGNOSTIC_FILE_BYTES and was \
                         dropped rather than written. Raise that limit to capture it.",
            });
            let bytes = serde_json::to_vec_pretty(&stub).unwrap_or_default();
            crate::resources::record_diagnostic_truncated(bytes.len() as u64);
            std::fs::write(path, bytes)
        }
    }
}

/// A writer that gives up once it has passed `limit` bytes.
///
/// Returning an error is what makes the abandonment cheap: `serde_json` stops
/// serializing the moment a write fails, so the rest of a huge value is never
/// visited, let alone allocated.
struct LimitWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
}

impl<W: std::io::Write> LimitWriter<W> {
    fn new(inner: W, limit: usize) -> Self {
        Self {
            inner,
            written: 0,
            limit,
        }
    }

    fn written(&self) -> usize {
        self.written
    }
}

impl<W: std::io::Write> std::io::Write for LimitWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written + buf.len() > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "diagnostics record exceeded its byte limit",
            ));
        }
        let n = self.inner.write(buf)?;
        self.written += n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
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
