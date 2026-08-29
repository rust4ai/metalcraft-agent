//! The nightly dream — where raw turn material becomes memory.
//!
//! Capture (`capture.rs`) is deliberately dumb: a turn appends one line and pays
//! nothing. That bet only works if something drains the queue, and this is that
//! something. Five stages, run per agent instance, while nobody is waiting:
//!
//! | # | Stage | Cost |
//! |---|---|---|
//! | 1 | **Index** — drain captures, derive episodes, dedupe | mechanical |
//! | 2 | **Consolidate** — merge near-duplicate memories | one LLM call per cluster |
//! | 3 | **Abstract** — episode → durable facts, preferences, methods | one LLM call per episode |
//! | 4 | **Associate** — link, adjudicate contradictions, reflect | mechanical + 2 LLM calls |
//! | 5 | **Decay** — importance decay, archive, purge | mechanical |
//!
//! ## Three decisions worth stating
//!
//! **The dream writes memories from parsed JSON, not by letting a model call
//! `mem_remember`.** `docs/MEMORY_SYSTEM_PLAN.md` §5 imagined the discretionary
//! stages as an agent run with the `mem_*` tools attached. That makes the dream's
//! output whatever the model felt like doing that night — uncapped, untyped, and
//! impossible to journal honestly. Here every stage asks a *plain completion* for
//! JSON and this module does the writing, so every write is capped by code,
//! carries its provenance link, and lands in the run report. It also means the
//! dream cannot be prompt-injected into deleting memories: there is no forget
//! path in this file that a model's output can reach.
//!
//! **Similarity is lexical, because there are no embeddings.** The plan's
//! `cosine ≥ 0.86` gates do not exist to be implemented — the vector leg was
//! removed (see [`super`]). Near-duplicate detection instead shortlists with the
//! BM25 index the store already maintains and gates on Jaccard token overlap.
//! Cheaper, and honest about what it can and cannot catch: it finds rephrasings,
//! it will not find two sentences that share no vocabulary.
//!
//! **The write lock is never held across an LLM call.** Each stage gathers under
//! a read lock, releases, thinks, then re-acquires to apply. A dream takes
//! minutes; a turn arriving in the middle of one must not block on it.
//!
//! ## Failure
//!
//! A stage that fails is recorded in the report and the next one still runs. A
//! dream that fails entirely leaves the capture queue untouched, so tomorrow's
//! run re-reads the same material rather than losing it. Nothing here can fail a
//! turn: the loop is spawned detached and every path returns a report rather
//! than propagating an error.
use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::capture::{Capture, CaptureKind};
use super::index::MemoryIndex;
use super::instance::InstanceMemory;
use super::types::{Event, Link, LinkKind, Memory, MemoryKind, Source, content_hash};

// ── configuration ────────────────────────────────────────────────────────────

fn env_str(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    env_str(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_flag(key: &str, default: bool) -> bool {
    match env_str(key) {
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => default,
    }
}

/// Whether the nightly loop runs at all (`MEMORY_DREAM`).
///
/// Separate from [`super::enabled`] so a pod can keep memory — recall, the
/// profile block, `mem_remember` — while opting out of the part that spends
/// money overnight. On-demand dreams still work when this is off; turning off a
/// schedule is not the same as removing the capability.
pub fn nightly_enabled() -> bool {
    super::enabled() && env_flag("MEMORY_DREAM", true)
}

/// Six-field cron, like every other schedule in the product (`MEMORY_DREAM_CRON`).
pub fn cron_expr() -> String {
    env_str("MEMORY_DREAM_CRON").unwrap_or_else(|| "0 30 3 * * *".to_string())
}

/// IANA zone the cron is read in (`MEMORY_DREAM_TZ`). Unset means the pod's clock,
/// which in the cluster is UTC.
pub fn timezone() -> Option<String> {
    env_str("MEMORY_DREAM_TZ")
}

/// The model the discretionary stages think with (`MEMORY_DREAM_MODEL`).
///
/// Defaults to the pod's configured model rather than something cheaper: the
/// stages that cost money are the ones that decide what this agent will believe
/// for the next six months, and a bad merge is much more expensive than the
/// tokens it saved.
pub fn model_name() -> String {
    env_str("MEMORY_DREAM_MODEL").unwrap_or_else(crate::runtime::configured_default_model)
}

/// Which stages to run (`MEMORY_DREAM_STAGES`, e.g. `1,5`). Default: all of them.
pub fn configured_stages() -> Vec<u8> {
    match env_str("MEMORY_DREAM_STAGES") {
        Some(v) => {
            let mut s: Vec<u8> = v
                .split(',')
                .filter_map(|p| p.trim().parse::<u8>().ok())
                .filter(|n| (1..=5).contains(n))
                .collect();
            s.sort_unstable();
            s.dedup();
            s
        }
        None => vec![1, 2, 3, 4, 5],
    }
}

/// How recently an agent must have been used to be dreamt for
/// (`MEMORY_DREAM_ACTIVE_DAYS`).
pub fn active_days() -> i64 {
    env_num("MEMORY_DREAM_ACTIVE_DAYS", 3i64).max(1)
}

/// Silence that ends an episode (`MEMORY_EPISODE_IDLE_MINUTES`).
fn episode_idle_minutes() -> i64 {
    env_num("MEMORY_EPISODE_IDLE_MINUTES", 90i64).max(1)
}

/// Importance half-life in days (`MEMORY_HALF_LIFE_DAYS`).
fn half_life_days() -> f32 {
    env_num("MEMORY_HALF_LIFE_DAYS", 45.0f32).max(1.0)
}

/// How long an archived memory waits before it is dropped for good
/// (`MEMORY_PURGE_AFTER_DAYS`).
fn purge_after_days() -> i64 {
    env_num("MEMORY_PURGE_AFTER_DAYS", 180i64).max(1)
}

/// Merge clusters considered per run (`MEMORY_DREAM_MAX_MERGE`).
fn max_merge() -> usize {
    env_num("MEMORY_DREAM_MAX_MERGE", 50usize)
}

/// Episodes distilled per run (`MEMORY_DREAM_MAX_EPISODES`).
fn max_episodes() -> usize {
    env_num("MEMORY_DREAM_MAX_EPISODES", 20usize)
}

/// Token overlap at which two same-kind memories are merge candidates
/// (`MEMORY_DREAM_MERGE_SIMILARITY`).
fn merge_similarity() -> f32 {
    env_num("MEMORY_DREAM_MERGE_SIMILARITY", 0.6f32).clamp(0.2, 1.0)
}

/// Token overlap at which two memories get a `RelatesTo` edge
/// (`MEMORY_DREAM_RELATE_SIMILARITY`). Lower than the merge gate on purpose:
/// "these are about the same thing" is a much weaker claim than "these are the
/// same thing".
fn relate_similarity() -> f32 {
    env_num("MEMORY_DREAM_RELATE_SIMILARITY", 0.35f32).clamp(0.1, 1.0)
}

/// Journals kept per agent. Enough to see a pattern, bounded so the directory
/// cannot grow without limit on a pod nobody prunes.
const JOURNALS_KEPT: usize = 30;

/// Below this decayed importance a memory is archived.
const ARCHIVE_BELOW: f32 = 1.5;

/// Largest merge cluster. Beyond a handful the merge prompt stops being "say
/// these more concisely" and starts being "write an essay".
const MAX_CLUSTER: usize = 8;

/// Insights one reflection may produce. The hard cap is deliberate: reflection is
/// where a memory system goes off the rails if allowed to be prolific.
const MAX_INSIGHTS: usize = 3;

// ── the report ───────────────────────────────────────────────────────────────

/// Why a dream ran. Journaled, because "did the schedule fire or did someone
/// press the button?" is the first question asked of a surprising run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The cron fired.
    Nightly,
    /// The pod was down when the cron fired and noticed on boot.
    Catchup,
    /// A person or the agent asked for it now.
    Manual,
}

impl Trigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nightly => "nightly",
            Self::Catchup => "catchup",
            Self::Manual => "manual",
        }
    }
}

/// What one stage did. `counts` is open-ended on purpose — each stage reports the
/// numbers that mean something for it, and a fixed struct would be mostly zeros.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StageReport {
    pub stage: u8,
    pub name: String,
    /// False when `MEMORY_DREAM_STAGES` excluded it.
    pub ran: bool,
    #[schema(value_type = Object)]
    pub counts: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub millis: u64,
}

impl StageReport {
    fn skipped(stage: u8, name: &str) -> Self {
        Self {
            stage,
            name: name.to_string(),
            ran: false,
            counts: BTreeMap::new(),
            error: None,
            millis: 0,
        }
    }
}

/// One run of the dream, for one agent. Written to
/// `<data>/memory/instances/<id>/dreams/<ts>.json` and returned by the on-demand
/// entry points.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DreamReport {
    pub instance_id: String,
    pub trigger: String,
    pub model: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub stages: Vec<StageReport>,
    /// Live memories before and after, so the run's net effect is one subtraction
    /// rather than a sum over stage counts that double-count merges.
    pub memories_before: usize,
    pub memories_after: usize,
    pub captures_pending_before: usize,
    pub captures_pending_after: usize,
    /// Whether the log was folded into a fresh snapshot at the end.
    pub snapshot_written: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DreamReport {
    /// A one-line human summary — the morning-message form.
    pub fn headline(&self) -> String {
        let n = |stage: u8, key: &str| -> usize {
            self.stages
                .iter()
                .find(|s| s.stage == stage)
                .and_then(|s| s.counts.get(key))
                .copied()
                .unwrap_or(0)
        };
        format!(
            "distilled {} episode(s), learned {} thing(s), merged {}, linked {}, archived {}",
            n(3, "episodes_distilled"),
            n(3, "memories_extracted") + n(4, "insights"),
            n(2, "merged"),
            n(4, "links_created"),
            n(5, "archived"),
        )
    }
}

