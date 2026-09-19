//! A directory of the sub-agents this process has delegated to.
//!
//! Delegation used to be one-shot: [`crate::tools::sub_agent`] built a child,
//! ran it, read its answer and dropped everything else on the floor. The child
//! that spent forty seconds reading a codebase was the only thing in the system
//! that knew that codebase, and the follow-up question — "now fix the two files
//! you found" — re-paid for every one of those reads because the second child
//! started blank.
//!
//! This is where a finished child stays addressable. The parent gets an id
//! immediately, can list what it has delegated to, can read a delegate's full
//! transcript, and can send a follow-up that resumes the *same* accumulated
//! history instead of starting over.
//!
//! # States
//!
//! Exactly four, ported from pi's agent registry:
//!
//! - [`DelegateState::Running`] — a run is in flight. Not addressable for a
//!   follow-up: two turns writing one history is the same corruption two agents
//!   writing one workspace is.
//! - [`DelegateState::Idle`] — the run finished. The verbatim message history is
//!   held, so a follow-up replays the child exactly as it was.
//! - [`DelegateState::Parked`] — idle past [`DELEGATE_IDLE_TTL_SECS`]. The
//!   verbatim history (which carries every tool result the child ever saw, and
//!   is by far the expensive part) is released; the rendered transcript and the
//!   spawn arguments are kept, so a follow-up still revives — re-seeded from the
//!   transcript rather than replayed. Cheaper, lossier, and the tool result says
//!   which one happened rather than letting the parent assume fidelity it did
//!   not get.
//! - [`DelegateState::Aborted`] — timed out, stopped by the user, or cancelled.
//!   Terminal. A hard timeout leaves nothing behind to resume from (the run
//!   future is dropped mid-flight, so there is no history to replay), and a stop
//!   is a decision, not an accident. Offering a revival that cannot work — or
//!   that quietly undoes a stop — is worse than refusing.
//!
//! # Durability
//!
//! **This registry is process-local.** It is a `Mutex<Store>` in a `OnceLock`,
//! nothing is written to the data dir, and nothing is reloaded at startup. What
//! a restart loses: every delegate id, every transcript, every message history,
//! and therefore every revival. After a restart the parent's *persisted chat*
//! still contains the tool results it already read (those live in the chat file
//! like any other tool result), but `sub_agent_list` comes back empty and
//! `sub_agent_send` reports the id as unknown. That is deliberate for this pass
//! — a half-durable registry that resurrects ids whose histories are gone would
//! promise a revival it cannot perform, which is the exact failure the `Aborted`
//! state exists to prevent.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use metalcraft::AgentMessage;

/// How much of a delegate's output rides inline in the parent's tool result.
///
/// Two levels, because the single-level answers are both wrong: naive
/// truncation throws the data away, and no truncation throws the *parent's
/// context* away — a 300 KB transcript inlined into a tool result is replayed
/// into every LLM request for the rest of the conversation (see
/// [`crate::tools::capped`]). So the parent gets a bounded preview plus the id,
/// and the full text stays here, reachable with `sub_agent_read`.
pub const DELEGATE_PREVIEW_BYTES: usize = 4_000;

/// Line ceiling for the same preview. Bytes alone let a thousand one-word lines
/// through, which is the shape a delegate's file listing or test output takes.
pub const DELEGATE_PREVIEW_LINES: usize = 40;

/// The second level: how much of a delegate's transcript this registry retains.
///
/// Above this the head and tail are kept and the middle is dropped, once, at
/// settle time — so the retained text never grows without bound even if a
/// delegate loops printing.
pub const DELEGATE_TRANSCRIPT_BYTES: usize = 500_000;

/// How much transcript one `sub_agent_read` call returns. The caller pages with
/// `offset`; this keeps a single read from re-creating the unbounded inline
/// result the preview exists to avoid.
pub const DELEGATE_READ_CHUNK_BYTES: usize = 24_000;

/// How long an idle delegate keeps its verbatim message history before it is
/// parked.
///
/// Fifteen minutes is about the span of one working exchange: long enough that
/// "ask the delegate a follow-up" is still a full-fidelity resume, short enough
/// that a long session does not accumulate a dozen complete tool-result
/// histories in memory.
pub const DELEGATE_IDLE_TTL_SECS: u64 = 900;

