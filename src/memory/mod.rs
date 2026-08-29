//! Persistent, cross-session memory for the agent.
//!
//! See `docs/MEMORY_SYSTEM_PLAN.md` for the design: the store, the keyword index,
//! the explicit `mem_*` tools, automatic injection into every turn, automatic
//! capture of raw turn material, and the nightly [`dream`] that turns that
//! material into memory.
//!
//! Shape, and why:
//!
//! * **Memory belongs to an agent, not to the pod.** There is no pod-global
//!   store. Every entry point here takes an `instance_id`, because a memory with
//!   no agent to own it has no one it is true *about* — the CLI, which has no
//!   agent, simply has no memory. What an agent knows is a shared immutable
//!   **base** shipped by its preset plus the **delta** it learns, and both live
//!   under `<data>/memory/instances/<id>/`. See [`instance`].
//! * **The store is RAM; the disk is a log.** [`index::MemoryIndex`] is
//!   authoritative; [`wal`] is how it survives a restart. Writes append one line
//!   (O(1)) instead of rewriting a JSON array (O(n)) the way `scheduled_tasks`
//!   does — because recall touches access times on *every* turn, and that write
//!   pattern would not survive being O(n).
//! * **Recall is keyword + graph.** There are no embeddings. The vector leg was
//!   removed rather than kept as decoration: it only ever ran against the
//!   pod-global store, so on a real pod — where every turn is instance-scoped —
//!   it had never contributed a single result.
//! * **Cheap to write, expensive to sleep.** A turn appends one line to
//!   `capture.jsonl` and pays nothing else — no LLM call at interactive latency.
//!   The nightly [`dream`] is where distillation, merging, linking and pruning
//!   happen. This is the central design bet, and [`capture`] is only half of it:
//!   without the dream draining the queue, that file would just grow.
//! * **Nothing here can fail a turn.** Every public entry point returns a
//!   `Result` the caller is expected to discard on failure, and initialization
//!   degrades to an empty store rather than panicking.
//!
//! The module is named `wal` rather than `log` on purpose: `pub mod log` inside
//! this file would shadow the `log` logging facade for every sibling.
pub mod capture;
pub mod dream;
pub mod index;
pub mod inject;
pub mod instance;
pub mod recall;
pub mod redact;
pub mod tools;
pub mod types;
pub mod wal;

use chrono::{DateTime, Utc};

use index::MemoryIndex;
use recall::{RecallOptions, Scored};
use types::{Event, Memory, MemoryKind, Source};