// ── the entry points ─────────────────────────────────────────────────────────

/// Dream for one agent, now.
///
/// This is the whole engine's front door: the nightly loop calls it per active
/// instance, and `mem_dream_now` and `POST …/memory/dream` call it directly.
/// Always returns a report — a failed stage is data, not an error.
pub async fn dream(instance_id: &str, trigger: Trigger) -> DreamReport {
    dream_stages(instance_id, trigger, &configured_stages()).await
}

/// Dream a chosen subset of stages. Exists for debugging and for the on-demand
/// callers, which let you ask for just the mechanical half.
pub async fn dream_stages(instance_id: &str, trigger: Trigger, want: &[u8]) -> DreamReport {
    let started_at = Utc::now();
    let model = model_name();
    let mut report = DreamReport {
        instance_id: instance_id.to_string(),
        trigger: trigger.as_str().to_string(),
        model: model.clone(),
        started_at,
        finished_at: started_at,
        stages: Vec::new(),
        memories_before: 0,
        memories_after: 0,
        captures_pending_before: super::capture::pending_count(instance_id),
        captures_pending_after: 0,
        snapshot_written: false,
        error: None,
    };

    if !super::enabled() {
        report.error = Some("memory is disabled (MEMORY_ENABLED)".into());
        report.finished_at = Utc::now();
        return report;
    }

    let mem = match super::handle_for_instance(instance_id) {
        Ok(m) => m,
        Err(e) => {
            report.error = Some(e);
            report.finished_at = Utc::now();
            return report;
        }
    };

    report.memories_before = mem.delta.read().await.iter().filter(|m| m.is_live()).count();
    log::info!(
        "memory: dreaming for '{instance_id}' ({}) — {} pending capture(s), stages {want:?}",
        trigger.as_str(),
        report.captures_pending_before
    );

    let ctx = Ctx {
        instance_id: instance_id.to_string(),
        mem,
        model,
    };

    for (stage, name) in [
        (1u8, "index"),
        (2, "consolidate"),
        (3, "abstract"),
        (4, "associate"),
        (5, "decay"),
    ] {
        if !want.contains(&stage) {
            report.stages.push(StageReport::skipped(stage, name));
            continue;
        }
        let began = std::time::Instant::now();
        let result = match stage {
            1 => stage_index(&ctx).await,
            2 => stage_consolidate(&ctx).await,
            3 => stage_abstract(&ctx).await,
            4 => stage_associate(&ctx).await,
            _ => stage_decay(&ctx).await,
        };
        let millis = began.elapsed().as_millis() as u64;
        let (counts, error) = match result {
            Ok(c) => (c, None),
            Err(e) => {
                // Loud, but not fatal: the remaining stages are independent, and a
                // failed consolidation must not cost the night's decay pass.
                log::warn!("memory: dream stage {stage} ({name}) failed for '{instance_id}': {e}");
                (BTreeMap::new(), Some(e))
            }
        };
        report.stages.push(StageReport {
            stage,
            name: name.to_string(),
            ran: true,
            counts,
            error,
            millis,
        });
    }

    // Fold the night's writes into a snapshot and drop the log. Last, not in
    // stage 1, because stages 2–5 all append: compacting first would leave the
    // biggest part of the run un-compacted.
    report.snapshot_written = compact_log(&ctx).await;
    report.memories_after = ctx
        .mem
        .delta
        .read()
        .await
        .iter()
        .filter(|m| m.is_live())
        .count();
    report.captures_pending_after = super::capture::pending_count(instance_id);
    report.finished_at = Utc::now();

    log::info!(
        "memory: dream for '{instance_id}' finished in {}s — {}",
        (report.finished_at - report.started_at).num_seconds(),
        report.headline()
    );
    write_journal(instance_id, &report);
    report
}

/// Everything a stage needs: which agent, its layers, and what to think with.
struct Ctx {
    instance_id: String,
    mem: InstanceMemory,
    model: String,
}

impl Ctx {
    fn wal(&self) -> std::path::PathBuf {
        crate::paths::memory_instance_dir(&self.instance_id).join("wal.jsonl")
    }

    /// Apply a batch of events under one write lock.
    ///
    /// Batched rather than one call per event because the lock is the thing a
    /// concurrent turn waits on, and stage 5 can touch every memory the agent
    /// has. Log-before-apply per event, so a failed append leaves RAM and disk
    /// agreeing.
    async fn commit(&self, events: Vec<PendingEvent>) -> Result<usize, String> {
        if events.is_empty() {
            return Ok(0);
        }
        let path = self.wal();
        let mut delta = self.mem.delta.write().await;
        let mut written = 0usize;
        for pending in events {
            delta.seq += 1;
            let event = pending.into_event(delta.seq);
            super::commit_to(&path, &mut delta, event, false)?;
            written += 1;
        }
        Ok(written)
    }
}

/// An event with its sequence number not yet assigned — stages build these while
/// holding no lock, and `Ctx::commit` numbers them in order at apply time.
enum PendingEvent {
    Upsert(Box<Memory>),
    Link(Link),
    Archive(String),
    Purge(String),
}

impl PendingEvent {
    fn into_event(self, seq: u64) -> Event {
        let at = Utc::now();
        match self {
            Self::Upsert(memory) => Event::Upsert { seq, at, memory },
            Self::Link(link) => Event::Link { seq, at, link },
            Self::Archive(id) => Event::Archive { seq, at, id },
            Self::Purge(id) => Event::Purge { seq, at, id },
        }
    }
}

fn link(src: &str, dst: &str, kind: LinkKind, weight: f32) -> PendingEvent {
    PendingEvent::Link(Link {
        src: src.to_string(),
        dst: dst.to_string(),
        kind,
        weight,
        created_by: "dream".to_string(),
    })
}

// ── stage 1: index ───────────────────────────────────────────────────────────

/// One conversation's worth of captured material, ready to distil.
struct Episode {
    id: String,
    chat_id: Option<String>,
    persona: Option<String>,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    capture_ids: Vec<String>,
    turns: usize,
    tools: Vec<String>,
    transcript: String,
}

/// Drain the capture queue into episodes.
///
/// The queue holds turns, compaction summaries, and end-of-session markers, in
/// time order. An episode is a run of captures from one chat with no gap longer
/// than `MEMORY_EPISODE_IDLE_MINUTES`, terminated by a `SessionEnd` marker or by
/// that gap. There is no episode state machine anywhere else in the system, and
/// this is why: deriving the boundary at dream time means there is no lifecycle
/// to leave half-open when a pod is killed mid-conversation.
///
/// An episode whose last capture is still inside the idle window is **left
/// pending** — it is a conversation the user may still be having, and distilling
/// it now would produce a memory of half a thought.
fn group_episodes(captures: &[Capture], now: DateTime<Utc>) -> (Vec<Episode>, usize) {
    let idle = Duration::minutes(episode_idle_minutes());
    let mut by_chat: HashMap<String, Vec<&Capture>> = HashMap::new();
    for c in captures {
        // Captures with no chat share one bucket: a one-shot run or a flow node
        // has no conversation to group by, but it still has a clock.
        let key = c.chat_id.clone().unwrap_or_else(|| "(unbound)".to_string());
        by_chat.entry(key).or_default().push(c);
    }

    let mut episodes = Vec::new();
    let mut still_open = 0usize;
    for (_chat, mut group) in by_chat {
        group.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        let mut run: Vec<&Capture> = Vec::new();
        let mut closed_by_marker = false;

        for c in group {
            let gap_broke = run
                .last()
                .is_some_and(|prev: &&Capture| c.at - prev.at > idle);
            if gap_broke && !run.is_empty() {
                if let Some(ep) = build_episode(&run, closed_by_marker) {
                    episodes.push(ep);
                }
                run.clear();
                closed_by_marker = false;
            }
            if c.kind == CaptureKind::SessionEnd {
                // The marker closes the run it ends and is consumed with it, so
                // its id is still marked processed.
                run.push(c);
                if let Some(ep) = build_episode(&run, true) {
                    episodes.push(ep);
                }
                run.clear();
                closed_by_marker = false;
                continue;
            }
            run.push(c);
        }

        // Whatever is left is only distillable if the conversation has gone quiet.
        if let Some(last) = run.last() {
            if now - last.at > idle {
                if let Some(ep) = build_episode(&run, false) {
                    episodes.push(ep);
                }
            } else {
                still_open += 1;
            }
        }
    }

    episodes.sort_by_key(|e| e.started_at);
    (episodes, still_open)
}