/// How many delegates this process tracks at once, across all scopes.
///
/// Past this the oldest settled record is dropped entirely — id, transcript and
/// all. Unlike parking this is real loss, so the bound is generous: a session
/// that delegated sixty-four times has long since stopped caring about the
/// first one.
pub const MAX_TRACKED_DELEGATES: usize = 64;

/// Where a delegate is in its life. See the module docs for why there are
/// exactly these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegateState {
    Running,
    Idle,
    Parked,
    Aborted,
}

impl DelegateState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Parked => "parked",
            Self::Aborted => "aborted",
        }
    }

    /// Can a follow-up be sent to a delegate in this state?
    ///
    /// `Running` is excluded for the same reason `Aborted` is, from the other
    /// direction: there is no history to hand a second turn, because the first
    /// one has not finished writing it.
    pub fn is_revivable(self) -> bool {
        matches!(self, Self::Idle | Self::Parked)
    }
}

/// Everything one settled run adds to a delegate's record.
pub struct Outcome {
    /// What the delegate said it finished, in its own words.
    pub summary: String,
    /// Concrete items it reported as outstanding.
    pub not_done: Vec<String>,
    /// The delegate never met the termination contract and this summary was
    /// reconciled out of its trailing prose. See
    /// [`crate::tools::yield_result`].
    pub unreconciled: bool,
    pub tools_used: Vec<String>,
    pub turns: usize,
    /// Human-readable rendering of the run, for `sub_agent_read`.
    pub transcript: String,
    /// The verbatim history, for a full-fidelity follow-up.
    pub messages: Vec<AgentMessage>,
}

/// What a caller needs to run a delegate again.
#[derive(Debug)]
pub enum Revival {
    /// The verbatim history survived: replay it and append the follow-up, so
    /// the child continues the conversation it was already having.
    Resume {
        spawn_args: serde_json::Value,
        messages: Vec<AgentMessage>,
    },
    /// The delegate was parked and its history released. A fresh child is
    /// seeded with the transcript as prior context — it knows what it found,
    /// not how it phrased every step of finding it.
    Reseed {
        spawn_args: serde_json::Value,
        transcript: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviveError {
    /// No delegate by that id in this scope — or the process restarted (see the
    /// module docs on durability).
    Unknown,
    /// Still running. Its history is being written right now.
    Running,
    /// Terminal. Refused rather than offered a revival that cannot work.
    Aborted { reason: String },
}

impl std::fmt::Display for ReviveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(
                f,
                "no delegate with that id. It may have been evicted, or this process restarted — \
                 delegate history is not persisted. Call `sub_agent_list` for what is addressable, \
                 or `sub_agent` to start a fresh one."
            ),
            Self::Running => write!(
                f,
                "that delegate is still running; its history is being written. Wait for the \
                 delegation that started it to return."
            ),
            Self::Aborted { reason } => write!(
                f,
                "that delegate was aborted ({reason}) and cannot be revived — there is no usable \
                 history behind it. Start a fresh delegation instead."
            ),
        }
    }
}

/// One row of `sub_agent_list`.
pub struct DelegateSummary {
    pub id: String,
    pub label: String,
    pub state: DelegateState,
    pub task: String,
    pub age_secs: u64,
    pub turns: usize,
    pub not_done: Vec<String>,
    pub unreconciled: bool,
    pub note: Option<String>,
    pub preview: String,
    pub transcript_bytes: usize,
}

/// One page of `sub_agent_read`.
pub struct TranscriptChunk {
    pub state: DelegateState,
    pub total_bytes: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub text: String,
}

struct DelegateRecord {
    label: String,
    task: String,
    state: DelegateState,
    spawn_args: serde_json::Value,
    summary: String,
    not_done: Vec<String>,
    unreconciled: bool,
    note: Option<String>,
    tools_used: Vec<String>,
    turns: usize,
    transcript: String,
    /// Released on park; see the module docs.
    messages: Vec<AgentMessage>,
    settled_at: Option<Instant>,
    /// Insertion order, so eviction can drop the oldest without comparing
    /// `Instant`s that a `Running` record does not have yet.
    seq: u64,
}

#[derive(Default)]
struct Store {
    records: BTreeMap<(String, String), DelegateRecord>,
    next_seq: u64,
}