/// Whether the memory subsystem is on. `MEMORY_ENABLED=0|false|off` disables it;
/// anything else (including unset) leaves it enabled.
pub fn enabled() -> bool {
    match std::env::var("MEMORY_ENABLED") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

// ── writing ──────────────────────────────────────────────────────────────────

/// What a caller wants remembered. Only `kind` and `content` are required.
#[derive(Debug, Clone)]
pub struct RememberRequest {
    pub kind: MemoryKind,
    pub content: String,
    pub summary: Option<String>,
    pub entity: Option<String>,
    pub importance: Option<f32>,
    pub pinned: bool,
    pub source: Source,
    pub chat_id: Option<String>,
    pub persona: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
}

impl RememberRequest {
    pub fn new(kind: MemoryKind, content: impl Into<String>, source: Source) -> Self {
        Self {
            kind,
            content: content.into(),
            summary: None,
            entity: None,
            importance: None,
            pinned: false,
            source,
            chat_id: None,
            persona: None,
            occurred_at: None,
        }
    }
}

/// The outcome of a write — distinguishing a new memory from a deduplicated one,
/// so a caller (and the agent) can tell "saved" from "already knew that".
#[derive(Debug, Clone)]
pub struct Remembered {
    pub memory: Memory,
    /// True when an identical memory already existed and was reinforced instead.
    pub deduplicated: bool,
    /// How many secrets were scrubbed from the content before storing.
    pub redactions: usize,
}

/// Write a memory into one agent's delta.
///
/// Content is scrubbed for secrets first (always — this is not optional, see
/// [`redact`]). An exact duplicate is *reinforced* rather than stored twice:
/// importance takes the max of old and new and the access clock is bumped, which
/// is what "the user told me this again" should mean. A memory the agent's preset
/// already *shipped* also dedupes, so it is not learned a second time.
pub async fn remember(instance_id: &str, req: RememberRequest) -> Result<Remembered, String> {
    if !enabled() {
        return Err("memory is disabled (MEMORY_ENABLED)".into());
    }
    if req.content.trim().is_empty() {
        return Err("content is required".into());
    }

    let scrubbed = redact::redact(&req.content);
    if scrubbed.count > 0 {
        log::info!(
            "memory: redacted {} secret(s) ({}) before storing",
            scrubbed.count,
            scrubbed.kinds.join(", ")
        );
    }

    remember_into_instance(instance_id, req, scrubbed).await
}

/// Append to an instance's log, then apply. Log-before-apply is deliberate: if the
/// append fails, in-memory state must not diverge from disk, so the caller sees the
/// error and the store is unchanged.
fn commit_to(
    path: &std::path::Path,
    idx: &mut MemoryIndex,
    event: Event,
    durable: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let write = if durable {
        wal::append_durable
    } else {
        wal::append
    };
    write(path, &event).map_err(|e| format!("failed to write memory log: {e}"))?;
    idx.apply(event);
    Ok(())
}

// ── reading ──────────────────────────────────────────────────────────────────

/// Recall across one agent's layers: keyword + graph, fused.
///
/// Degrades to the delta alone if the base cannot be loaded — a missing base is a
/// smaller agent, never a failed turn.
pub async fn recall(instance_id: &str, query: &str, opts: RecallOptions) -> Vec<Scored> {
    if !enabled() || query.trim().is_empty() {
        return Vec::new();
    }
    recall_for_instance(instance_id, query, &opts).await
}

/// One memory from an agent's own layers.
pub async fn get(instance_id: &str, id: &str) -> Option<Memory> {
    instance_get(instance_id, id).await
}

/// Forget inside one agent's memory.
pub async fn forget(instance_id: &str, id: &str) -> Result<instance::Forgotten, String> {
    instance_forget(instance_id, id).await
}

// ── turn integration ─────────────────────────────────────────────────────────

/// Whether per-turn recall injection is on (`MEMORY_RECALL`). The profile block
/// is governed separately, by [`enabled`] alone, because it is cheap and stable.
pub fn recall_enabled() -> bool {
    if !enabled() {
        return false;
    }
    match std::env::var("MEMORY_RECALL") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// Token budget for the per-turn recall block (`MEMORY_RECALL_TOKENS`).
pub fn recall_token_budget() -> usize {
    std::env::var("MEMORY_RECALL_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1200)
}

/// Token budget for the system-prompt profile block (`MEMORY_PROFILE_TOKENS`).
pub fn profile_token_budget() -> usize {
    std::env::var("MEMORY_PROFILE_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(600)
}

/// The stable "what you know about this user" block for the system prompt.
///
/// Deliberately *not* a search: it is the slow-moving core — pinned memories
/// first, then preferences and working methods by importance — so it stays
/// identical across turns and therefore inside the provider's cached prompt
/// prefix. Query-dependent recall goes in the message tail instead
/// ([`inject`]).
///
/// Scoped to one agent, across both its layers: what its preset shipped and what
/// it has since learned, minus anything it has forgotten. This block used to read
/// a pod-global store no instance ever wrote to, which meant it was empty on every
/// pod where a turn actually runs.
///
/// Returns an empty string when memory is off or there is nothing durable yet,
/// which suppresses the whole section rather than printing an empty heading.
pub async fn profile_block(instance_id: &str) -> String {
    if !enabled() {
        return String::new();
    }
    let mem = match handle_for_instance(instance_id) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("memory: profile for instance '{instance_id}' unavailable: {e}");
            return String::new();
        }
    };

    let durable = |m: &Memory| {
        m.is_live() && (m.pinned || matches!(m.kind, MemoryKind::Preference | MemoryKind::Procedural))
    };

    let tombstones = mem.tombstones.read().await.clone();
    let delta = mem.delta.read().await;
    let mut candidates: Vec<Memory> = delta.iter().filter(|m| durable(m)).cloned().collect();
    // A learned memory shadows a shipped one with the same content, so the agent
    // does not read its own correction and the thing it corrected.
    let learned: std::collections::HashSet<String> =
        delta.iter().map(|m| types::content_hash(&m.content)).collect();
    if let Some(base) = &mem.base {
        let base = base.read().await;
        candidates.extend(
            base.iter()
                .filter(|m| durable(m))
                .filter(|m| !tombstones.contains(&m.id))
                .filter(|m| !learned.contains(&types::content_hash(&m.content)))
                .cloned(),
        );
    }

    // Pinned first, then by importance, then newest — a stable total order, so
    // the block does not reshuffle between turns and bust the prompt cache.
    candidates.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| {
                b.importance
                    .partial_cmp(&a.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });

    let budget = profile_token_budget();
    let mut out = String::new();
    let mut spent = 0usize;
    for m in candidates {
        let text = m.display_text().replace('\n', " ");
        let line = format!("- {} ({})\n", text, m.kind.as_str());
        let cost = recall::estimate_tokens(&line);
        if spent + cost > budget {
            break;
        }
        spent += cost;
        out.push_str(&line);
    }
    out
}

// ── one agent's layers ───────────────────────────────────────────────────────

/// Resolve the preset base an instance should read, from its record.
///
/// Returns `None` when the instance has no preset version or its pack shipped no
/// memories — both ordinary, and both mean "this agent only knows what it learns".
/// Which shipped knowledge base this agent reads from.
///
/// **The installed version wins, not the one the agent was born against.** Personas
/// and skills already follow the installed pack — they resolve straight off its
/// directory — so pinning memory to the birth version made an agent that upgraded
/// half-follow: it used the new prompts and the old facts. An author correcting a
/// seed memory in v1.5.0 would never reach an agent created on v1.4.0, which is the
/// whole reason to ship a correction.
///
/// `created_from_version` stays as the diagnostic it is documented to be, and is the
/// fallback when the preset no longer resolves — an agent whose pack was uninstalled
/// keeps whatever base it still has rather than losing it twice over.
fn base_for_instance(instance_id: &str) -> Option<(String, String)> {
    let inst = crate::agent_instance::load(instance_id).ok()?;
    let version = crate::memory::instance::current_base_version(&inst.agent_preset)
        .or_else(|| inst.created_from_version.clone())?;
    Some((inst.agent_preset, version))
}

/// Get one agent's memory handle, resolving which preset base it should read.
///
/// The three lines this replaces appeared at every entry point below, and every
/// one of them had to remember that the base is looked up by the *installed*
/// pack version rather than the one the instance was born against — see
/// [`base_for_instance`]. One place to get that wrong is enough.
pub(crate) fn handle_for_instance(instance_id: &str) -> Result<instance::InstanceMemory, String> {
    let base_ref = base_for_instance(instance_id);
    let base_arg = base_ref.as_ref().map(|(p, v)| (p.as_str(), v.as_str()));
    instance::handle_for(instance_id, base_arg)
}

/// Recall across one agent's layers. Degrades to the delta alone if the base can't
/// be loaded — a missing base is a smaller agent, never a failed turn.
async fn recall_for_instance(
    instance_id: &str,
    query: &str,
    opts: &RecallOptions,
) -> Vec<Scored> {
    let mem = match handle_for_instance(instance_id) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("memory: recall for instance '{instance_id}' unavailable: {e}");
            return Vec::new();
        }
    };

    let tombstones = mem.tombstones.read().await.clone();
    let delta = mem.delta.read().await;
    match &mem.base {
        Some(base) => {
            let base = base.read().await;
            recall::search_layers(&delta, Some(&base), &tombstones, query, opts)
        }
        None => recall::search_layers(&delta, None, &tombstones, query, opts),
    }
}

