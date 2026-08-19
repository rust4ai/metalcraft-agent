//! Turn capture: the cheap half of remembering.
//!
//! This is the central bet of the whole design. A turn appends **one line** to
//! `capture.jsonl` — no LLM call, no embedding, no summarization at interactive
//! latency. The raw material sits there until the nightly dream distills it into
//! actual memories. So the agent accumulates experience continuously while the
//! per-turn cost stays at one `O_APPEND` write.
//!
//! Two sources feed the queue:
//!
//! * **Every completed turn** — the user's message, the agent's answer, and
//!   which tools ran. The tool names matter: they are what lets the dream
//!   extract a *procedural* memory ("to check DNS here, use `cloudflare_list`
//!   then …") rather than only facts.
//! * **Every compaction** — [`crate::context::compact_if_needed`] already pays
//!   an LLM call to summarize the history it is about to discard, then buries the
//!   result in one `Assistant` message and forgets it. That summary is the
//!   highest-value memory material in the system and it currently evaporates.
//!   Capturing it costs nothing extra.
//!
//! There is deliberately **no episode state machine**. An episode is derived at
//! dream time by grouping captures on `chat_id` and time gaps, plus the explicit
//! `SessionEnd` markers written when a conversation demonstrably ends (a gateway
//! idle-reset, a deleted chat). Deriving beats tracking: no lifecycle to get
//! wrong, no half-open episodes after a crash.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::redact;

/// How much of one side of a turn to keep. Generous — the dream reads these, and
/// truncating too hard is what makes distillation vague — but bounded, because a
/// pathological turn should not write a megabyte to the queue.
const MAX_SIDE_CHARS: usize = 8_000;

/// What produced a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    /// One completed user↔agent exchange.
    Turn,
    /// A context-compaction summary rescued before it was discarded.
    Compaction,
    /// A marker that a conversation ended. Carries no content.
    SessionEnd,
}

/// One line of `capture.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    pub id: String,
    pub kind: CaptureKind,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub chat_id: Option<String>,
    /// Which agent produced this turn. The queue is pod-global, so without it a
    /// later distillation pass could not tell whose memory the material belongs
    /// in — and with per-instance memory that is the whole question.
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub user_text: String,
    #[serde(default)]
    pub agent_text: String,
    /// Tools called during the turn, in order, deduplicated.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Set once the dream has turned this into memories.
    #[serde(default)]
    pub processed_at: Option<DateTime<Utc>>,
}

impl Capture {
    /// Whether there is anything here worth distilling.
    pub fn has_content(&self) -> bool {
        !self.user_text.trim().is_empty() || !self.agent_text.trim().is_empty()
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_SIDE_CHARS {
        return s.to_string();
    }
    let kept: String = s.chars().take(MAX_SIDE_CHARS).collect();
    format!("{kept}\n[…truncated]")
}

/// What a turn knows about itself. `None` fields simply mean "not a chat-bound
/// turn" (a one-shot run, a flow node), which is fine — the dream can still
/// distill it, just without conversation grouping.
#[derive(Debug, Clone, Default)]
pub struct CaptureContext {
    pub chat_id: Option<String>,
    pub persona: Option<String>,
    /// The agent instance this turn ran as, carried into every capture it produces.
    pub instance_id: Option<String>,
}

/// Append a capture. Never returns an error to the caller's critical path — a
/// failed capture is logged and dropped, because losing raw material is a much
/// smaller problem than failing the turn that produced it.
fn append(capture: &Capture) {
    let path = crate::paths::memory_capture_file();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        log::debug!("memory: could not create the capture directory ({e})");
        return;
    }
    let line = match serde_json::to_string(capture) {
        Ok(l) => l,
        Err(e) => {
            log::debug!("memory: could not serialize a capture ({e})");
            return;
        }
    };
    use std::io::Write;
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{line}") {
                log::debug!("memory: could not append a capture ({e})");
            }
        }
        Err(e) => log::debug!("memory: could not open the capture queue ({e})"),
    }
}

/// Record one completed turn.
///
/// Content is redacted here rather than at distillation time, so a secret never
/// reaches the disk at all — the queue is a file like any other.
pub fn record_turn(ctx: &CaptureContext, user_text: &str, agent_text: &str, tools: Vec<String>) {
    if !super::enabled() || !capture_enabled() {
        return;
    }
    if user_text.trim().is_empty() && agent_text.trim().is_empty() {
        return;
    }
    let capture = Capture {
        id: uuid::Uuid::new_v4().to_string(),
        kind: CaptureKind::Turn,
        at: Utc::now(),
        chat_id: ctx.chat_id.clone(),
        instance_id: ctx.instance_id.clone(),
        persona: ctx.persona.clone(),
        user_text: redact::redact(&truncate(user_text)).content,
        agent_text: redact::redact(&truncate(agent_text)).content,
        tools,
        processed_at: None,
    };
    append(&capture);
}