fn store() -> MutexGuard<'static, Store> {
    static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::default()));
    // A panic inside one delegation must not turn the whole registry into a
    // permanent error for every later one; the data behind the lock is plain
    // records with no invariant a half-finished write could break.
    STORE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reserve an id and open a `Running` record for it.
///
/// Called **before** the child starts, so the tool result can name the id even
/// when the run then times out — an id the parent only learns on success is
/// useless exactly when it is needed.
pub fn open(scope: &str, name: &str, task: &str, spawn_args: serde_json::Value) -> String {
    let mut store = store();
    sweep(&mut store);
    let taken: BTreeSet<String> = store
        .records
        .keys()
        .filter(|(s, _)| s == scope)
        .map(|(_, id)| id.clone())
        .collect();
    let id = next_free_id(name, &taken);
    let seq = store.next_seq;
    store.next_seq += 1;
    store.records.insert(
        (scope.to_string(), id.clone()),
        DelegateRecord {
            label: name.to_string(),
            task: task.to_string(),
            state: DelegateState::Running,
            spawn_args,
            summary: String::new(),
            not_done: Vec::new(),
            unreconciled: false,
            note: None,
            tools_used: Vec::new(),
            turns: 0,
            transcript: String::new(),
            messages: Vec::new(),
            settled_at: None,
            seq,
        },
    );
    evict_overflow(&mut store);
    id
}

/// Record a finished run against an open delegate, leaving it `Idle`.
///
/// A follow-up appends, so the transcript accumulates across revivals rather
/// than each run overwriting what the last one learned.
pub fn settle(scope: &str, id: &str, outcome: Outcome) {
    let mut store = store();
    let Some(record) = store.records.get_mut(&(scope.to_string(), id.to_string())) else {
        return;
    };
    record.state = DelegateState::Idle;
    record.summary = outcome.summary;
    record.not_done = outcome.not_done;
    record.unreconciled = outcome.unreconciled;
    record.turns += outcome.turns;
    record.tools_used = outcome.tools_used;
    record.messages = outcome.messages;
    record.settled_at = Some(Instant::now());
    if !record.transcript.is_empty() {
        record.transcript.push_str("\n\n");
    }
    record.transcript.push_str(&outcome.transcript);
    record.transcript = elide_middle(&record.transcript, DELEGATE_TRANSCRIPT_BYTES);
    record.note = outcome
        .unreconciled
        .then(|| "ended without calling yield_result; summary reconciled from its prose".into());
}

/// Mark a delegate terminal. Not revivable afterwards, by construction.
pub fn abort(scope: &str, id: &str, reason: &str) {
    let mut store = store();
    let Some(record) = store.records.get_mut(&(scope.to_string(), id.to_string())) else {
        return;
    };
    record.state = DelegateState::Aborted;
    record.note = Some(reason.to_string());
    record.settled_at = Some(Instant::now());
    // The history is what a revival would replay, and after an abort it is
    // either absent (a dropped timeout future) or mid-sentence. Dropping it
    // here is what makes "aborted is not revivable" true in storage rather than
    // only in a branch somebody could later delete.
    record.messages.clear();
}

/// Retire a delegate that was cancelled cooperatively rather than killed.
///
/// The distinction is the whole reason both states exist. A stop the operator
/// pressed, or a timeout that dropped the run future, leaves either a decision
/// that must not be silently undone or nothing to resume from — that is
/// [`abort`]. A cooperative cancellation hands back intact state the caller
/// still intends to come back to, so it lands `Parked`: the verbatim history
/// goes (it ends mid-turn, and replaying a half-written turn is how an orphaned
/// tool call reaches the provider) but the transcript and spawn arguments stay,
/// and a follow-up re-seeds from them.
pub fn park(scope: &str, id: &str, reason: &str, transcript: &str) {
    let mut store = store();
    let Some(record) = store.records.get_mut(&(scope.to_string(), id.to_string())) else {
        return;
    };
    record.state = DelegateState::Parked;
    record.note = Some(reason.to_string());
    record.settled_at = Some(Instant::now());
    record.messages.clear();
    if !transcript.trim().is_empty() {
        if !record.transcript.is_empty() {
            record.transcript.push_str("\n\n");
        }
        record.transcript.push_str(transcript);
        record.transcript = elide_middle(&record.transcript, DELEGATE_TRANSCRIPT_BYTES);
    }
}