/// Render a run of captures as one episode. Returns `None` when the run holds no
/// distillable content — a bare `SessionEnd` marker for a conversation whose
/// turns were distilled last night is a real and common case.
fn build_episode(run: &[&Capture], _closed_by_marker: bool) -> Option<Episode> {
    if !run.iter().any(|c| c.has_content()) {
        return None;
    }
    let first = run.first()?;
    let last = run.last()?;
    let mut tools: Vec<String> = Vec::new();
    let mut turns = 0usize;
    let mut transcript = String::new();
    for c in run {
        for t in &c.tools {
            if !tools.contains(t) {
                tools.push(t.clone());
            }
        }
        match c.kind {
            CaptureKind::Turn => {
                turns += 1;
                if !c.user_text.trim().is_empty() {
                    transcript.push_str("User: ");
                    transcript.push_str(c.user_text.trim());
                    transcript.push('\n');
                }
                if !c.agent_text.trim().is_empty() {
                    transcript.push_str("Agent: ");
                    transcript.push_str(c.agent_text.trim());
                    transcript.push('\n');
                }
                if !c.tools.is_empty() {
                    transcript.push_str("Tools used: ");
                    transcript.push_str(&c.tools.join(", "));
                    transcript.push('\n');
                }
            }
            CaptureKind::Compaction => {
                // Already an LLM's summary of a longer stretch — the densest
                // material in the queue, so it is labelled rather than blended in.
                transcript.push_str("Earlier in this conversation (summary): ");
                transcript.push_str(c.agent_text.trim());
                transcript.push('\n');
            }
            CaptureKind::SessionEnd => {}
        }
        transcript.push('\n');
    }

    Some(Episode {
        id: uuid::Uuid::new_v4().to_string(),
        chat_id: first.chat_id.clone(),
        persona: run.iter().find_map(|c| c.persona.clone()),
        started_at: first.at,
        ended_at: last.at,
        capture_ids: run.iter().map(|c| c.id.clone()).collect(),
        turns,
        tools,
        transcript,
    })
}

/// Stage 1. Mechanical: drain the queue into `Episodic` memories, then clear the
/// captures those memories now stand for.
///
/// The episode memory holds the transcript verbatim (bounded). Stage 3 replaces
/// nothing and adds a summary plus extractions — the raw text stays, because the
/// provenance link from an extracted fact has to point at something a person can
/// read when they ask "why do you think that?".
async fn stage_index(ctx: &Ctx) -> Result<BTreeMap<String, usize>, String> {
    let mut counts = BTreeMap::new();
    let pending = super::capture::pending(&ctx.instance_id);
    counts.insert("captures_read".into(), pending.len());
    if pending.is_empty() {
        return Ok(counts);
    }

    let (episodes, still_open) = group_episodes(&pending, Utc::now());
    counts.insert("episodes_open".into(), still_open);
    counts.insert("episodes_closed".into(), episodes.len());

    let existing_hashes: HashSet<String> = {
        let delta = ctx.mem.delta.read().await;
        delta.iter().map(|m| m.content_hash.clone()).collect()
    };

    let mut events = Vec::new();
    let mut processed: Vec<String> = Vec::new();
    let mut duplicates = 0usize;
    for ep in &episodes {
        // Every capture the episode covers is processed whether or not it
        // produces a memory — otherwise a duplicate episode is re-read forever.
        processed.extend(ep.capture_ids.iter().cloned());

        let content = episode_content(ep);
        if existing_hashes.contains(&content_hash(&content)) {
            duplicates += 1;
            continue;
        }
        let mut m = Memory::new(MemoryKind::Episodic, content, Source::Turn);
        m.episode_id = Some(ep.id.clone());
        m.chat_id = ep.chat_id.clone();
        m.persona = ep.persona.clone();
        m.occurred_at = Some(ep.started_at);
        m.confidence = 0.8;
        // An episode's own importance is modest — it is raw material. What stage 3
        // lifts out of it is what deserves to survive the decay pass.
        m.importance = 4.0;
        events.push(PendingEvent::Upsert(Box::new(m)));
    }

    counts.insert("episodes_duplicate".into(), duplicates);
    counts.insert("episodes_created".into(), ctx.commit(events).await?);

    // Only now: a crash before this point re-reads the material, which is the
    // safe direction. Doing it first would lose a night's captures to a failed
    // append.
    match super::capture::retain_pending(&ctx.instance_id, &processed) {
        Ok(dropped) => {
            counts.insert("captures_drained".into(), dropped);
        }
        Err(e) => return Err(format!("could not rewrite the capture queue: {e}")),
    }
    Ok(counts)
}

/// The transcript, headed by what the episode was. Bounded: an episode with two
/// hundred turns is not more distillable than one with the first fifty.
fn episode_content(ep: &Episode) -> String {
    const MAX_TRANSCRIPT_CHARS: usize = 24_000;
    let minutes = (ep.ended_at - ep.started_at).num_minutes().max(0);
    let mut head = format!(
        "Conversation on {} ({} turn(s) over {minutes} minute(s))",
        ep.started_at.format("%Y-%m-%d %H:%M UTC"),
        ep.turns
    );
    if let Some(p) = &ep.persona {
        head.push_str(&format!(", as {p}"));
    }
    if !ep.tools.is_empty() {
        head.push_str(&format!(". Tools used: {}", ep.tools.join(", ")));
    }
    head.push_str(".\n\n");

    let body = ep.transcript.trim();
    if body.chars().count() > MAX_TRANSCRIPT_CHARS {
        let kept: String = body.chars().take(MAX_TRANSCRIPT_CHARS).collect();
        format!("{head}{kept}\n[…transcript truncated]")
    } else {
        format!("{head}{body}")
    }
}

// ── stage 2: consolidate ─────────────────────────────────────────────────────

/// Stage 2. Merge memories that say overlapping things.
///
/// Candidates are shortlisted with BM25 (the memory's own text as the query) and
/// gated on token overlap, then clustered transitively. Each cluster is one LLM
/// call, and the model may decline — `KEEP_SEPARATE` is a first-class answer,
/// because two similar-sounding preferences are often two real preferences.
///
/// Originals are **superseded, not deleted**: the merge stays auditable, inbound
/// links keep resolving, and a bad merge is a thing you can look at rather than a
/// thing you can only notice by its absence.
async fn stage_consolidate(ctx: &Ctx) -> Result<BTreeMap<String, usize>, String> {
    let mut counts = BTreeMap::new();
    let clusters = {
        let delta = ctx.mem.delta.read().await;
        find_clusters(&delta, merge_similarity(), max_merge())
    };
    counts.insert("clusters".into(), clusters.len());
    if clusters.is_empty() {
        return Ok(counts);
    }

    let mut merged = 0usize;
    let mut kept_separate = 0usize;
    let mut superseded = 0usize;
    for cluster in clusters {
        let answer = match ask_json::<MergeAnswer>(
            &ctx.model,
            MERGE_SYSTEM,
            &merge_prompt(&cluster),
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                log::debug!("memory: dream merge call failed: {e}");
                continue;
            }
        };
        if !answer.merge || answer.content.trim().is_empty() {
            kept_separate += 1;
            continue;
        }

        let mut survivor = Memory::new(MemoryKind::Episodic, answer.content.trim(), Source::Dream);
        survivor.kind = cluster[0].kind;
        survivor.summary = answer.summary.unwrap_or_default();
        survivor.entity = answer
            .entity
            .filter(|e| !e.trim().is_empty())
            .or_else(|| cluster.iter().find_map(|m| m.entity.clone()));
        survivor.tags = {
            let mut tags: Vec<String> = Vec::new();
            for m in &cluster {
                for t in &m.tags {
                    if !tags.contains(t) {
                        tags.push(t.clone());
                    }
                }
            }
            tags
        };
        survivor.importance = cluster.iter().fold(0.0f32, |a, m| a.max(m.importance));
        survivor.confidence = cluster.iter().fold(1.0f32, |a, m| a.min(m.confidence));
        survivor.pinned = cluster.iter().any(|m| m.pinned);
        survivor.chat_id = cluster.iter().find_map(|m| m.chat_id.clone());
        survivor.occurred_at = cluster.iter().filter_map(|m| m.occurred_at).min();
        survivor.created_at = cluster.iter().map(|m| m.created_at).min().unwrap_or(survivor.created_at);

        let mut events = vec![PendingEvent::Upsert(Box::new(survivor.clone()))];
        for original in &cluster {
            let mut retired = original.clone();
            retired.superseded_by = Some(survivor.id.clone());
            retired.updated_at = Utc::now();
            events.push(PendingEvent::Upsert(Box::new(retired)));
            events.push(link(&survivor.id, &original.id, LinkKind::Supersedes, 1.0));
            superseded += 1;
        }
        // Inbound provenance follows the survivor, or a derived fact would point
        // at a memory recall can no longer return.
        {
            let delta = ctx.mem.delta.read().await;
            for original in &cluster {
                for inbound in delta.links_to(&original.id) {
                    if inbound.kind == LinkKind::Supersedes {
                        continue;
                    }
                    events.push(link(
                        &inbound.src,
                        &survivor.id,
                        inbound.kind,
                        inbound.weight,
                    ));
                }
            }
        }
        ctx.commit(events).await?;
        merged += 1;
    }

    counts.insert("merged".into(), merged);
    counts.insert("kept_separate".into(), kept_separate);
    counts.insert("superseded".into(), superseded);
    Ok(counts)
}