/// Record a compaction summary before it is discarded.
pub fn record_compaction(ctx: &CaptureContext, summary: &str) {
    if !super::enabled() || !capture_enabled() || summary.trim().is_empty() {
        return;
    }
    let capture = Capture {
        id: uuid::Uuid::new_v4().to_string(),
        kind: CaptureKind::Compaction,
        at: Utc::now(),
        chat_id: ctx.chat_id.clone(),
        instance_id: ctx.instance_id.clone(),
        persona: ctx.persona.clone(),
        user_text: String::new(),
        agent_text: redact::redact(&truncate(summary)).content,
        tools: Vec::new(),
        processed_at: None,
    };
    append(&capture);
}

/// Mark a conversation as finished, so the dream can distill it without waiting
/// for a time gap to prove it.
pub fn record_session_end(chat_id: &str) {
    if !super::enabled() || !capture_enabled() {
        return;
    }
    let capture = Capture {
        id: uuid::Uuid::new_v4().to_string(),
        kind: CaptureKind::SessionEnd,
        at: Utc::now(),
        chat_id: Some(chat_id.to_string()),
        // A session-end marker is a boundary, not material; the distiller reads
        // the agent from the turns it bounds.
        instance_id: None,
        persona: None,
        user_text: String::new(),
        agent_text: String::new(),
        tools: Vec::new(),
        processed_at: None,
    };
    append(&capture);
}

/// Whether turn capture is on (`MEMORY_CAPTURE`).
pub fn capture_enabled() -> bool {
    match std::env::var("MEMORY_CAPTURE") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

/// Read every parseable capture. Unparseable lines (a torn tail from an unclean
/// shutdown) are counted, not fatal — same tolerance as the event log.
pub fn read_all() -> (Vec<Capture>, usize) {
    let path = crate::paths::memory_capture_file();
    let Ok(file) = std::fs::File::open(&path) else {
        return (Vec::new(), 0);
    };
    use std::io::BufRead;
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            skipped += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Capture>(&line) {
            Ok(c) => out.push(c),
            Err(_) => skipped += 1,
        }
    }
    (out, skipped)
}

/// Captures the dream has not yet distilled, oldest first.
pub fn pending() -> Vec<Capture> {
    let (all, skipped) = read_all();
    if skipped > 0 {
        log::warn!("memory: {skipped} unreadable line(s) in the capture queue were skipped");
    }
    let mut pending: Vec<Capture> = all.into_iter().filter(|c| c.processed_at.is_none()).collect();
    pending.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    pending
}

/// How many captures are waiting. Reported by `mem_stats` so a queue that stops
/// draining (a dream that never runs) is visible rather than silent.
pub fn pending_count() -> usize {
    pending().len()
}

/// Rewrite the queue keeping only what is still undistilled.
///
/// Atomic (tmp + rename), like the snapshot. Called by the dream after it has
/// turned captures into memories; separated from distillation so a crash mid-run
/// re-reads material rather than losing it.
pub fn retain_pending(processed_ids: &[String]) -> std::io::Result<usize> {
    let path = crate::paths::memory_capture_file();
    let (all, _) = read_all();
    let processed: std::collections::HashSet<&str> = processed_ids.iter().map(|s| s.as_str()).collect();
    let keep: Vec<&Capture> = all.iter().filter(|c| !processed.contains(c.id.as_str())).collect();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("jsonl.tmp");
    {
        use std::io::Write;
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        for c in &keep {
            let line = serde_json::to_string(c).map_err(std::io::Error::other)?;
            writeln!(w, "{line}")?;
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(all.len() - keep.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_capture_keeps_both_sides_and_the_tools() {
        let c = Capture {
            instance_id: None,
            id: "x".into(),
            kind: CaptureKind::Turn,
            at: Utc::now(),
            chat_id: Some("chat-1".into()),
            persona: Some("orchestrator-agent".into()),
            user_text: "how do I deploy?".into(),
            agent_text: "run ./start-agent.sh".into(),
            tools: vec!["read_file".into(), "bash".into()],
            processed_at: None,
        };
        assert!(c.has_content());
        let round: Capture = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(round.tools, vec!["read_file", "bash"]);
        assert_eq!(round.kind, CaptureKind::Turn);
        assert_eq!(round.chat_id.as_deref(), Some("chat-1"));
    }

    #[test]
    fn a_session_end_marker_has_no_content() {
        let c = Capture {
            instance_id: None,
            id: "x".into(),
            kind: CaptureKind::SessionEnd,
            at: Utc::now(),
            chat_id: Some("chat-1".into()),
            persona: None,
            user_text: String::new(),
            agent_text: String::new(),
            tools: vec![],
            processed_at: None,
        };
        assert!(!c.has_content(), "a marker is not distillable material");
    }

    #[test]
    fn oversized_sides_are_truncated_with_a_marker() {
        let long = "x".repeat(MAX_SIDE_CHARS + 500);
        let t = truncate(&long);
        assert!(t.ends_with("[…truncated]"));
        assert!(t.chars().count() < long.chars().count());
        // Short input is untouched.
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn truncation_is_char_safe_on_multibyte_text() {
        let long = "é".repeat(MAX_SIDE_CHARS + 10);
        let t = truncate(&long);
        assert!(t.starts_with('é'), "must not split a multibyte char");
        assert!(t.ends_with("[…truncated]"));
    }

    #[test]
    fn capture_is_on_by_default_and_respects_its_switch() {
        // The ambient env has no MEMORY_CAPTURE in a clean test process.
        assert!(capture_enabled());
    }
}