/// Append a follow-up prompt to a delegate's transcript, so reading it later
/// shows the exchange rather than two answers with no questions.
pub fn note_followup(scope: &str, id: &str, message: &str) {
    let mut store = store();
    if let Some(record) = store.records.get_mut(&(scope.to_string(), id.to_string())) {
        if !record.transcript.is_empty() {
            record.transcript.push_str("\n\n");
        }
        record.transcript.push_str("follow-up: ");
        record.transcript.push_str(message);
    }
}

/// Take a delegate out for another run, flipping it to `Running`.
///
/// Combined rather than a `state()` check followed by a separate transition:
/// two parallel follow-ups to one delegate would both pass a separate check and
/// then both write its history.
pub fn check_out(scope: &str, id: &str) -> Result<Revival, ReviveError> {
    let mut store = store();
    sweep(&mut store);
    let Some(record) = store.records.get_mut(&(scope.to_string(), id.to_string())) else {
        return Err(ReviveError::Unknown);
    };
    match record.state {
        DelegateState::Running => return Err(ReviveError::Running),
        DelegateState::Aborted => {
            return Err(ReviveError::Aborted {
                reason: record.note.clone().unwrap_or_else(|| "terminated".into()),
            });
        }
        DelegateState::Idle | DelegateState::Parked => {}
    }
    let parked = record.state == DelegateState::Parked;
    record.state = DelegateState::Running;
    let spawn_args = record.spawn_args.clone();
    if parked {
        Ok(Revival::Reseed {
            spawn_args,
            transcript: record.transcript.clone(),
        })
    } else {
        Ok(Revival::Resume {
            spawn_args,
            messages: std::mem::take(&mut record.messages),
        })
    }
}