/// Write into one agent's own delta.
///
/// There is no ceiling check: an agent is bounded by its own use, not the pod's.
/// Dedup is against what *this* agent can see — its delta first, then what its
/// preset shipped.
async fn remember_into_instance(
    instance_id: &str,
    req: RememberRequest,
    scrubbed: redact::Redaction,
) -> Result<Remembered, String> {
    let mem = handle_for_instance(instance_id)?;

    let hash = types::content_hash(&scrubbed.content);
    let mut delta = mem.delta.write().await;

    // Already learned it? Reinforce rather than duplicate.
    if let Some(existing) = delta.by_hash(&hash)
        && existing.is_live()
    {
        let mut updated = existing.clone();
        updated.importance = updated.importance.max(req.importance.unwrap_or(5.0));
        updated.pinned = updated.pinned || req.pinned;
        updated.access_count = updated.access_count.saturating_add(1);
        updated.last_accessed_at = Utc::now();
        updated.updated_at = Utc::now();
        let event = Event::Upsert {
            seq: delta.seq + 1,
            at: Utc::now(),
            memory: Box::new(updated.clone()),
        };
        commit_to(&instance_wal(instance_id), &mut delta, event, true)?;
        return Ok(Remembered {
            memory: updated,
            deduplicated: true,
            redactions: scrubbed.count,
        });
    }

    // Already *shipped* with it? Say so rather than writing a near-duplicate the
    // agent will then see twice.
    if let Some(base) = &mem.base {
        let base = base.read().await;
        if let Some(shipped) = base.by_hash(&hash)
            && shipped.is_live()
            && mem.is_visible(&shipped.id).await
        {
            return Ok(Remembered {
                memory: shipped.clone(),
                deduplicated: true,
                redactions: scrubbed.count,
            });
        }
    }

    let mut memory = Memory::new(req.kind, scrubbed.content, req.source);
    memory.summary = req.summary.unwrap_or_default();
    memory.entity = req.entity;
    memory.importance = req.importance.unwrap_or(5.0).clamp(0.0, 10.0);
    memory.pinned = req.pinned;
    memory.chat_id = req.chat_id;
    memory.persona = req.persona;
    memory.occurred_at = req.occurred_at;
    memory.confidence = match memory.source {
        Source::User | Source::Tool | Source::Seeded => 1.0,
        Source::Turn | Source::Compaction => 0.8,
        Source::Dream => 0.9,
    };

    let event = Event::Upsert {
        seq: delta.seq + 1,
        at: Utc::now(),
        memory: Box::new(memory.clone()),
    };
    commit_to(&instance_wal(instance_id), &mut delta, event, true)?;

    Ok(Remembered {
        memory,
        deduplicated: false,
        redactions: scrubbed.count,
    })
}