/// Transitively cluster near-duplicate memories, best-scoring pairs first.
///
/// Episodic memories are excluded: two conversations that covered the same
/// ground are still two conversations, and merging them would destroy the
/// provenance stage 3's extractions hang off.
fn find_clusters(idx: &MemoryIndex, threshold: f32, max_clusters: usize) -> Vec<Vec<Memory>> {
    let live: Vec<&Memory> = idx
        .iter()
        .filter(|m| m.is_live() && m.kind != MemoryKind::Episodic)
        .collect();
    if live.len() < 2 {
        return Vec::new();
    }
    let tokens: HashMap<&str, HashSet<String>> = live
        .iter()
        .map(|m| {
            (
                m.id.as_str(),
                super::index::tokenize(&m.indexable()).into_iter().collect(),
            )
        })
        .collect();

    // Union-find over qualifying pairs. Transitive closure is what makes a
    // cluster rather than a pile of pairs the model would see three times.
    let mut parent: HashMap<String, String> =
        live.iter().map(|m| (m.id.clone(), m.id.clone())).collect();
    // `get` rather than indexing: this runs in a detached nightly loop, and an id
    // that somehow escaped the map would take the whole dream down with a panic
    // rather than costing one missed merge.
    fn root(parent: &HashMap<String, String>, id: &str) -> String {
        let mut cur = id.to_string();
        while let Some(next) = parent.get(&cur) {
            if next == &cur {
                break;
            }
            cur = next.clone();
        }
        cur
    }

    for m in &live {
        // BM25 shortlists; the overlap gate decides. Searching by the memory's own
        // text is what keeps this O(n · small) instead of O(n²).
        for hit in idx.search(&m.indexable(), 12, Some(m.kind)) {
            if hit.id == m.id {
                continue;
            }
            let (Some(a), Some(b)) = (tokens.get(m.id.as_str()), tokens.get(hit.id.as_str()))
            else {
                continue;
            };
            if jaccard(a, b) < threshold {
                continue;
            }
            let (ra, rb) = (root(&parent, &m.id), root(&parent, &hit.id));
            if ra != rb {
                parent.insert(ra, rb);
            }
        }
    }

    let mut groups: HashMap<String, Vec<&Memory>> = HashMap::new();
    for m in &live {
        groups.entry(root(&parent, &m.id)).or_default().push(m);
    }

    let mut clusters: Vec<Vec<Memory>> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            // Oldest first: the merge prompt reads as a history, and the survivor
            // inherits the earliest creation date.
            g.sort_by_key(|m| m.created_at);
            g.truncate(MAX_CLUSTER);
            g.into_iter().cloned().collect()
        })
        .collect();
    // Biggest clusters first — under a cap, the most redundancy removed per call.
    clusters.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].id.cmp(&b[0].id)));
    clusters.truncate(max_clusters);
    clusters
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    inter / union
}

// ── stage 3: abstract ────────────────────────────────────────────────────────

/// Stage 3. The episodic → semantic lift, and the reason the system exists.
///
/// One call per undistilled episode, producing a summary for the episode itself
/// plus durable extractions. Each extraction gets a `DerivedFrom` edge back to
/// the episode, so every claim the agent makes traces to the conversation that
/// produced it.
///
/// **Dream output is not input here.** An episode written by a previous dream is
/// never re-distilled, and `Source::Dream` memories are invisible to this stage —
/// otherwise the agent abstracts its own abstractions and drifts into confident
/// nonsense over a few weeks.
async fn stage_abstract(ctx: &Ctx) -> Result<BTreeMap<String, usize>, String> {
    let mut counts = BTreeMap::new();
    let episodes: Vec<Memory> = {
        let delta = ctx.mem.delta.read().await;
        let mut pending: Vec<Memory> = delta
            .iter()
            .filter(|m| {
                m.is_live()
                    && m.kind == MemoryKind::Episodic
                    && m.source != Source::Dream
                    && m.summary.trim().is_empty()
            })
            .cloned()
            .collect();
        pending.sort_by_key(|m| m.created_at);
        pending.truncate(max_episodes());
        pending
    };
    counts.insert("episodes_pending".into(), episodes.len());
    if episodes.is_empty() {
        return Ok(counts);
    }

    let known: HashSet<String> = {
        let delta = ctx.mem.delta.read().await;
        delta.iter().map(|m| m.content_hash.clone()).collect()
    };
    let mut seen = known;
    let mut distilled = 0usize;
    let mut extracted = 0usize;
    let mut skipped_duplicate = 0usize;

    for episode in episodes {
        let answer = match ask_json::<AbstractAnswer>(
            &ctx.model,
            ABSTRACT_SYSTEM,
            &format!(
                "Distil this conversation.\n\n---\n{}\n---",
                episode.content
            ),
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                log::debug!("memory: dream abstraction call failed: {e}");
                continue;
            }
        };

        let mut events = Vec::new();
        let mut updated = episode.clone();
        // A summary is written even when nothing was extracted: it is what marks
        // the episode distilled, and it is what recall shows instead of a
        // transcript once the dream has read it.
        updated.summary = answer
            .summary
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| answer.title.clone().unwrap_or_else(|| "A conversation.".into()))
            .trim()
            .to_string();
        updated.updated_at = Utc::now();
        events.push(PendingEvent::Upsert(Box::new(updated)));

        for item in answer.memories.into_iter().take(12) {
            let Some(kind) = MemoryKind::parse(&item.kind) else {
                continue;
            };
            // Episodes and insights are not extractable: an episode is a record of
            // something that happened (stage 1 owns those) and an insight is a
            // cross-memory pattern (stage 4 owns those). Letting this stage emit
            // either would let one conversation manufacture its own conclusions.
            if matches!(kind, MemoryKind::Episodic | MemoryKind::Insight) {
                continue;
            }
            let content = super::redact::redact(item.content.trim()).content;
            if content.is_empty() {
                continue;
            }
            let hash = content_hash(&content);
            if !seen.insert(hash) {
                skipped_duplicate += 1;
                continue;
            }
            let mut m = Memory::new(kind, content, Source::Dream);
            m.entity = item.entity.filter(|e| !e.trim().is_empty());
            m.episode_id = episode.episode_id.clone();
            m.chat_id = episode.chat_id.clone();
            m.persona = episode.persona.clone();
            m.occurred_at = episode.occurred_at;
            m.importance = item.importance.unwrap_or(5.0).clamp(0.0, 10.0);
            m.confidence = 0.9;
            events.push(link(&m.id, &episode.id, LinkKind::DerivedFrom, 1.0));
            events.push(PendingEvent::Upsert(Box::new(m)));
            extracted += 1;
        }

        ctx.commit(events).await?;
        distilled += 1;
    }

    counts.insert("episodes_distilled".into(), distilled);
    counts.insert("memories_extracted".into(), extracted);
    counts.insert("extractions_duplicate".into(), skipped_duplicate);
    Ok(counts)
}

// ── stage 4: associate & reflect ─────────────────────────────────────────────