/// Every delegate in one scope, newest first.
pub fn list(scope: &str) -> Vec<DelegateSummary> {
    let mut store = store();
    sweep(&mut store);
    let now = Instant::now();
    let mut rows: Vec<(u64, DelegateSummary)> = store
        .records
        .iter()
        .filter(|((s, _), _)| s == scope)
        .map(|((_, id), r)| {
            (
                r.seq,
                DelegateSummary {
                    id: id.clone(),
                    label: r.label.clone(),
                    state: r.state,
                    task: r.task.clone(),
                    age_secs: r
                        .settled_at
                        .map(|t| now.duration_since(t).as_secs())
                        .unwrap_or(0),
                    turns: r.turns,
                    not_done: r.not_done.clone(),
                    unreconciled: r.unreconciled,
                    note: r.note.clone(),
                    preview: preview_of(if r.summary.is_empty() {
                        &r.transcript
                    } else {
                        &r.summary
                    })
                    .text,
                    transcript_bytes: r.transcript.len(),
                },
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, s)| s).collect()
}

/// One page of a delegate's retained transcript.
pub fn read(scope: &str, id: &str, offset: usize) -> Option<TranscriptChunk> {
    let store = store();
    let record = store.records.get(&(scope.to_string(), id.to_string()))?;
    let total = record.transcript.len();
    let start = floor_boundary(&record.transcript, offset.min(total));
    let end = floor_boundary(
        &record.transcript,
        start.saturating_add(DELEGATE_READ_CHUNK_BYTES).min(total),
    );
    Some(TranscriptChunk {
        state: record.state,
        total_bytes: total,
        offset: start,
        next_offset: (end < total).then_some(end),
        text: record.transcript[start..end].to_string(),
    })
}

/// Serialise tests that share the process-global store.
///
/// Not a harness nicety: the directory is one per process — that is the whole
/// design — and the eviction bound counts across scopes, so two tests resetting
/// and filling it at once really do corrupt each other's view. Every test that
/// touches the store takes this first and holds it for its duration.
#[cfg(test)]
pub fn exclusive() -> MutexGuard<'static, ()> {
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drop everything. Tests share one process, and a registry that carried ids
/// between them would make collision-suffixing depend on test order. Call it
/// while holding [`exclusive`].
#[cfg(test)]
pub fn reset() {
    let mut store = store();
    store.records.clear();
    store.next_seq = 0;
}

/// Park idle delegates whose TTL has run out.
fn sweep(store: &mut Store) {
    let now = Instant::now();
    for record in store.records.values_mut() {
        if record.state != DelegateState::Idle {
            continue;
        }
        let stale = record
            .settled_at
            .is_some_and(|t| now.duration_since(t).as_secs() >= DELEGATE_IDLE_TTL_SECS);
        if stale {
            record.state = DelegateState::Parked;
            record.messages = Vec::new();
        }
    }
}

/// Keep the directory bounded by dropping the oldest settled record. `Running`
/// records are never evicted: something is still writing to them.
fn evict_overflow(store: &mut Store) {
    while store.records.len() > MAX_TRACKED_DELEGATES {
        let victim = store
            .records
            .iter()
            .filter(|(_, r)| r.state != DelegateState::Running)
            .min_by_key(|(_, r)| r.seq)
            .map(|(k, _)| k.clone());
        match victim {
            Some(key) => {
                store.records.remove(&key);
            }
            // Everything tracked is in flight. Overshooting the cap beats
            // evicting a record a running delegation is about to settle.
            None => break,
        }
    }
}

/// A readable, stable id for a child, unique inside its scope.
///
/// The name is what the model asked for (a persona slug, or the ad-hoc tool
/// set), so delegating to `research-agent` twice gives `research-agent` and
/// `research-agent-2` rather than two opaque uuids the model cannot tell apart.
/// Suffixing starts at `-2` because the first one is already the unsuffixed
/// name — `-1` would imply a sibling that does not exist.
fn next_free_id(name: &str, taken: &BTreeSet<String>) -> String {
    let base = slugify(name);
    if !taken.contains(&base) {
        return base;
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 exhausted allocating a delegate id")
}

fn slugify(name: &str) -> String {
    const MAX: usize = 40;
    let mut out = String::with_capacity(name.len().min(MAX));
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(ch.to_lowercase());
            if out.len() >= MAX {
                break;
            }
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "delegate".to_string()
    } else {
        out
    }
}

/// A bounded view of a body, plus enough metadata to tell that it *is* bounded.
///
/// A truncation the consumer cannot detect is a correctness bug: the model
/// reads the tail of a preview as the end of the data and concludes the
/// delegate found nothing more.
pub struct Preview {
    pub text: String,
    pub full_bytes: usize,
    pub truncated: bool,
}

pub fn preview_of(text: &str) -> Preview {
    let full_bytes = text.len();
    let by_lines = cap_lines(text, DELEGATE_PREVIEW_LINES);
    let capped = elide_middle(&by_lines, DELEGATE_PREVIEW_BYTES);
    Preview {
        truncated: capped.len() < full_bytes,
        text: capped,
        full_bytes,
    }
}

/// Keep the head and tail lines, drop the middle. The head says what the
/// delegate set out to do and the tail says how it ended; the middle is the
/// part `sub_agent_read` exists for.
fn cap_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    let head = max * 2 / 3;
    let tail = max - head;
    format!(
        "{}\n… {} lines omitted …\n{}",
        lines[..head].join("\n"),
        lines.len() - head - tail,
        lines[lines.len() - tail..].join("\n")
    )
}

