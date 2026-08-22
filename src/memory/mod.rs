//! Persistent, cross-session memory for the agent.
//!
//! See `docs/MEMORY_SYSTEM_PLAN.md` for the full design. This module is
//! **Phase 0–4**: the store, the keyword index, embeddings and hybrid recall, the
//! explicit `mem_*` tools, automatic injection into every turn, and automatic
//! capture of raw turn material. The nightly dream (Phase 5) is what turns that
//! captured material into memories — until it lands, the capture queue fills but
//! only `mem_remember` creates memories.
//!
//! Shape, and why:
//!
//! * **The store is RAM; the disk is a log.** [`index::MemoryIndex`] is
//!   authoritative; [`wal`] is how it survives a restart, and [`vectors`] is the
//!   binary sidecar for embeddings. Writes append one line (O(1)) instead of
//!   rewriting a JSON array (O(n)) the way `scheduled_tasks` does — because
//!   recall touches access times on *every* turn, and that write pattern would
//!   not survive being O(n).
//! * **One process, one handle.** A `OnceLock<Arc<RwLock<..>>>` global, the same
//!   shape `key_store` and `scheduled_tasks` use. The pod is single-tenant.
//! * **Nothing here can fail a turn.** Every public entry point returns a
//!   `Result` the caller is expected to discard on failure, and initialization
//!   degrades to an empty store rather than panicking.
//!
//! The module is named `wal` rather than `log` on purpose: `pub mod log` inside
//! this file would shadow the `log` logging facade for every sibling.
pub mod capture;
pub mod embed;
pub mod index;
pub mod instance;
pub mod inject;
pub mod recall;
pub mod redact;
pub mod tools;
pub mod types;
pub mod vectors;
pub mod wal;

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use embed::{Availability, Embedder, Embeddings};
use index::MemoryIndex;
use recall::{RecallOptions, Scored};
use types::{Event, Link, LinkKind, Memory, MemoryKind, Source, Stats};

/// Default ceiling on live memories. Everything is resident, so this is a real
/// memory-footprint bound, not a policy preference — see the RAM ceiling risk in
/// the plan. Past this, writes are refused rather than silently OOM-ing the pod.
const DEFAULT_MAX_MEMORIES: usize = 100_000;