fn instance_wal(instance_id: &str) -> std::path::PathBuf {
    crate::paths::memory_instance_dir(instance_id).join("wal.jsonl")
}

/// What one agent knows, split by where it came from.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct InstanceMemoryView {
    pub instance_id: String,
    /// `<preset>@<version>` when this agent was shipped a knowledge base.
    pub base: Option<String>,
    /// Memories the pack gave it, still visible (tombstones excluded).
    pub shipped: usize,
    /// Memories this agent formed itself.
    pub learned: usize,
    /// Shipped memories this agent has been told to forget.
    pub forgotten: usize,
    pub sample: Vec<MemorySample>,
    /// How the machinery behind those numbers is configured and when it last ran.
    ///
    /// Shipped with the view rather than on a route of its own because the
    /// question it answers — "why does this agent know so little / so much?" — is
    /// the same question the counts above provoke, and two round trips to answer
    /// one question is two chances to show a half-loaded pane.
    pub system: MemorySystemStatus,
}

/// The state of the memory subsystem for one agent: what is switched on, what is
/// waiting, and what the dream did about it last.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct MemorySystemStatus {
    /// `MEMORY_ENABLED`. Off means no recall, no capture, no dream.
    pub enabled: bool,
    /// `MEMORY_CAPTURE` — whether turns are being written to the queue at all.
    pub capture_enabled: bool,
    /// `MEMORY_RECALL` — whether recall is injected into each turn.
    pub recall_enabled: bool,
    /// Turns captured but not yet distilled. A number that only grows means a
    /// dream that is not running.
    pub pending_captures: usize,
    /// Live memories by kind, largest first.
    pub by_kind: Vec<KindCount>,
    /// Edges in this agent's own memory graph.
    pub links: usize,
    /// Faded out by the decay pass, still on disk.
    pub archived: usize,
    /// Merged into another memory by the dream.
    pub superseded: usize,
    pub dream: DreamStatus,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct KindCount {
    pub kind: String,
    pub count: usize,
}

/// When the dream runs, and what happened the last time it did.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct DreamStatus {
    /// Whether the nightly schedule is armed (`MEMORY_DREAM`). On-demand runs work
    /// either way — turning off a schedule is not removing the capability.
    pub nightly_enabled: bool,
    /// The six-field cron it fires on.
    pub cron: String,
    /// The zone that cron is read in; absent means the pod's own clock.
    pub timezone: Option<String>,
    /// The model the thinking stages use.
    pub model: String,
    /// How recently an agent must have been used to be swept.
    pub active_days: i64,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    /// One line on what the last run did, ready to print.
    pub last_summary: Option<String>,
    /// `"nightly"`, `"catchup"` or `"manual"` — how the last run started.
    pub last_trigger: Option<String>,
    /// Why the last run failed, when it did.
    pub last_error: Option<String>,
}