/// Stage 4. Build the graph, resolve what contradicts what, and notice patterns.
///
/// Three parts, in ascending order of how much they can go wrong:
///
/// 1. **Mechanical linking** — `AboutEntity` between memories sharing a canonical
///    entity, `RelatesTo` between lexically overlapping ones. Free, and the graph
///    leg of recall is useless without it.
/// 2. **Contradiction adjudication** — one batched call over candidate pairs that
///    share an entity and carry a negation or correction signal. A confirmed
///    contradiction lowers the loser's confidence; a clean update supersedes it.
/// 3. **Reflection** — at most [`MAX_INSIGHTS`] `Insight` memories, each linked
///    to the memories that support it.
async fn stage_associate(ctx: &Ctx) -> Result<BTreeMap<String, usize>, String> {
    let mut counts = BTreeMap::new();

    // ── mechanical ──
    let (mechanical, live) = {
        let delta = ctx.mem.delta.read().await;
        let live: Vec<Memory> = delta.iter().filter(|m| m.is_live()).cloned().collect();
        (mechanical_links(&delta, &live), live)
    };
    counts.insert("links_created".into(), ctx.commit(mechanical).await?);

    // ── contradictions ──
    let candidates = contradiction_candidates(&live);
    counts.insert("contradiction_candidates".into(), candidates.len());
    if !candidates.is_empty() {
        match ask_json::<ContradictionAnswer>(
            &ctx.model,
            CONTRADICTION_SYSTEM,
            &contradiction_prompt(&candidates),
        )
        .await
        {
            Ok(answer) => {
                let by_id: HashMap<&str, &Memory> =
                    live.iter().map(|m| (m.id.as_str(), m)).collect();
                let mut events = Vec::new();
                let mut resolved = 0usize;
                for verdict in answer.verdicts.into_iter().take(candidates.len()) {
                    let (Some(older), Some(newer)) = (
                        by_id.get(verdict.older_id.as_str()),
                        by_id.get(verdict.newer_id.as_str()),
                    ) else {
                        continue;
                    };
                    match verdict.relation.trim().to_ascii_lowercase().as_str() {
                        "supersedes" => {
                            let mut retired = (*older).clone();
                            retired.superseded_by = Some(newer.id.clone());
                            retired.updated_at = Utc::now();
                            events.push(PendingEvent::Upsert(Box::new(retired)));
                            events.push(link(&newer.id, &older.id, LinkKind::Supersedes, 1.0));
                            resolved += 1;
                        }
                        "contradicts" => {
                            // Both survive, the older one trusted less. A flat
                            // deletion here would be the system quietly picking a
                            // winner between two things a person said.
                            let mut doubted = (*older).clone();
                            doubted.confidence = (doubted.confidence * 0.5).max(0.1);
                            doubted.updated_at = Utc::now();
                            events.push(PendingEvent::Upsert(Box::new(doubted)));
                            events.push(link(&newer.id, &older.id, LinkKind::Contradicts, 1.0));
                            resolved += 1;
                        }
                        _ => {}
                    }
                }
                counts.insert("contradictions_resolved".into(), resolved);
                ctx.commit(events).await?;
            }
            Err(e) => log::debug!("memory: dream contradiction call failed: {e}"),
        }
    }

    // ── reflection ──
    let mut recent: Vec<&Memory> = live
        .iter()
        .filter(|m| m.kind != MemoryKind::Insight && m.kind != MemoryKind::Episodic)
        .collect();
    recent.sort_by(|a, b| {
        b.created_at.cmp(&a.created_at).then_with(|| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    recent.truncate(24);
    if recent.len() < 3 {
        // Nothing to see a pattern across. Reflecting over two memories produces
        // a restatement of one of them.
        counts.insert("insights".into(), 0);
        return Ok(counts);
    }

    let existing: HashSet<String> = live.iter().map(|m| m.content_hash.clone()).collect();
    match ask_json::<ReflectAnswer>(&ctx.model, REFLECT_SYSTEM, &reflect_prompt(&recent)).await {
        Ok(answer) => {
            let by_id: HashSet<&str> = live.iter().map(|m| m.id.as_str()).collect();
            let mut events = Vec::new();
            let mut written = 0usize;
            for insight in answer.insights.into_iter().take(MAX_INSIGHTS) {
                let content = super::redact::redact(insight.content.trim()).content;
                if content.is_empty() || existing.contains(&content_hash(&content)) {
                    continue;
                }
                let mut m = Memory::new(MemoryKind::Insight, content, Source::Dream);
                m.importance = insight.importance.unwrap_or(6.0).clamp(0.0, 10.0);
                m.confidence = 0.7;
                for support in insight.supported_by.iter().take(8) {
                    if by_id.contains(support.as_str()) {
                        events.push(link(&m.id, support, LinkKind::DerivedFrom, 1.0));
                    }
                }
                events.push(PendingEvent::Upsert(Box::new(m)));
                written += 1;
            }
            counts.insert("insights".into(), written);
            ctx.commit(events).await?;
        }
        Err(e) => {
            log::debug!("memory: dream reflection call failed: {e}");
            counts.insert("insights".into(), 0);
        }
    }

    Ok(counts)
}

/// `AboutEntity` and `RelatesTo` edges that do not exist yet.
///
/// Bounded per memory: a hub memory linked to everything makes graph expansion
/// return the same twelve things for every query, which is worse than no graph
/// leg at all.
fn mechanical_links(idx: &MemoryIndex, live: &[Memory]) -> Vec<PendingEvent> {
    const MAX_LINKS_PER_MEMORY: usize = 12;
    const MAX_NEW_PER_MEMORY: usize = 10;

    let existing: HashSet<(String, String)> = live
        .iter()
        .flat_map(|m| {
            idx.links_from(&m.id)
                .iter()
                .map(|l| (l.src.clone(), l.dst.clone()))
        })
        .collect();
    let mut added: HashSet<(String, String)> = HashSet::new();
    let mut events = Vec::new();

    // Entities first: an exact canonical-key match is a much stronger claim than
    // any amount of shared vocabulary, and it should not lose its budget to it.
    let mut by_entity: HashMap<String, Vec<&Memory>> = HashMap::new();
    for m in live {
        if let Some(e) = m.entity.as_ref().map(|e| e.trim().to_lowercase())
            && !e.is_empty()
        {
            by_entity.entry(e).or_default().push(m);
        }
    }
    for group in by_entity.values() {
        if group.len() < 2 || group.len() > 40 {
            continue;
        }
        for a in group {
            for b in group {
                if a.id >= b.id {
                    continue;
                }
                let key = (a.id.clone(), b.id.clone());
                if existing.contains(&key) || !added.insert(key) {
                    continue;
                }
                events.push(link(&a.id, &b.id, LinkKind::AboutEntity, 1.0));
            }
        }
    }

    let threshold = relate_similarity();
    let tokens: HashMap<&str, HashSet<String>> = live
        .iter()
        .map(|m| {
            (
                m.id.as_str(),
                super::index::tokenize(&m.indexable()).into_iter().collect(),
            )
        })
        .collect();

    for m in live {
        if idx.links_from(&m.id).len() >= MAX_LINKS_PER_MEMORY {
            continue;
        }
        let mut new_here = 0usize;
        for hit in idx.search(&m.indexable(), 15, None) {
            if new_here >= MAX_NEW_PER_MEMORY {
                break;
            }
            if hit.id == m.id {
                continue;
            }
            let key = if m.id < hit.id {
                (m.id.clone(), hit.id.clone())
            } else {
                (hit.id.clone(), m.id.clone())
            };
            if existing.contains(&key) || added.contains(&key) {
                continue;
            }
            let (Some(a), Some(b)) = (tokens.get(m.id.as_str()), tokens.get(hit.id.as_str()))
            else {
                continue;
            };
            let score = jaccard(a, b);
            if score < threshold {
                continue;
            }
            added.insert(key);
            events.push(link(&m.id, &hit.id, LinkKind::RelatesTo, score));
            new_here += 1;
        }
    }

    events
}

/// Words that mark a memory as a correction of something rather than a fresh fact.
///
/// The signal exists because similarity alone is a terrible contradiction
/// detector: "deploys with Railway" and "deploys with Render" are lexically close
/// and both perfectly true of different services. Requiring a correction marker
/// costs recall of subtle contradictions and buys not asking an LLM about every
/// similar pair on the pod.
const CORRECTION_MARKERS: &[&str] = &[
    "no longer", "not ", "never", "instead", "actually", "changed", "moved",
    "switched", "migrated", "stopped", "deprecated", "replaced", "used to",
    "correction", "wrong", "isn't", "doesn't", "don't", "prefers not",
];

/// Pairs worth asking about: same entity, one of them carrying a correction
/// signal, and the newer one actually newer.
fn contradiction_candidates(live: &[Memory]) -> Vec<(Memory, Memory)> {
    const MAX_CANDIDATES: usize = 10;
    let mut by_entity: HashMap<String, Vec<&Memory>> = HashMap::new();
    for m in live {
        if m.kind == MemoryKind::Episodic {
            continue;
        }
        if let Some(e) = m.entity.as_ref().map(|e| e.trim().to_lowercase())
            && !e.is_empty()
        {
            by_entity.entry(e).or_default().push(m);
        }
    }

    let mut out: Vec<(Memory, Memory)> = Vec::new();
    for group in by_entity.values() {
        for a in group {
            for b in group {
                if a.id == b.id || a.created_at >= b.created_at {
                    continue;
                }
                let newer = b.content.to_lowercase();
                if !CORRECTION_MARKERS.iter().any(|w| newer.contains(w)) {
                    continue;
                }
                let ta: HashSet<String> = super::index::tokenize(&a.content).into_iter().collect();
                let tb: HashSet<String> = super::index::tokenize(&b.content).into_iter().collect();
                if jaccard(&ta, &tb) < 0.25 {
                    continue;
                }
                out.push(((*a).clone(), (*b).clone()));
            }
        }
    }
    out.sort_by_key(|(_, newer)| std::cmp::Reverse(newer.created_at));
    out.truncate(MAX_CANDIDATES);
    out
}

// ── stage 5: decay ───────────────────────────────────────────────────────────

/// Stage 5. Let unused memories fade, archive what has faded, drop what has been
/// archived long enough.
///
/// ```text
/// importance' = importance · 2^(-days_since_access / half_life)
///             + (accessed in the last day ? 1.0 : 0.0)
/// ```
///
/// **There is no age limit anywhere in here**, and that is the point. A
/// preference learned once and never re-read is exactly what an unconditional
/// 30-day prune would delete and exactly what must survive. What protects a
/// memory: being pinned, being a `Preference`/`Procedural`/`Entity`, or being on
/// the receiving end of a `DerivedFrom`/`Supersedes` edge — something else
/// depends on it for provenance.
async fn stage_decay(ctx: &Ctx) -> Result<BTreeMap<String, usize>, String> {
    let mut counts = BTreeMap::new();
    let now = Utc::now();
    let half_life = half_life_days();
    let purge_after = Duration::days(purge_after_days());

    let (events, decayed, archived, purged) = {
        let delta = ctx.mem.delta.read().await;
        let mut events = Vec::new();
        let (mut decayed, mut archived, mut purged) = (0usize, 0usize, 0usize);

        for m in delta.iter() {
            if let Some(at) = m.archived_at {
                if now - at > purge_after {
                    events.push(PendingEvent::Purge(m.id.clone()));
                    purged += 1;
                }
                continue;
            }
            if m.superseded_by.is_some() {
                continue;
            }

            let days = (now - m.last_accessed_at).num_seconds() as f32 / 86_400.0;
            let recent_bonus = if m.access_count > 0 && days < 1.0 { 1.0 } else { 0.0 };
            let decayed_importance =
                (m.importance * 2f32.powf(-days.max(0.0) / half_life) + recent_bonus).clamp(0.0, 10.0);

            let protected = m.pinned
                || m.kind.exempt_from_decay()
                || delta
                    .links_to(&m.id)
                    .iter()
                    .any(|l| l.kind.protects_target());

            if decayed_importance < ARCHIVE_BELOW && !protected {
                events.push(PendingEvent::Archive(m.id.clone()));
                archived += 1;
                continue;
            }
            // Only write back a change worth a log line. Every memory drifting by
            // a thousandth every night would turn the log into a decay journal.
            if (decayed_importance - m.importance).abs() > 0.05 {
                let mut updated = m.clone();
                updated.importance = decayed_importance;
                updated.updated_at = now;
                events.push(PendingEvent::Upsert(Box::new(updated)));
                decayed += 1;
            }
        }
        (events, decayed, archived, purged)
    };

    ctx.commit(events).await?;
    counts.insert("decayed".into(), decayed);
    counts.insert("archived".into(), archived);
    counts.insert("purged".into(), purged);
    Ok(counts)
}

// ── log compaction ───────────────────────────────────────────────────────────

/// Fold the log into a fresh snapshot and truncate it.
///
/// Snapshot first, then truncate — a crash between the two replays already-folded
/// events, which is harmless because every event is idempotent under
/// `MemoryIndex::apply`. The reverse order would lose the night's work.
async fn compact_log(ctx: &Ctx) -> bool {
    let dir = crate::paths::memory_instance_dir(&ctx.instance_id);
    let snapshot = { ctx.mem.delta.read().await.snapshot() };
    if let Err(e) = super::wal::write_snapshot(&dir.join("snapshot.json"), &snapshot) {
        log::warn!("memory: dream could not snapshot '{}': {e}", ctx.instance_id);
        return false;
    }
    if let Err(e) = super::wal::truncate(&dir.join("wal.jsonl")) {
        log::warn!(
            "memory: dream snapshotted '{}' but could not truncate its log: {e}",
            ctx.instance_id
        );
        // The snapshot is good; the log will simply replay events already in it.
        return true;
    }
    true
}

// ── the journal ──────────────────────────────────────────────────────────────

fn write_journal(instance_id: &str, report: &DreamReport) {
    let dir = crate::paths::memory_dreams_dir(instance_id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::debug!("memory: could not create the dream journal directory: {e}");
        return;
    }
    let name = format!("{}.json", report.started_at.format("%Y%m%dT%H%M%SZ"));
    match serde_json::to_string_pretty(report) {
        Ok(json) => {
            if let Err(e) = std::fs::write(dir.join(&name), json) {
                log::debug!("memory: could not write the dream journal: {e}");
                return;
            }
        }
        Err(e) => {
            log::debug!("memory: could not serialize the dream journal: {e}");
            return;
        }
    }
    prune_journals(&dir);
}

/// Keep the most recent [`JOURNALS_KEPT`] runs. Names sort chronologically
/// because they are timestamps, so this is a sort and a truncate.
fn prune_journals(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    if files.len() <= JOURNALS_KEPT {
        return;
    }
    files.sort();
    let excess = files.len() - JOURNALS_KEPT;
    for path in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// The most recent run for one agent, newest first. Read by `mem_stats` and the
/// instance memory view, so "when did this agent last consolidate?" is answerable
/// without shelling into the pod.
pub fn last_journal(instance_id: &str) -> Option<DreamReport> {
    let dir = crate::paths::memory_dreams_dir(instance_id);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let newest = files.pop()?;
    let raw = std::fs::read_to_string(newest).ok()?;
    serde_json::from_str(&raw).ok()
}

// ── talking to the model ─────────────────────────────────────────────────────

/// One plain completion, no tools, no agent loop.
///
/// Deliberately not `runtime::run_one_shot_task`: that builds a persona, a tool
/// registry, a step guard and a turn-capture context, and the last of those would
/// have the dream writing captures into the very queue it is draining.
async fn ask(model_name: &str, system: &str, prompt: &str) -> Result<String, String> {
    use rig::client::CompletionClient;
    use rig::completion::Chat;

    let key = crate::runtime::inference_api_key()
        .ok_or("no inference credential; the dream's thinking stages cannot run")?;
    let client = crate::runtime::build_openai_client(&key).map_err(|e| e.to_string())?;
    let agent = rig::agent::AgentBuilder::new(client.completion_model(model_name))
        .preamble(system)
        .build();
    agent
        .chat(prompt, &mut Vec::<rig::completion::Message>::new())
        .await
        .map_err(|e| format!("dream LLM call failed: {e}"))
}

/// Ask for JSON and get it back typed.
///
/// Models fence JSON in markdown about half the time regardless of instruction,
/// so the fence is stripped rather than treated as a failure — refusing a
/// perfectly good answer over three backticks would make the dream flaky for no
/// reason.
async fn ask_json<T: serde::de::DeserializeOwned>(
    model_name: &str,
    system: &str,
    prompt: &str,
) -> Result<T, String> {
    let raw = ask(model_name, system, prompt).await?;
    let text = strip_fence(&raw);
    serde_json::from_str::<T>(text)
        .map_err(|e| format!("dream answer was not the expected JSON ({e}): {}", brief(text)))
}

fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    // ```json\n… or ```\n…
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches('\n')
        .trim_end()
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim()
}

fn brief(s: &str) -> String {
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    format!("{}…", s.chars().take(200).collect::<String>())
}

// ── prompts and answer shapes ────────────────────────────────────────────────

const MERGE_SYSTEM: &str = "\
You consolidate an AI agent's long-term memory. You are shown several stored memories that may \
say overlapping things. Decide whether they are genuinely the same knowledge stated more than \
once, or distinct things that merely sound alike.

Reply with JSON only, no prose, no markdown fence:
{\"merge\": true, \"content\": \"the single merged memory\", \"summary\": \"one short line\", \"entity\": \"canonical key or null\"}
or
{\"merge\": false}

Rules:
- Prefer {\"merge\": false}. Two similar preferences are usually two real preferences.
- Merge only if a reader would be surprised to find both stored separately.
- The merged content must preserve every specific detail from the originals: names, versions, \
paths, numbers. Losing a detail is worse than not merging.
- Write it as a standalone statement, not as a comparison of the originals.";

#[derive(Deserialize)]
struct MergeAnswer {
    #[serde(default)]
    merge: bool,
    #[serde(default)]
    content: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    entity: Option<String>,
}

fn merge_prompt(cluster: &[Memory]) -> String {
    let mut out = String::from("These stored memories may overlap:\n\n");
    for (i, m) in cluster.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] {}\n",
            i + 1,
            m.kind.as_str(),
            m.content.trim()
        ));
    }
    out.push_str("\nMerge them, or say they should stay separate.");
    out
}

const ABSTRACT_SYSTEM: &str = "\
You distil an AI agent's conversation into long-term memory. You are given one conversation \
transcript. Produce a short summary of it, plus any durable knowledge worth remembering months \
from now.

Reply with JSON only, no prose, no markdown fence:
{\"title\": \"short title\",
 \"summary\": \"2-4 sentences on what happened and what was decided\",
 \"memories\": [{\"kind\": \"semantic|preference|procedural|entity\", \"content\": \"...\", \"entity\": \"canonical key or null\", \"importance\": 1-10}]}

Kinds:
- semantic: a durable fact about the world or the user's setup.
- preference: how the user wants things done.
- procedural: a method that worked here, grounded in the tools actually used.
- entity: a person, repo, service or other named thing, with a canonical key.

Rules:
- Extract NOTHING that is only true today. \"the build is failing\" is not a memory; \
\"the test suite needs LC_ALL=C on macOS\" is.
- Prefer zero extractions to speculative ones. An empty \"memories\" list is a good answer and \
the common one.
- Never extract secrets, tokens, keys or credentials.
- Each memory must stand alone: a reader with no access to this conversation must understand it.";

#[derive(Deserialize)]
struct AbstractAnswer {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    memories: Vec<AbstractItem>,
}

#[derive(Deserialize)]
struct AbstractItem {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    importance: Option<f32>,
}

const CONTRADICTION_SYSTEM: &str = "\
You audit an AI agent's long-term memory for stale beliefs. You are given pairs of stored \
memories about the same thing, an older one and a newer one, where the newer one looks like it \
might correct the older.

Reply with JSON only, no prose, no markdown fence:
{\"verdicts\": [{\"older_id\": \"...\", \"newer_id\": \"...\", \"relation\": \"supersedes|contradicts|unrelated\"}]}

- supersedes: the newer is a clean update of the older. The older stops being true.
- contradicts: they conflict and it is not clear which is right.
- unrelated: they coexist fine. This is the default and usually the right answer.

Return one verdict per pair, in the order given.";

#[derive(Deserialize)]
struct ContradictionAnswer {
    #[serde(default)]
    verdicts: Vec<ContradictionVerdict>,
}

#[derive(Deserialize)]
struct ContradictionVerdict {
    #[serde(default)]
    older_id: String,
    #[serde(default)]
    newer_id: String,
    #[serde(default)]
    relation: String,
}

fn contradiction_prompt(pairs: &[(Memory, Memory)]) -> String {
    let mut out = String::from("Pairs to adjudicate:\n\n");
    for (older, newer) in pairs {
        out.push_str(&format!(
            "- older_id={} ({}): {}\n  newer_id={} ({}): {}\n\n",
            older.id,
            older.created_at.format("%Y-%m-%d"),
            older.content.trim(),
            newer.id,
            newer.created_at.format("%Y-%m-%d"),
            newer.content.trim(),
        ));
    }
    out
}

const REFLECT_SYSTEM: &str = "\
You look for patterns across an AI agent's long-term memory. Given a set of individual memories, \
name at most three patterns that no single memory states but that all of them together suggest.

Reply with JSON only, no prose, no markdown fence:
{\"insights\": [{\"content\": \"...\", \"supported_by\": [\"id\", \"id\"], \"importance\": 1-10}]}

Rules:
- At most three. Fewer is better. An empty list is a good answer.
- An insight must be supported by at least two of the given memories, cited by id.
- Do not restate a single memory in different words. If it is already stored, it is not an insight.
- Be concrete about this user and their work. Generic observations about software are worthless here.";

#[derive(Deserialize)]
struct ReflectAnswer {
    #[serde(default)]
    insights: Vec<ReflectItem>,
}

#[derive(Deserialize)]
struct ReflectItem {
    #[serde(default)]
    content: String,
    #[serde(default)]
    supported_by: Vec<String>,
    #[serde(default)]
    importance: Option<f32>,
}

fn reflect_prompt(memories: &[&Memory]) -> String {
    let mut out = String::from("Memories:\n\n");
    for m in memories {
        out.push_str(&format!(
            "- id={} [{}] {}\n",
            m.id,
            m.kind.as_str(),
            m.display_text().trim()
        ));
    }
    out.push_str("\nWhat pattern do these suggest that no single one states?");
    out
}

// ── the nightly loop ─────────────────────────────────────────────────────────

/// The schedule's bookmark and what the last sweep did, across restarts.
///
/// Pod-global, like the schedule itself. Persisted for the reason
/// `schedule_timing`'s bookmarks are: a bookmark held only in RAM makes every
/// pod roll look like a first sighting, and a pod rolls on every image upgrade.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DreamState {
    /// The cron occurrence the sweep last reached.
    #[serde(default)]
    bookmark: Option<DateTime<Utc>>,
    /// When a sweep last actually ran, for the boot catch-up check.
    #[serde(default)]
    last_run: Option<DateTime<Utc>>,
    /// How many agents the last sweep dreamt for.
    #[serde(default)]
    last_instances: usize,
}

impl DreamState {
    fn load() -> Self {
        std::fs::read_to_string(crate::paths::memory_dream_state_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Failure is logged, not propagated: losing this file costs one seeded
    /// interval, and refusing to dream costs the night.
    fn save(&self) {
        let path = crate::paths::memory_dream_state_file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    log::warn!("memory: could not write dream state to {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("memory: could not serialize dream state: {e}"),
        }
    }
}

/// How stale the last run may be before boot treats the night as missed.
///
/// Longer than a day so a pod that restarts at 3:29am does not immediately dream
/// again for the night it is about to dream for anyway.
const CATCHUP_AFTER_HOURS: i64 = 36;

/// How often the loop wakes to ask whether the cron is due.
const TICK_SECONDS: u64 = 60;

/// The nightly dream loop. Spawned once from the daemon and never returns.
///
/// Its own loop rather than a flow or a scheduled task, and that is a decision:
/// `scheduled_tasks` is one-shot and capped at 24h, and a user-deletable flow can
/// be switched off — but the mechanical stages (draining captures, compacting the
/// log, decay) have to run unconditionally or recall silently rots. Turning off
/// `MEMORY_DREAM` is how you opt out; deleting something by accident is not.
pub async fn dream_loop() {
    if !nightly_enabled() {
        log::info!("memory: the nightly dream is off (MEMORY_DREAM / MEMORY_ENABLED)");
        return;
    }
    let expr = cron_expr();
    let tz = timezone();
    log::info!(
        "memory: nightly dream armed — cron '{expr}' in {}, model {}, agents active within {} day(s)",
        tz.as_deref().unwrap_or("the pod's clock"),
        model_name(),
        active_days()
    );

    // A night missed while the pod was down is made up once, on boot. Without
    // this a pod that restarts every evening never dreams at all.
    let state = DreamState::load();
    if let Some(last) = state.last_run
        && Utc::now() - last > Duration::hours(CATCHUP_AFTER_HOURS)
    {
        log::info!(
            "memory: last dream was {} hour(s) ago; catching up",
            (Utc::now() - last).num_hours()
        );
        sweep(Trigger::Catchup).await;
    }

    let schedule = crate::flows::FlowSchedule::Cron(expr);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(TICK_SECONDS)).await;
        let mut state = DreamState::load();
        let Some(decision) = crate::schedule_timing::decide(
            state.bookmark,
            &schedule,
            tz.as_deref(),
            Utc::now(),
        ) else {
            continue;
        };
        // Bookmark the occurrence even when it only seeds, or the schedule keeps
        // re-deciding the same starting point and never reaches a second one.
        state.bookmark = Some(decision.occurrence);
        if !decision.run {
            state.save();
            continue;
        }
        let dreamt = sweep(Trigger::Nightly).await;
        state.last_run = Some(Utc::now());
        state.last_instances = dreamt;
        state.save();
    }
}

/// Dream for every agent that has been used lately. Returns how many ran.
///
/// Sequential on purpose: a sweep is several LLM calls per agent, and running a
/// fleet of them at once would spike a pod's inference spend into whatever rate
/// limit it has at 3am, for work that nobody is waiting on.
async fn sweep(trigger: Trigger) -> usize {
    let instances = active_instances();
    if instances.is_empty() {
        log::info!("memory: nothing to dream for — no agent has been active recently");
        return 0;
    }
    log::info!(
        "memory: dreaming for {} agent(s): {}",
        instances.len(),
        instances.join(", ")
    );
    for id in &instances {
        let report = dream(id, trigger).await;
        if let Some(e) = &report.error {
            log::warn!("memory: dream for '{id}' failed: {e}");
        }
    }
    instances.len()
}

/// Which agents tonight's sweep covers.
///
/// **Active means used in the last `MEMORY_DREAM_ACTIVE_DAYS` days** — a fleet
/// accumulates agents that were made once and abandoned, and paying for LLM
/// stages on all of them nightly is the fastest way to make this feature
/// something people turn off.
///
/// With one exception: an agent that still has **undistilled captures** is
/// included whether or not it is active. Otherwise material written the day
/// before an agent went quiet sits in the queue forever, and the queue is the one
/// thing in this system that only grows. The exception is self-limiting — the run
/// drains the queue, so the agent stops qualifying after one night.
fn active_instances() -> Vec<String> {
    let cutoff = Utc::now() - Duration::days(active_days());
    crate::agent_instance::list()
        .into_iter()
        .filter(|inst| {
            let recent = DateTime::parse_from_rfc3339(&inst.last_active_at)
                .map(|t| t.with_timezone(&Utc) >= cutoff)
                .unwrap_or(false);
            recent || super::capture::pending_count(&inst.id) > 0
        })
        .map(|inst| inst.id)
        .collect()
}

/// When the next nightly dream is due, for display. `None` when the schedule is
/// off or the cron does not parse.
pub fn next_run_at() -> Option<DateTime<Utc>> {
    use std::str::FromStr;
    if !nightly_enabled() {
        return None;
    }
    let schedule = cron::Schedule::from_str(&cron_expr()).ok()?;
    match timezone().and_then(|name| name.parse::<chrono_tz::Tz>().ok()) {
        Some(zone) => schedule
            .after(&Utc::now().with_timezone(&zone))
            .next()
            .map(|t| t.with_timezone(&Utc)),
        None => schedule.upcoming(Utc).next(),
    }
}

/// When the last sweep ran, for display.
pub fn last_run_at() -> Option<DateTime<Utc>> {
    DreamState::load().last_run
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(chat: &str, at: DateTime<Utc>, user: &str, agent: &str) -> Capture {
        Capture {
            id: uuid::Uuid::new_v4().to_string(),
            kind: CaptureKind::Turn,
            at,
            chat_id: Some(chat.to_string()),
            instance_id: Some("inst".into()),
            persona: Some("orchestrator-agent".into()),
            user_text: user.into(),
            agent_text: agent.into(),
            tools: vec!["bash".into()],
            processed_at: None,
        }
    }

    fn marker(chat: &str, at: DateTime<Utc>) -> Capture {
        Capture {
            id: uuid::Uuid::new_v4().to_string(),
            kind: CaptureKind::SessionEnd,
            at,
            chat_id: Some(chat.to_string()),
            instance_id: Some("inst".into()),
            persona: None,
            user_text: String::new(),
            agent_text: String::new(),
            tools: vec![],
            processed_at: None,
        }
    }

    fn t(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-29T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(minutes)
    }

    #[test]
    fn a_quiet_conversation_becomes_one_episode() {
        let caps = vec![
            capture("c1", t(0), "how do I deploy?", "run ./start-agent.sh"),
            capture("c1", t(5), "and roll back?", "redeploy the previous tag"),
        ];
        let (eps, open) = group_episodes(&caps, t(1000));
        assert_eq!(open, 0);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].turns, 2);
        assert!(eps[0].transcript.contains("start-agent.sh"));
    }

    #[test]
    fn a_conversation_still_inside_the_idle_window_is_left_alone() {
        // The whole point: distilling a conversation someone is still having
        // produces a memory of half a thought.
        let caps = vec![capture("c1", t(0), "hi", "hello")];
        let (eps, open) = group_episodes(&caps, t(10));
        assert!(eps.is_empty(), "too recent to close");
        assert_eq!(open, 1);
    }

    #[test]
    fn a_long_silence_splits_one_chat_into_two_episodes() {
        let caps = vec![
            capture("c1", t(0), "morning question", "morning answer"),
            capture("c1", t(600), "evening question", "evening answer"),
        ];
        let (eps, _) = group_episodes(&caps, t(2000));
        assert_eq!(eps.len(), 2, "a 10-hour gap is two conversations");
    }

    #[test]
    fn a_session_end_marker_closes_an_episode_immediately() {
        // Without the marker this would still be inside the idle window and left
        // open; the marker is the system saying "that conversation is over".
        let caps = vec![
            capture("c1", t(0), "q", "a"),
            marker("c1", t(1)),
        ];
        let (eps, open) = group_episodes(&caps, t(2));
        assert_eq!(eps.len(), 1, "the marker closed it early");
        assert_eq!(open, 0);
    }

    #[test]
    fn separate_chats_never_merge_into_one_episode() {
        let caps = vec![
            capture("c1", t(0), "about the gateway", "ok"),
            capture("c2", t(1), "about the calendar", "ok"),
        ];
        let (eps, _) = group_episodes(&caps, t(1000));
        assert_eq!(eps.len(), 2);
    }

    #[test]
    fn a_bare_marker_produces_no_episode() {
        // A conversation whose turns were distilled last night still writes its
        // end marker. There is nothing left to distil.
        let caps = vec![marker("c1", t(0))];
        let (eps, open) = group_episodes(&caps, t(1000));
        assert!(eps.is_empty());
        assert_eq!(open, 0);
    }

    #[test]
    fn every_capture_in_a_closed_episode_is_marked_processed() {
        // The marker's own id has to be in the list, or it is re-read forever and
        // the queue never drains.
        let caps = vec![capture("c1", t(0), "q", "a"), marker("c1", t(1))];
        let (eps, _) = group_episodes(&caps, t(1000));
        assert_eq!(eps[0].capture_ids.len(), 2);
    }

    #[test]
    fn an_oversized_transcript_is_truncated_on_a_char_boundary() {
        let mut ep = Episode {
            id: "e".into(),
            chat_id: None,
            persona: None,
            started_at: t(0),
            ended_at: t(1),
            capture_ids: vec![],
            turns: 1,
            tools: vec![],
            transcript: "é".repeat(30_000),
        };
        let content = episode_content(&ep);
        assert!(content.ends_with("[…transcript truncated]"));
        ep.transcript = "short".into();
        assert!(!episode_content(&ep).contains("truncated"));
    }

    #[test]
    fn jaccard_is_symmetric_and_bounded() {
        let a: HashSet<String> = ["deploy", "railway", "agent"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["deploy", "railway", "pod"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard(&a, &b) - jaccard(&b, &a)).abs() < f32::EPSILON);
        assert_eq!(jaccard(&a, &a), 1.0);
        assert_eq!(jaccard(&a, &HashSet::new()), 0.0);
    }

    #[test]
    fn clusters_group_rephrasings_and_leave_distinct_memories_alone() {
        let mut idx = MemoryIndex::new();
        for (i, content) in [
            "the user prefers rust edition 2024 for every new crate",
            "for new crates the user prefers rust edition 2024",
            "the pod deploys to railway on a push to main",
        ]
        .iter()
        .enumerate()
        {
            let mut m = Memory::new(MemoryKind::Preference, *content, Source::Tool);
            m.id = format!("m{i}");
            idx.insert_memory(m);
        }
        let clusters = find_clusters(&idx, 0.5, 10);
        assert_eq!(clusters.len(), 1, "one cluster, not one per pair");
        assert_eq!(clusters[0].len(), 2);
        let ids: Vec<&str> = clusters[0].iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"m0") && ids.contains(&"m1"));
    }

    #[test]
    fn episodic_memories_are_never_merge_candidates() {
        // Two conversations covering the same ground are still two conversations,
        // and merging them would orphan the provenance links stage 3 writes.
        let mut idx = MemoryIndex::new();
        for i in 0..2 {
            let mut m = Memory::new(
                MemoryKind::Episodic,
                "conversation about deploying the gateway to railway",
                Source::Turn,
            );
            m.id = format!("e{i}");
            m.content_hash = format!("h{i}");
            idx.insert_memory(m);
        }
        assert!(find_clusters(&idx, 0.5, 10).is_empty());
    }

    #[test]
    fn contradiction_candidates_need_a_correction_signal() {
        let entity = Some("metalcraft-gateway".to_string());
        let mut old = Memory::new(MemoryKind::Semantic, "the gateway deploys to railway", Source::Tool);
        old.entity = entity.clone();
        old.created_at = t(0);

        // Similar, same entity, but no correction marker: two facts, not a conflict.
        let mut sibling = Memory::new(
            MemoryKind::Semantic,
            "the gateway deploys to railway with a redis queue",
            Source::Tool,
        );
        sibling.entity = entity.clone();
        sibling.created_at = t(10);
        assert!(contradiction_candidates(&[old.clone(), sibling]).is_empty());

        // Same shape, plus "no longer": worth asking about.
        let mut correction = Memory::new(
            MemoryKind::Semantic,
            "the gateway no longer deploys to railway",
            Source::Tool,
        );
        correction.entity = entity;
        correction.created_at = t(10);
        let pairs = contradiction_candidates(&[old, correction]);
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].1.content.contains("no longer"), "newer is the correction");
    }

    #[test]
    fn mechanical_links_connect_shared_entities_without_duplicating() {
        let mut idx = MemoryIndex::new();
        let mut live = Vec::new();
        for i in 0..3 {
            let mut m = Memory::new(
                MemoryKind::Semantic,
                format!("fact number {i} about the gateway service"),
                Source::Tool,
            );
            m.id = format!("m{i}");
            m.entity = Some("metalcraft-gateway".into());
            idx.insert_memory(m.clone());
            live.push(m);
        }
        let first = mechanical_links(&idx, &live);
        let entity_edges = first
            .iter()
            .filter(|e| matches!(e, PendingEvent::Link(l) if l.kind == LinkKind::AboutEntity))
            .count();
        assert_eq!(entity_edges, 3, "three memories, three undirected pairs");

        // Apply them, then ask again: nothing new to add.
        for e in first {
            if let PendingEvent::Link(l) = e {
                idx.insert_link(l);
            }
        }
        let second = mechanical_links(&idx, &live);
        let repeats = second
            .iter()
            .filter(|e| matches!(e, PendingEvent::Link(l) if l.kind == LinkKind::AboutEntity))
            .count();
        assert_eq!(repeats, 0, "links already present must not be rewritten");
    }

    #[test]
    fn stages_can_be_narrowed_by_env_but_default_to_all_five() {
        // Parsing only; the env var itself is process-global and read at run time.
        assert_eq!(configured_stages(), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn dream_state_round_trips_and_an_absent_file_is_a_fresh_schedule() {
        let state = DreamState {
            bookmark: Some(t(0)),
            last_run: Some(t(5)),
            last_instances: 3,
        };
        let back: DreamState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(back.bookmark, Some(t(0)));
        assert_eq!(back.last_instances, 3);
        // A pod that has never dreamt reads as "no bookmark", which seeds rather
        // than fires — the same rule flow schedules follow.
        let fresh: DreamState = serde_json::from_str("{}").unwrap();
        assert!(fresh.bookmark.is_none() && fresh.last_run.is_none());
    }

    #[test]
    fn the_default_cron_is_a_six_field_expression_that_parses() {
        // Five-field POSIX crons are silently never due (see `schedule_timing`),
        // so the default shipping broken would mean a dream that never runs and
        // never says why.
        use std::str::FromStr;
        assert!(cron::Schedule::from_str(&cron_expr()).is_ok());
        assert_eq!(cron_expr().split_whitespace().count(), 6);
    }

    #[test]
    fn fenced_json_is_unwrapped() {
        assert_eq!(strip_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fence("  {\"a\":1}  "), "{\"a\":1}");
    }

    #[test]
    fn a_report_headline_reads_as_a_sentence() {
        let mut counts = BTreeMap::new();
        counts.insert("episodes_distilled".to_string(), 2usize);
        counts.insert("memories_extracted".to_string(), 5usize);
        let now = Utc::now();
        let report = DreamReport {
            instance_id: "inst".into(),
            trigger: "nightly".into(),
            model: "gpt-5.4".into(),
            started_at: now,
            finished_at: now,
            stages: vec![StageReport {
                stage: 3,
                name: "abstract".into(),
                ran: true,
                counts,
                error: None,
                millis: 10,
            }],
            memories_before: 10,
            memories_after: 15,
            captures_pending_before: 4,
            captures_pending_after: 0,
            snapshot_written: true,
            error: None,
        };
        let line = report.headline();
        assert!(line.contains("2 episode(s)"), "{line}");
        assert!(line.contains("5 thing(s)"), "{line}");
    }
}