static MEMORY: OnceLock<Arc<RwLock<MemoryIndex>>> = OnceLock::new();
/// The process-wide embedder.
///
/// A `OnceLock` would be simpler, but it can only be written once: a pod that
/// boots with no provider key and is bound one later would keep answering
/// "keyword search only" for the life of the process. Now that a key can arrive
/// through the key store mid-life, absence has to stay re-checkable. Successes
/// are cached; a missing key is not.
static EMBEDDINGS: std::sync::RwLock<Option<Arc<Embeddings>>> = std::sync::RwLock::new(None);
/// Guards the "no key / could not initialize" log so a per-recall re-check does
/// not turn into a per-recall log line.
static EMBED_QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the memory subsystem is on. `MEMORY_ENABLED=0|false|off` disables it;
/// anything else (including unset) leaves it enabled.
pub fn enabled() -> bool {
    match std::env::var("MEMORY_ENABLED") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

fn max_memories() -> usize {
    std::env::var("MEMORY_MAX_MEMORIES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_MEMORIES)
}

// ── embeddings ───────────────────────────────────────────────────────────────

/// The process-wide embedder, or `None` when no API key is configured.
///
/// Note there is no "embeddings enabled" setting: either the endpoint answers or
/// it doesn't. Missing credentials are the one case we can detect without a
/// round trip, so that alone is checked here; everything else is handled by the
/// breaker in [`embed`].
pub fn embeddings() -> Option<Arc<Embeddings>> {
    use std::sync::atomic::Ordering;

    if let Some(existing) = EMBEDDINGS.read().ok().and_then(|slot| slot.clone()) {
        return Some(existing);
    }

    // Resolved through the key store first, so a provider bound from a client is
    // picked up here on the next recall rather than at the next restart — and through
    // the same fallback the turn path uses, so a managed pod embeds with its injected
    // Metalcraft token instead of quietly downgrading recall to keyword search.
    let Some(key) = crate::runtime::inference_api_key() else {
        if !EMBED_QUIET.swap(true, Ordering::Relaxed) {
            log::info!(
                "memory: no inference credential — recall will use keyword + graph search only"
            );
        }
        return None;
    };

    let model = embed::configured_model();
    let dims = embed::configured_dims();
    match embed::OpenAiEmbedder::new(&key, &model, dims) {
        Ok(e) => {
            log::info!("memory: embeddings via {model} at {dims} dims");
            let embeddings = Arc::new(Embeddings::new(Arc::new(e)));
            if let Ok(mut slot) = EMBEDDINGS.write() {
                // Another thread may have won the race; keep whichever landed
                // first so callers all share one breaker.
                let stored = slot.get_or_insert_with(|| embeddings.clone()).clone();
                EMBED_QUIET.store(false, Ordering::Relaxed);
                return Some(stored);
            }
            Some(embeddings)
        }
        Err(e) => {
            if !EMBED_QUIET.swap(true, Ordering::Relaxed) {
                log::warn!("memory: could not initialize embeddings ({e}) — keyword search only");
            }
            None
        }
    }
}

/// Install a specific embedder. Returns `false` if one is already set, so a test
/// cannot silently swap the embedder out from under another.
///
/// Exists for tests ([`embed::NullEmbedder`]); production goes through
/// [`embeddings`].
pub fn set_embedder(embedder: Arc<dyn Embedder>) -> bool {
    let Ok(mut slot) = EMBEDDINGS.write() else {
        return false;
    };
    if slot.is_some() {
        return false;
    }
    *slot = Some(Arc::new(Embeddings::new(embedder)));
    true
}

/// Current embedding availability, for `mem_stats` and diagnostics.
pub fn embedding_availability() -> Availability {
    match embeddings() {
        Some(e) => e.availability(),
        None => Availability::Unavailable,
    }
}

// ── store lifecycle ──────────────────────────────────────────────────────────

/// The process-wide store, loaded from disk on first access.
///
/// Load order is snapshot-then-tail: read the snapshot for the bulk of the state,
/// then replay whatever the log has accumulated since, then attach vectors. A
/// missing or corrupt snapshot is not fatal — the log replay alone rebuilds
/// whatever it can, and a completely empty store is a valid starting point.
pub fn handle() -> Arc<RwLock<MemoryIndex>> {
    MEMORY
        .get_or_init(|| {
            let idx = load_from_disk();
            Arc::new(RwLock::new(idx))
        })
        .clone()
}

fn load_from_disk() -> MemoryIndex {
    let snapshot_path = crate::paths::memory_snapshot_file();
    let wal_path = crate::paths::memory_wal_file();
    let vectors_path = crate::paths::memory_vectors_file();

    // What the snapshot says the stored vectors were produced with. Compared
    // against the current config below.
    let mut stored_embedding: Option<(Option<String>, Option<usize>)> = None;

    let mut idx = match wal::read_snapshot(&snapshot_path) {
        Some(s) => {
            let n = s.memories.len();
            stored_embedding = Some((s.embed_model.clone(), s.embed_dims));
            let idx = MemoryIndex::from_snapshot(s);
            log::info!("memory: loaded snapshot with {n} memories (seq {})", idx.seq);
            idx
        }
        None => MemoryIndex::new(),
    };

    let (events, skipped) = wal::replay(&wal_path);
    let replayed = events.len();
    for e in events {
        // Events already folded into the snapshot are replayed harmlessly: every
        // variant is idempotent except `Touch`, which may over-count an access.
        idx.apply(e);
    }
    if replayed > 0 || skipped > 0 {
        log::info!("memory: replayed {replayed} log event(s), skipped {skipped} unreadable line(s)");
    }
    if skipped > 0 {
        log::warn!(
            "memory: {skipped} unreadable line(s) in {} — likely a torn write from an unclean shutdown",
            wal_path.display()
        );
    }

    let (stored_vectors, torn) = vectors::load(&vectors_path);
    if torn > 0 {
        log::warn!("memory: a truncated record at the end of {} was skipped", vectors_path.display());
    }

    // Vectors are only comparable to vectors from the same model at the same
    // dimensionality. On a mismatch we do NOT mix the two — we drop them all and
    // let the backfill re-embed, because a silently mixed corpus ranks nonsense
    // and gives no signal that anything is wrong.
    let want_model = embed::configured_model();
    let want_dims = embed::configured_dims();
    let stale = match stored_embedding {
        Some((Some(model), Some(dims))) => model != want_model || dims != want_dims,
        // A snapshot written before vectors existed, or with no vectors recorded:
        // trust the file and let dimension checking below catch a real mismatch.
        _ => false,
    };
    if stale {
        log::warn!(
            "memory: stored vectors were produced by a different embedding model/size than the \
             configured {want_model}@{want_dims} — discarding {} of them. Recall runs on keyword + \
             graph search until they are re-embedded.",
            stored_vectors.len()
        );
    } else {
        let wrong_size = stored_vectors.values().filter(|v| v.len() != want_dims).count();
        if wrong_size > 0 {
            log::warn!("memory: {wrong_size} stored vector(s) have the wrong dimensionality and were dropped");
        }
        let kept: std::collections::HashMap<String, Vec<f32>> =
            stored_vectors.into_iter().filter(|(_, v)| v.len() == want_dims).collect();
        let loaded = idx.load_vectors(kept);
        if loaded > 0 {
            log::info!("memory: attached {loaded} embedding(s)");
        }
    }

    idx
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
    /// Write into this agent's own delta rather than the pod-global store. An
    /// agent that recalls from its instance must write there too, or it would
    /// never see what it just chose to remember.
    pub instance_id: Option<String>,
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
            instance_id: None,
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

/// Write a memory.
///
/// Content is scrubbed for secrets first (always — this is not optional, see
/// [`redact`]). An exact duplicate is *reinforced* rather than stored twice:
/// importance takes the max of old and new and the access clock is bumped, which
/// is what "the user told me this again" should mean.
///
/// Embedding happens in a detached task, so a write never waits on the network.
/// If that task fails, the memory is simply left for the next
/// [`backfill_embeddings`] — nothing is lost, it is just keyword-only until then.
pub async fn remember(req: RememberRequest) -> Result<Remembered, String> {
    if !enabled() {
        return Err("memory is disabled (MEMORY_ENABLED)".into());
    }
    if req.content.trim().is_empty() {
        return Err("content is required".into());
    }

    let scrubbed = redact::redact(&req.content);
    if scrubbed.count > 0 {
        log::info!("memory: redacted {} secret(s) ({}) before storing", scrubbed.count, scrubbed.kinds.join(", "));
    }

    if let Some(instance_id) = req.instance_id.clone() {
        return remember_into_instance(&instance_id, req, scrubbed).await;
    }

    let handle = handle();
    let mut idx = handle.write().await;

    let hash = types::content_hash(&scrubbed.content);
    if let Some(existing) = idx.by_hash(&hash)
        && existing.is_live()
    {
        let mut updated = existing.clone();
        updated.importance = updated.importance.max(req.importance.unwrap_or(5.0));
        updated.pinned = updated.pinned || req.pinned;
        updated.access_count = updated.access_count.saturating_add(1);
        updated.last_accessed_at = Utc::now();
        updated.updated_at = Utc::now();
        let event = Event::Upsert {
            seq: idx.seq + 1,
            at: Utc::now(),
            memory: Box::new(updated.clone()),
        };
        commit(&mut idx, event, true)?;
        return Ok(Remembered { memory: updated, deduplicated: true, redactions: scrubbed.count });
    }

    if idx.len() >= max_memories() {
        return Err(format!(
            "memory is at its ceiling of {} records — the decay pass has not run or the limit needs raising (MEMORY_MAX_MEMORIES)",
            max_memories()
        ));
    }

    let mut memory = Memory::new(req.kind, scrubbed.content, req.source);
    memory.summary = req.summary.unwrap_or_default();
    memory.entity = req.entity;
    memory.importance = req.importance.unwrap_or(5.0).clamp(0.0, 10.0);
    memory.pinned = req.pinned;
    memory.chat_id = req.chat_id;
    memory.persona = req.persona;
    memory.occurred_at = req.occurred_at;
    // Gateway-sourced material is less trustworthy than something the operator
    // said directly; the dream's contradiction handling uses this.
    memory.confidence = match memory.source {
        Source::User | Source::Tool => 1.0,
        Source::Turn | Source::Compaction => 0.8,
        Source::Dream => 0.9,
        // Authored by a human and reviewed at publish; trusted like a tool write.
        Source::Seeded => 1.0,
    };

    let event = Event::Upsert { seq: idx.seq + 1, at: Utc::now(), memory: Box::new(memory.clone()) };
    // An explicit write is `fsync`ed: "remember this" must survive a crash, which
    // bulk machine-generated captures do not need to.
    commit(&mut idx, event, true)?;
    drop(idx);

    // Detached so the write returns at local-disk speed, not network speed.
    if embeddings().is_some() {
        let id = memory.id.clone();
        let text = memory.indexable();
        tokio::spawn(async move {
            if let Err(e) = embed_one(&id, &text).await {
                log::debug!("memory: deferred embedding for {id} failed ({e}); backfill will retry");
            }
        });
    }

    Ok(Remembered { memory, deduplicated: false, redactions: scrubbed.count })
}

/// Append an event to the log, then apply it to the index.
///
/// Log-before-apply is deliberate: if the append fails, in-memory state must not
/// diverge from disk, so the caller sees the error and the store is unchanged.
fn commit(idx: &mut MemoryIndex, event: Event, durable: bool) -> Result<(), String> {
    commit_to(&crate::paths::memory_wal_file(), idx, event, durable)
}

/// Append to a specific log, then apply. An agent instance owns its own log, so
/// its writes are durable without going through the pod-global store.
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
    let write = if durable { wal::append_durable } else { wal::append };
    write(path, &event).map_err(|e| format!("failed to write memory log: {e}"))?;
    idx.apply(event);
    Ok(())
}

/// Embed one memory and persist the vector.
async fn embed_one(id: &str, text: &str) -> Result<(), String> {
    let Some(emb) = embeddings() else {
        return Err("no embedder configured".into());
    };
    let mut vecs = emb.embed_batch(vec![text.to_string()]).await?;
    if vecs.is_empty() {
        return Err("embedder returned nothing".into());
    }
    let vec = vecs.remove(0);
    store_vector(id, vec).await
}

async fn store_vector(id: &str, vec: Vec<f32>) -> Result<(), String> {
    vectors::append(&crate::paths::memory_vectors_file(), id, &vec)
        .map_err(|e| format!("failed to write vector: {e}"))?;
    let handle = handle();
    let mut idx = handle.write().await;
    // Only attach if the memory still exists — it may have been purged while the
    // embedding was in flight.
    if idx.get(id).is_some() {
        idx.set_vector(id, vec);
    }
    Ok(())
}

/// Embed live memories that have no vector yet, up to `limit`.
///
/// This is the self-healing path: anything written while the endpoint was down,
/// or before a model change, gets picked up here. Phase 5's dream calls it
/// nightly; it is public now so a store can be brought up to date (and tested)
/// without one.
///
/// Returns how many vectors were produced.
pub async fn backfill_embeddings(limit: usize) -> Result<usize, String> {
    let Some(emb) = embeddings() else {
        return Ok(0);
    };
    let handle = handle();
    let pending: Vec<(String, String)> = {
        let idx = handle.read().await;
        idx.missing_vectors(limit)
    };
    if pending.is_empty() {
        return Ok(0);
    }

    let mut done = 0usize;
    for chunk in pending.chunks(embed::BATCH_SIZE) {
        let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
        let vecs = emb.embed_batch(texts).await?;
        for ((id, _), vec) in chunk.iter().zip(vecs) {
            store_vector(id, vec).await?;
            done += 1;
        }
    }
    if done > 0 {
        log::info!("memory: embedded {done} memory/memories");
    }
    Ok(done)
}

// ── reading ──────────────────────────────────────────────────────────────────

/// Hybrid recall: keyword + vector + graph, fused. The main read path.
///
/// The query embedding is bounded by `MEMORY_RECALL_TIMEOUT_MS` and simply
/// omitted on timeout, so a slow or missing embeddings endpoint costs a worse
/// ranking rather than a failed turn.
pub async fn recall(query: &str, opts: RecallOptions) -> Vec<Scored> {
    if !enabled() || query.trim().is_empty() {
        return Vec::new();
    }

    let query_vec = if opts.mode == recall::Mode::Text {
        None
    } else {
        match embeddings() {
            Some(e) => e.embed_query(query, embed::query_timeout()).await,
            None => None,
        }
    };

    // An instance recalls across its own two layers; without one we fall back to the
    // pod-global store, which is what a legacy pod and the CLI still use.
    if let Some(instance_id) = opts.instance_id.clone() {
        return recall_for_instance(&instance_id, query, query_vec.as_deref(), &opts).await;
    }

    let handle = handle();
    let results = {
        let idx = handle.read().await;
        recall::search_index(&idx, query, query_vec.as_deref(), &opts)
    };
    if results.is_empty() {
        return results;
    }

    // Record the access — this is what feeds the decay curve later, so it must
    // reflect actual use rather than mere existence.
    let ids: Vec<String> = results.iter().map(|r| r.memory.id.clone()).collect();
    let mut idx = handle.write().await;
    let touched = idx.touch(&ids);
    if !touched.is_empty() {
        let event = Event::Touch { seq: idx.seq + 1, at: Utc::now(), ids: touched };
        if let Err(e) = commit(&mut idx, event, false) {
            // Losing a Touch costs a slightly stale decay input, nothing more.
            log::debug!("memory: could not record access ({e})");
        }
    }
    results
}

/// One memory plus its graph edges.
pub async fn get(id: &str) -> Option<(Memory, Vec<Link>, Vec<Link>)> {
    let handle = handle();
    let idx = handle.read().await;
    let memory = idx.get(id)?.clone();
    Some((memory, idx.links_from(id).to_vec(), idx.links_to(id).to_vec()))
}

/// Archive (soft) or purge (hard) a memory.
///
/// Archive is the default everywhere else in the system, because automatic
/// forgetting must be reversible. `purge` is for when a human says "delete that"
/// — an explicit instruction deserves an actual deletion.
pub async fn forget(id: &str, purge: bool) -> Result<(), String> {
    let handle = handle();
    let mut idx = handle.write().await;
    if idx.get(id).is_none() {
        return Err(format!("no memory with id '{id}'"));
    }
    let event = if purge {
        Event::Purge { seq: idx.seq + 1, at: Utc::now(), id: id.to_string() }
    } else {
        Event::Archive { seq: idx.seq + 1, at: Utc::now(), id: id.to_string() }
    };
    commit(&mut idx, event, true)
}

/// Create a typed edge between two memories.
pub async fn link(src: &str, dst: &str, kind: LinkKind, created_by: &str) -> Result<(), String> {
    let handle = handle();
    let mut idx = handle.write().await;
    for id in [src, dst] {
        if idx.get(id).is_none() {
            return Err(format!("no memory with id '{id}'"));
        }
    }
    if src == dst {
        return Err("a memory cannot link to itself".into());
    }
    let event = Event::Link {
        seq: idx.seq + 1,
        at: Utc::now(),
        link: Link { src: src.to_string(), dst: dst.to_string(), kind, weight: 1.0, created_by: created_by.to_string() },
    };
    commit(&mut idx, event, true)
}

pub async fn stats() -> Stats {
    let handle = handle();
    let idx = handle.read().await;
    idx.stats(wal::count(&crate::paths::memory_wal_file()))
}

// ── turn integration ─────────────────────────────────────────────────────────

/// Whether per-turn recall injection is on (`MEMORY_RECALL`). The profile block
/// is governed separately, by [`enabled`] alone, because it is cheap and stable.
pub fn recall_enabled() -> bool {
    if !enabled() {
        return false;
    }
    match std::env::var("MEMORY_RECALL") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
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
/// Returns an empty string when memory is off or there is nothing durable yet,
/// which suppresses the whole section rather than printing an empty heading.
pub async fn profile_block() -> String {
    if !enabled() {
        return String::new();
    }
    let budget = profile_token_budget();
    let handle = handle();
    let idx = handle.read().await;

    let mut candidates: Vec<&Memory> = idx
        .iter()
        .filter(|m| {
            m.is_live()
                && (m.pinned
                    || matches!(m.kind, MemoryKind::Preference | MemoryKind::Procedural))
        })
        .collect();
    // Pinned first, then by importance, then newest — a stable total order, so
    // the block does not reshuffle between turns and bust the prompt cache.
    candidates.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| b.importance.partial_cmp(&a.importance).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });

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

/// Fold the log into a fresh snapshot, rewrite the vector file, and truncate the
/// log.
///
/// Ordering is snapshot-then-truncate: a crash between the two replays
/// already-applied events, which is harmless, whereas the reverse order would
/// lose them. Phase 5's dream calls this nightly; it is public now so the store
/// can be compacted (and tested) without one.
pub async fn compact() -> Result<u64, String> {
    let handle = handle();
    let idx = handle.read().await;
    let wal_path = crate::paths::memory_wal_file();
    let folded = wal::count(&wal_path);

    // Record what produced the stored vectors, so a later model change is
    // detectable rather than silently mixing incomparable embeddings.
    let (model, dims) = match embeddings() {
        Some(e) => (Some(e.model().to_string()), Some(e.dims())),
        None => (Some(embed::configured_model()), Some(embed::configured_dims())),
    };

    let snapshot = idx.snapshot(model, dims);
    wal::write_snapshot(&crate::paths::memory_snapshot_file(), &snapshot)
        .map_err(|e| format!("failed to write memory snapshot: {e}"))?;

    // Collapses superseded records and drops vectors for purged memories.
    let kept = vectors::rewrite(&crate::paths::memory_vectors_file(), idx.vectors_iter())
        .map_err(|e| format!("failed to rewrite memory vectors: {e}"))?;

    wal::truncate(&wal_path).map_err(|e| format!("failed to truncate memory log: {e}"))?;
    log::info!(
        "memory: compacted {folded} log event(s) into a snapshot at seq {}, kept {kept} vector(s)",
        idx.seq
    );
    Ok(folded)
}


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

/// Recall across one agent's layers. Degrades to the delta alone if the base can't
/// be loaded — a missing base is a smaller agent, never a failed turn.
async fn recall_for_instance(
    instance_id: &str,
    query: &str,
    query_vec: Option<&[f32]>,
    opts: &RecallOptions,
) -> Vec<Scored> {
    let base_ref = base_for_instance(instance_id);
    let base_arg = base_ref.as_ref().map(|(p, v)| (p.as_str(), v.as_str()));
    let mem = match instance::handle_for(instance_id, base_arg) {
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
            recall::search_layers(&delta, Some(&base), &tombstones, query, query_vec, opts)
        }
        None => recall::search_layers(&delta, None, &tombstones, query, query_vec, opts),
    }
}

/// Give an instance its starting knowledge by pointing it at a preset base.
///
/// This is O(1): no records are copied. Building the base itself happens once per
/// `preset@version` at pack install ([`instance::build_base`]).
pub fn attach_base(instance_id: &str, preset: &str, version: &str) -> Result<usize, String> {
    let base = instance::load_base(preset, version)?;
    let count = base.try_read().map(|b| b.len()).unwrap_or(0);
    instance::handle_for(instance_id, Some((preset, version)))?;
    Ok(count)
}


/// Write into one agent's own delta.
///
/// Deliberately simpler than the global path: no ceiling check (an instance is bounded
/// by its own use, not the pod's), and dedup is against what *this* agent can see.
async fn remember_into_instance(
    instance_id: &str,
    req: RememberRequest,
    scrubbed: redact::Redaction,
) -> Result<Remembered, String> {
    let base_ref = base_for_instance(instance_id);
    let base_arg = base_ref.as_ref().map(|(p, v)| (p.as_str(), v.as_str()));
    let mem = instance::handle_for(instance_id, base_arg)?;

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
        return Ok(Remembered { memory: updated, deduplicated: true, redactions: scrubbed.count });
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

    let event =
        Event::Upsert { seq: delta.seq + 1, at: Utc::now(), memory: Box::new(memory.clone()) };
    commit_to(&instance_wal(instance_id), &mut delta, event, true)?;

    Ok(Remembered { memory, deduplicated: false, redactions: scrubbed.count })
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
    let base_ref = base_for_instance(instance_id);
    let base_arg = base_ref.as_ref().map(|(p, v)| (p.as_str(), v.as_str()));
    let Ok(mem) = instance::handle_for(instance_id, base_arg) else {
        return InstanceMemoryView {
            instance_id: instance_id.to_string(),
            base: None,
            shipped: 0,
            learned: 0,
            forgotten: 0,
            sample: Vec::new(),
        };
    };

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
        for m in base.iter().filter(|m| m.is_live() && !tombs.contains(&m.id)) {
            shipped += 1;
            sample.push(sample_of(m, "shipped"));
        }
    }

    // Most important first — a sample should show what the agent leans on.
    sample.sort_by(|a, b| b.importance.partial_cmp(&a.importance).unwrap_or(std::cmp::Ordering::Equal));
    sample.truncate(sample_limit);

    InstanceMemoryView {
        instance_id: instance_id.to_string(),
        base: mem.base_key.clone(),
        shipped,
        learned,
        forgotten: tombs.len(),
        sample,
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
    let base_ref = base_for_instance(instance_id);
    let base_arg = base_ref.as_ref().map(|(p, v)| (p.as_str(), v.as_str()));
    let mem = instance::handle_for(instance_id, base_arg).ok()?;
    if let Some(m) = mem.delta.read().await.get(id) {
        return Some(m.clone());
    }
    if !mem.is_visible(id).await {
        return None;
    }
    let base = mem.base.as_ref()?;
    let hit = base.read().await.get(id).cloned();
    hit
}

/// Forget inside one agent's memory. Its own memories are purged; a shipped memory
/// is tombstoned, because that copy is shared with every other agent of the preset.
pub async fn instance_forget(
    instance_id: &str,
    id: &str,
) -> Result<instance::Forgotten, String> {
    let base_ref = base_for_instance(instance_id);
    let base_arg = base_ref.as_ref().map(|(p, v)| (p.as_str(), v.as_str()));
    let mem = instance::handle_for(instance_id, base_arg)?;
    mem.forget(id).await
}