/// Read the memory subsystem's state for one agent. Cheap: env reads, one journal
/// file, and counts over an index that is already resident.
pub async fn system_status(instance_id: &str) -> MemorySystemStatus {
    let last = dream::last_journal(instance_id);
    let mut status = MemorySystemStatus {
        enabled: enabled(),
        capture_enabled: capture::capture_enabled(),
        recall_enabled: recall_enabled(),
        pending_captures: capture::pending_count(instance_id),
        by_kind: Vec::new(),
        links: 0,
        archived: 0,
        superseded: 0,
        dream: DreamStatus {
            nightly_enabled: dream::nightly_enabled(),
            cron: dream::cron_expr(),
            timezone: dream::timezone(),
            model: dream::model_name(),
            active_days: dream::active_days(),
            next_run_at: dream::next_run_at(),
            last_run_at: last.as_ref().map(|r| r.finished_at),
            last_summary: last.as_ref().map(|r| r.headline()),
            last_trigger: last.as_ref().map(|r| r.trigger.clone()),
            last_error: last.as_ref().and_then(|r| r.error.clone()),
        },
    };

    // Counts come from the agent's own delta only. Its base is shared and
    // immutable, so "archived" and "superseded" there would be the pack author's
    // numbers, not this agent's, and reporting them together would make a fresh
    // agent look like it had already forgotten things.
    if let Ok(mem) = handle_for_instance(instance_id) {
        let delta = mem.delta.read().await;
        let stats = delta.stats(0);
        status.links = stats.links;
        status.archived = stats.archived;
        status.superseded = stats.superseded;
        status.by_kind = stats
            .by_kind
            .into_iter()
            .map(|(kind, count)| KindCount { kind, count })
            .collect();
    }
    status
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct MemorySample {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub importance: f32,
    /// `"shipped"` or `"learned"` — the distinction the UI actually needs.
    pub origin: &'static str,
    pub entity: Option<String>,
    pub tags: Vec<String>,
}

/// Read one agent's memory for display. Never mutates and never touches access
/// counts — looking at what an agent knows must not change its decay curve.
pub async fn instance_view(instance_id: &str, sample_limit: usize) -> InstanceMemoryView {
    let Ok(mem) = handle_for_instance(instance_id) else {
        return InstanceMemoryView {
            instance_id: instance_id.to_string(),
            base: None,
            shipped: 0,
            learned: 0,
            forgotten: 0,
            sample: Vec::new(),
            system: system_status(instance_id).await,
        };
    };

    // Read the status before taking the delta lock — `system_status` takes its
    // own, and a read guard held across that call would deadlock on a
    // non-reentrant lock.
    let system = system_status(instance_id).await;
    let tombs = mem.tombstones.read().await.clone();
    let delta = mem.delta.read().await;

    let mut sample: Vec<MemorySample> = delta
        .iter()
        .filter(|m| m.is_live())
        .map(|m| sample_of(m, "learned"))
        .collect();
    let learned = sample.len();

    let mut shipped = 0usize;
    if let Some(base) = &mem.base {
        let base = base.read().await;
        for m in base
            .iter()
            .filter(|m| m.is_live() && !tombs.contains(&m.id))
        {
            shipped += 1;
            sample.push(sample_of(m, "shipped"));
        }
    }

    // Most important first — a sample should show what the agent leans on.
    sample.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sample.truncate(sample_limit);

    InstanceMemoryView {
        instance_id: instance_id.to_string(),
        base: mem.base_key.clone(),
        shipped,
        learned,
        forgotten: tombs.len(),
        sample,
        system,
    }
}

fn sample_of(m: &Memory, origin: &'static str) -> MemorySample {
    MemorySample {
        id: m.id.clone(),
        kind: m.kind.as_str().to_string(),
        text: m.display_text().to_string(),
        importance: m.importance,
        origin,
        entity: m.entity.clone(),
        tags: m.tags.clone(),
    }
}

/// Read one memory from an agent's own layers — delta first, then its shipped base,
/// and never anything a tombstone hides.
pub async fn instance_get(instance_id: &str, id: &str) -> Option<Memory> {
    let mem = handle_for_instance(instance_id).ok()?;
    if let Some(m) = mem.delta.read().await.get(id) {
        return Some(m.clone());
    }
    if !mem.is_visible(id).await {
        return None;
    }
    let base = mem.base.as_ref()?;
    let guard = base.read().await;
    guard.get(id).cloned()
}

/// Forget inside one agent's memory. Its own memories are purged; a shipped memory
/// is tombstoned, because that copy is shared with every other agent of the preset.
pub async fn instance_forget(instance_id: &str, id: &str) -> Result<instance::Forgotten, String> {
    let mem = handle_for_instance(instance_id)?;
    mem.forget(id).await
}