fn elide_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let keep = max_bytes / 2;
    let head_end = floor_boundary(text, keep);
    let tail_start = ceil_boundary(text, text.len() - keep);
    format!(
        "{}\n… {} bytes omitted …\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

fn floor_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Render a child's message history as something a person (or the parent model)
/// can read.
///
/// User messages are skipped deliberately: the task and every follow-up are
/// recorded by the caller (`open`, `note_followup`), and the variant's payload
/// type is the one part of `AgentMessage` that has changed shape between
/// metalcraft releases. Reading it here would couple the transcript to that.
pub fn render_transcript(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            AgentMessage::Assistant(text) if !text.trim().is_empty() => {
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            AgentMessage::ToolCall { name, args, .. } => {
                out.push_str(&format!("→ {name}({})\n", compact_args(args)));
            }
            AgentMessage::ToolResult { name, result, .. } => {
                out.push_str(&format!(
                    "← {name}: {}\n",
                    elide_middle(result.trim(), 1_000)
                ));
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

fn compact_args(args: &serde_json::Value) -> String {
    let rendered = args.to_string();
    elide_middle(&rendered, 300)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_delegate_with_the_same_name_gets_a_suffix() {
        // Ids are the handle the model uses to follow up, so two delegations to
        // one persona must not collide — the second follow-up would otherwise
        // land on the first delegate's history.
        let mut taken = BTreeSet::new();
        assert_eq!(next_free_id("research-agent", &taken), "research-agent");
        taken.insert("research-agent".to_string());
        assert_eq!(next_free_id("research-agent", &taken), "research-agent-2");
        taken.insert("research-agent-2".to_string());
        assert_eq!(next_free_id("research-agent", &taken), "research-agent-3");
        // A name that slugifies onto an existing id collides just the same.
        assert_eq!(next_free_id("Research Agent", &taken), "research-agent-3");
    }

    #[test]
    fn a_name_that_is_not_a_slug_still_becomes_one() {
        assert_eq!(slugify("tool_set:read_only"), "tool-set-read-only");
        assert_eq!(slugify("  "), "delegate");
        assert_eq!(slugify("GitHub Agent"), "github-agent");
        assert!(slugify(&"x".repeat(200)).len() <= 40);
    }

    #[test]
    fn ids_are_allocated_per_scope() {
        let _exclusive = exclusive();
        reset();
        let a = open("agent-a", "research-agent", "t", serde_json::json!({}));
        let b = open("agent-b", "research-agent", "t", serde_json::json!({}));
        assert_eq!(a, "research-agent");
        assert_eq!(
            b, "research-agent",
            "a second agent instance is a different scope, not a collision"
        );
        let c = open("agent-a", "research-agent", "t", serde_json::json!({}));
        assert_eq!(c, "research-agent-2");
        assert_eq!(list("agent-a").len(), 2);
        assert_eq!(list("agent-b").len(), 1);
    }

    fn settled(text: &str) -> Outcome {
        Outcome {
            summary: "done".into(),
            not_done: vec![],
            unreconciled: false,
            tools_used: vec!["read_file".into()],
            turns: 3,
            transcript: text.to_string(),
            messages: vec![AgentMessage::Assistant("done".into())],
        }
    }

    #[test]
    fn a_running_delegate_settles_idle_and_revives_at_full_fidelity() {
        let _exclusive = exclusive();
        reset();
        let id = open("s", "research-agent", "look", serde_json::json!({"task":"look"}));
        assert_eq!(list("s")[0].state, DelegateState::Running);
        assert_eq!(check_out("s", &id).unwrap_err(), ReviveError::Running);

        settle("s", &id, settled("→ read_file\n← ok"));
        assert_eq!(list("s")[0].state, DelegateState::Idle);
        match check_out("s", &id).expect("idle revives") {
            Revival::Resume { messages, .. } => assert_eq!(messages.len(), 1),
            Revival::Reseed { .. } => panic!("an idle delegate still has its history"),
        }
        // check_out flips it back to Running so a second follow-up cannot race.
        assert_eq!(check_out("s", &id).unwrap_err(), ReviveError::Running);
    }

    #[test]
    fn an_aborted_delegate_refuses_revival() {
        // The point of the state: a hard timeout drops the run future, so there
        // is no history to resume. Offering a revival that cannot work is worse
        // than saying so.
        let _exclusive = exclusive();
        reset();
        let id = open("s", "slow-agent", "wait", serde_json::json!({}));
        settle("s", &id, settled("partial"));
        abort("s", &id, "timed out after 120 seconds");
        assert_eq!(list("s")[0].state, DelegateState::Aborted);
        match check_out("s", &id) {
            Err(ReviveError::Aborted { reason }) => assert!(reason.contains("timed out")),
            _ => panic!("aborted must refuse, got a revival"),
        }
        assert!(
            format!("{}", ReviveError::Unknown).contains("not persisted"),
            "the model has to learn that a restart is why the id is gone"
        );
    }

    #[test]
    fn a_cooperative_cancel_parks_where_a_kill_aborts() {
        // Main's rule for RunOutcome::Cancelled: state is intact and the caller
        // meant to come back, so it must not be filed as the terminal state a
        // stop button produces.
        let _exclusive = exclusive();
        reset();
        let killed = open("s", "killed-agent", "t", serde_json::json!({}));
        abort("s", &killed, "stopped by the user");
        assert!(matches!(
            check_out("s", &killed),
            Err(ReviveError::Aborted { .. })
        ));

        let cancelled = open("s", "cancelled-agent", "t", serde_json::json!({}));
        park("s", &cancelled, "cancelled", "what it had reached");
        match check_out("s", &cancelled).expect("a cooperative cancel stays revivable") {
            Revival::Reseed { transcript, .. } => assert!(transcript.contains("what it had")),
            Revival::Resume { .. } => panic!("a half-written turn must not be replayed verbatim"),
        }
    }

    #[test]
    fn parking_releases_the_history_but_keeps_the_transcript() {
        let _exclusive = exclusive();
        reset();
        let id = open("s", "research-agent", "look", serde_json::json!({}));
        settle("s", &id, settled("the findings"));
        {
            let mut store = store();
            // Age it past the TTL without sleeping for fifteen minutes.
            let record = store.records.get_mut(&("s".to_string(), id.clone())).unwrap();
            record.settled_at =
                Some(Instant::now() - std::time::Duration::from_secs(DELEGATE_IDLE_TTL_SECS + 1));
        }
        assert_eq!(list("s")[0].state, DelegateState::Parked);
        match check_out("s", &id).expect("parked still revives") {
            Revival::Reseed { transcript, .. } => assert!(transcript.contains("the findings")),
            Revival::Resume { .. } => panic!("a parked delegate has no verbatim history left"),
        }
    }

    #[test]
    fn the_parent_gets_a_preview_and_the_registry_keeps_the_rest() {
        // The two-level cap: what rides in the parent's context is bounded, and
        // the thing it is a preview *of* is still there to be read.
        let _exclusive = exclusive();
        reset();
        let big = (0..4_000)
            .map(|i| format!("line {i} of the delegate's output"))
            .collect::<Vec<_>>()
            .join("\n");
        let id = open("s", "research-agent", "survey", serde_json::json!({}));
        settle(
            "s",
            &id,
            Outcome {
                transcript: big.clone(),
                ..settled("")
            },
        );

        let preview = preview_of(&big);
        assert!(preview.truncated);
        assert!(
            preview.text.len() < DELEGATE_PREVIEW_BYTES * 2,
            "a preview that is not bounded is not a preview: {}",
            preview.text.len()
        );
        assert!(
            preview.text.contains("omitted"),
            "the model must be able to tell it was truncated"
        );
        assert_eq!(preview.full_bytes, big.len());

        let row = &list("s")[0];
        assert!(row.transcript_bytes > DELEGATE_PREVIEW_BYTES);

        // …and the full text pages back out, in order, with no gaps.
        let mut offset = 0;
        let mut rebuilt = String::new();
        loop {
            let chunk = read("s", &id, offset).expect("a settled delegate is readable");
            rebuilt.push_str(&chunk.text);
            match chunk.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(rebuilt.len(), row.transcript_bytes);
        assert!(rebuilt.contains("line 0 of"));
        assert!(rebuilt.contains("line 3999 of"));
    }

    #[test]
    fn a_follow_up_appends_rather_than_overwrites() {
        let _exclusive = exclusive();
        reset();
        let id = open("s", "research-agent", "first", serde_json::json!({}));
        settle("s", &id, settled("first pass"));
        let _ = check_out("s", &id);
        note_followup("s", &id, "now fix it");
        settle("s", &id, settled("second pass"));
        let chunk = read("s", &id, 0).unwrap();
        assert!(chunk.text.contains("first pass"));
        assert!(chunk.text.contains("now fix it"));
        assert!(chunk.text.contains("second pass"));
        assert_eq!(list("s")[0].turns, 6, "turns accumulate across revivals");
    }

    #[test]
    fn the_directory_stays_bounded() {
        let _exclusive = exclusive();
        reset();
        for i in 0..MAX_TRACKED_DELEGATES + 8 {
            let id = open("s", &format!("agent-{i}"), "t", serde_json::json!({}));
            settle("s", &id, settled("x"));
        }
        assert_eq!(list("s").len(), MAX_TRACKED_DELEGATES);
        assert!(
            list("s").iter().all(|r| r.id != "agent-0"),
            "eviction drops the oldest, not the newest"
        );
    }
}
