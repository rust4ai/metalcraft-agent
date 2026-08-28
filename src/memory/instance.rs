//! Per-instance memory: a shared, immutable **base** plus a writable **delta**.
//!
//! ## Why two layers rather than a copy
//!
//! The obvious implementation of "an agent starts out knowing what its pack shipped"
//! is to copy the pack's memories into the new instance. Across twenty instances of
//! one preset that is twenty copies of every record, and instance creation
//! becomes O(memories) — on the interactive path, because creating an instance *is*
//! starting a chat.
//!
//! Instead the base is loaded once per `preset@version`, shared by every instance of
//! it (one copy on disk, one in RAM, refcounted), and each instance owns only what it
//! has actually changed:
//!
//! * a **write** goes to the delta;
//! * an **edit** of a base memory materializes that one record into the delta, which
//!   shadows the base by id;
//! * a **forget** of a base memory writes a tombstone.
//!
//! Instance creation is then O(1): a pointer and an empty delta.
//!
//! ## The part that isn't just an optimization
//!
//! Because personas and skills follow the installed pack version, seed memories have
//! to as well — and the delta is what makes that coherent. Updating a pack repoints
//! `base` from `amy-kitchen@1.4.0` to `@1.5.0`: new authored memories appear, anything
//! the user forgot stays forgotten (the tombstone lives in the delta), and everything
//! the instance learned is untouched. Copying would force a choice between clobbering
//! the agent's own learning and never delivering the author's fix.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::index::MemoryIndex;
use super::types::{Memory, MemoryKind, Source};

/// How many instance deltas stay resident. Beyond this the least recently used is
/// flushed and dropped; base layers are refcounted separately and survive as long as
/// any resident instance references them.
const DEFAULT_RESIDENT_LIMIT: usize = 8;

/// A base layer key: `<preset-slug>@<version>`.
pub fn base_key(preset: &str, version: &str) -> String {
    format!("{preset}@{version}")
}

/// One agent's memory: what its pack gave it, and what it has learned since.
#[derive(Clone)]
pub struct InstanceMemory {
    pub instance_id: String,
    /// Shared and immutable. `None` for an instance with no preset base — a legacy
    /// pod, or a preset that ships no memories.
    pub base: Option<Arc<RwLock<MemoryIndex>>>,
    pub base_key: Option<String>,
    /// This instance's own memories.
    pub delta: Arc<RwLock<MemoryIndex>>,
    /// Base ids this instance has forgotten. Kept out of the delta index so a
    /// tombstone survives the base being repointed at a new pack version.
    pub tombstones: Arc<RwLock<HashSet<String>>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Tombstones {
    #[serde(default)]
    ids: Vec<String>,
}

impl InstanceMemory {
    fn tombstone_path(instance_id: &str) -> std::path::PathBuf {
        crate::paths::memory_instance_dir(instance_id).join("tombstones.json")
    }

    fn load_tombstones(instance_id: &str) -> HashSet<String> {
        match std::fs::read_to_string(Self::tombstone_path(instance_id)) {
            Ok(s) => serde_json::from_str::<Tombstones>(&s)
                .map(|t| t.ids.into_iter().collect())
                .unwrap_or_default(),
            Err(_) => HashSet::new(),
        }
    }

    async fn save_tombstones(&self) -> Result<(), String> {
        let ids: Vec<String> = self.tombstones.read().await.iter().cloned().collect();
        let path = Self::tombstone_path(&self.instance_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        // Union with what is already on disk rather than replacing it. Two handles for
        // the same agent can exist at once — eviction drops the registry entry while a
        // caller still holds its clone — and each would otherwise serialise only the
        // set *it* loaded, un-forgetting whatever the other had forgotten.
        let mut ids: HashSet<String> = ids.into_iter().collect();
        ids.extend(Self::load_tombstones(&self.instance_id));
        let ids: Vec<String> = {
            let mut v: Vec<String> = ids.into_iter().collect();
            v.sort();
            v
        };
        let json = serde_json::to_string_pretty(&Tombstones { ids })
            .map_err(|e| format!("failed to serialize tombstones: {e}"))?;
        // tmp + rename: a torn write must not lose the whole set.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("failed to finalize tombstones: {e}"))
    }

    /// Forget a memory. A delta record is purged outright; a base record is
    /// tombstoned, because the base is shared and must never be mutated by one
    /// instance's decision.
    pub async fn forget(&self, id: &str) -> Result<Forgotten, String> {
        {
            let mut delta = self.delta.write().await;
            if delta.get(id).is_some() {
                delta.purge(id);
                // Record it. The purge used to be RAM-only, so the next time this
                // agent's log was replayed the memory came straight back — a forget
                // that survived only until the instance left the resident set.
                delta.seq += 1;
                let event = crate::memory::types::Event::Purge {
                    seq: delta.seq,
                    at: chrono::Utc::now(),
                    id: id.to_string(),
                };
                if let Err(e) = crate::memory::wal::append(
                    &crate::paths::memory_instance_dir(&self.instance_id).join("wal.jsonl"),
                    &event,
                ) {
                    // The memory is already gone from RAM; failing the call would be
                    // a lie in the other direction. Warn and move on.
                    log::warn!(
                        "memory: instance '{}': could not record a purge: {e}",
                        self.instance_id
                    );
                }
                return Ok(Forgotten::Purged);
            }
        }
        let in_base = match &self.base {
            Some(b) => b.read().await.get(id).is_some(),
            None => false,
        };
        if !in_base {
            return Err(format!("memory '{id}' not found"));
        }
        self.tombstones.write().await.insert(id.to_string());
        self.save_tombstones().await?;
        Ok(Forgotten::Tombstoned)
    }

    /// Whether this instance still sees `id`.
    pub async fn is_visible(&self, id: &str) -> bool {
        !self.tombstones.read().await.contains(id)
    }

    /// Total memories this agent can see: base minus tombstones, plus delta, with
    /// delta shadowing base on id collision.
    pub async fn visible_count(&self) -> usize {
        let tombs = self.tombstones.read().await;
        let delta = self.delta.read().await;
        let delta_ids: HashSet<&String> = delta.iter().map(|m| &m.id).collect();
        let base_visible = match &self.base {
            Some(b) => b
                .read()
                .await
                .iter()
                .filter(|m| !tombs.contains(&m.id) && !delta_ids.contains(&m.id))
                .count(),
            None => 0,
        };
        base_visible + delta.len()
    }

    /// Swap the base layer to a different pack version. New authored memories appear,
    /// tombstones persist, and the delta is untouched — see the module docs.
    pub async fn repoint_base(&mut self, preset: &str, version: &str) -> Result<(), String> {
        let key = base_key(preset, version);
        let base = load_base(preset, version)?;
        self.base = Some(base);
        self.base_key = Some(key);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Forgotten {
    /// It was this instance's own memory; it is gone.
    Purged,
    /// It came from the pack; the shared copy stays, this agent stops seeing it.
    Tombstoned,
}

// ── base layers ──────────────────────────────────────────────────────────────

type BaseMap = HashMap<String, Arc<RwLock<MemoryIndex>>>;

fn bases() -> &'static std::sync::RwLock<BaseMap> {
    static BASES: std::sync::OnceLock<std::sync::RwLock<BaseMap>> = std::sync::OnceLock::new();
    BASES.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

/// One line of a pack's `memories.jsonl`, as authored on the registry and compiled
/// at build time. Deliberately not [`Memory`]: ids and timestamps are minted here, so
/// the same file always produces the same index.
#[derive(Debug, Clone, Deserialize)]
pub struct SeedMemory {
    #[serde(default)]
    pub kind: Option<String>,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub importance: Option<f32>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_entry: Option<String>,
}

impl SeedMemory {
    fn into_memory(self) -> Memory {
        let kind = self
            .kind
            .as_deref()
            .and_then(MemoryKind::parse)
            .unwrap_or(MemoryKind::Semantic);
        let mut m = Memory::new(kind, self.content, Source::Seeded);
        if let Some(s) = self.summary {
            m.summary = s;
        }
        m.entity = self.entity;
        if let Some(i) = self.importance {
            m.importance = i;
        }
        m.tags = self.tags;
        m
    }
}

/// Parse a `memories.jsonl`. A malformed line is skipped with a warning rather than
/// failing the install — one bad record must not cost an agent its whole knowledge.
pub fn parse_seed_file(contents: &str) -> (Vec<Memory>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for (n, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        match serde_json::from_str::<SeedMemory>(line) {
            Ok(seed) => out.push(seed.into_memory()),
            Err(e) => {
                log::warn!("memory: skipping seed line {}: {e}", n + 1);
                skipped += 1;
            }
        }
    }
    (out, skipped)
}

/// The version a preset's shipped memories are currently stored under.
///
/// **The agent pack's manifest version, not the preset document's.** `build_base` is
/// called at install with `manifest.version`, so that is the key on disk — and the
/// two fields are independent, unvalidated strings. Reading the preset's own
/// `version` meant an export defaulting to `0.1.0` while the preset document still
/// said `1.4.0` built `amy@0.1.0` and then looked up `amy@1.4.0`: every agent of that
/// preset silently lost one hundred percent of its shipped memories, reported as
/// `shipped: 0` with only a debug-level warning.
///
/// `None` for a preset no installed pack provides — a seeded or hand-authored one,
/// which has no shipped base at all.
pub fn current_base_version(preset: &str) -> Option<String> {
    crate::agent_packs::list()
        .into_iter()
        .find(|p| p.manifest.presets.iter().any(|s| s == preset))
        .map(|p| p.manifest.version)
}

/// The id a shipped memory has, for a given preset.
///
/// Content-derived and **deliberately not version-scoped**. A tombstone names an id;
/// if the id moved when the pack was upgraded, every memory the user had told the
/// agent to forget would come back, because the same unchanged seed line would hash
/// differently. Scoping by preset keeps the same sentence shipped in two different
/// agents distinct.
fn deterministic_base_id(preset: &str, content: &str, occurrence: usize) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(preset.as_bytes());
    h.update([0]);
    h.update(content.as_bytes());
    let base = format!("seed_{}", &hex::encode(h.finalize())[..32]);
    // Two seed lines may share `content` and differ in kind, entity or importance —
    // they are distinct memories, and hashing content alone collapsed them into one,
    // silently dropping the first. The occurrence index disambiguates, and only the
    // duplicate carries a suffix so the common case stays stable if a file is edited.
    if occurrence == 0 {
        base
    } else {
        format!("{base}_{occurrence}")
    }
}

/// Build a preset's base index from a `memories.jsonl` and persist it as a snapshot.
///
/// Runs **once per `preset@version`** at pack install — not per instance — so twenty
/// agents share one copy on disk.
pub fn build_base(preset: &str, version: &str, seed_file: &Path) -> Result<usize, String> {
    let contents = std::fs::read_to_string(seed_file)
        .map_err(|e| format!("failed to read {}: {e}", seed_file.display()))?;
    let (memories, skipped) = parse_seed_file(&contents);

    let mut idx = MemoryIndex::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for mut m in memories {
        // A deterministic id. `Memory::new` mints a v4 uuid, so rebuilding the same
        // file — which reinstalling or upgrading does — produced an entirely new set
        // of ids, and every tombstone an agent had written pointed at nothing.
        // Memories the user had told the agent to forget silently came back.
        let occurrence = seen.entry(m.content.clone()).or_insert(0);
        m.id = deterministic_base_id(preset, &m.content, *occurrence);
        *occurrence += 1;
        idx.insert_memory(m);
    }
    // `count` is what the install report shows. Reporting the parsed line count while
    // the index holds fewer would hide exactly the collision the occurrence index
    // exists to prevent.
    let count = idx.len();

    let dir = crate::paths::memory_preset_dir(preset, version);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let snapshot = idx.snapshot();
    let json = serde_json::to_string(&snapshot)
        .map_err(|e| format!("failed to serialize base snapshot: {e}"))?;
    let path = dir.join("snapshot.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("failed to finalize {}: {e}", path.display()))?;

    if skipped > 0 {
        log::warn!(
            "memory: {skipped} unreadable line(s) in {}",
            seed_file.display()
        );
    }
    log::info!("memory: built base {preset}@{version} with {count} memories");
    // Drop any cached copy so a rebuild is picked up.
    bases()
        .write()
        .ok()
        .map(|mut b| b.remove(&base_key(preset, version)));
    Ok(count)
}

/// Load (or reuse) a preset's base layer. Shared across every instance of that
/// version — the whole point of the split.
pub fn load_base(preset: &str, version: &str) -> Result<Arc<RwLock<MemoryIndex>>, String> {
    let key = base_key(preset, version);
    if let Some(existing) = bases().read().ok().and_then(|b| b.get(&key).cloned()) {
        return Ok(existing);
    }
    let path = crate::paths::memory_preset_dir(preset, version).join("snapshot.json");
    let contents =
        std::fs::read_to_string(&path).map_err(|_| format!("no base memory built for {key}"))?;
    let snapshot = serde_json::from_str(&contents)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    let idx = Arc::new(RwLock::new(MemoryIndex::from_snapshot(snapshot)));
    if let Ok(mut b) = bases().write() {
        b.insert(key, idx.clone());
    }
    Ok(idx)
}

/// Base layers currently held in memory. Diagnostics for the resident-set question.
pub fn resident_bases() -> Vec<String> {
    bases()
        .read()
        .map(|b| b.keys().cloned().collect())
        .unwrap_or_default()
}

// ── instance registry ────────────────────────────────────────────────────────

struct Registry {
    resident: HashMap<String, InstanceMemory>,
    /// Most-recently-used last.
    order: Vec<String>,
    limit: usize,
}

fn registry() -> &'static std::sync::Mutex<Registry> {
    static REG: std::sync::OnceLock<std::sync::Mutex<Registry>> = std::sync::OnceLock::new();
    REG.get_or_init(|| {
        let limit = std::env::var("METALCRAFT_MEMORY_RESIDENT_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RESIDENT_LIMIT);
        std::sync::Mutex::new(Registry {
            resident: HashMap::new(),
            order: Vec::new(),
            limit: limit.max(1),
        })
    })
}

/// Get (loading if needed) an instance's memory, marking it most-recently-used.
///
/// `base` is `Some((preset, version))` when the instance was created from a preset
/// that ships memories. A missing base is not an error — the agent simply starts
/// with nothing but what it learns.
pub fn handle_for(instance_id: &str, base: Option<(&str, &str)>) -> Result<InstanceMemory, String> {
    let mut reg = registry()
        .lock()
        .map_err(|_| "memory registry poisoned".to_string())?;

    let wanted_key = base.map(|(p, v)| base_key(p, v));
    if let Some(existing) = reg.resident.get(instance_id).cloned() {
        // …but only if it is still reading the right base. After a pack upgrade the
        // resident copy holds an `Arc` to the *old* index, which nothing else can
        // reach, so it kept serving the superseded memories until it happened to fall
        // out of the LRU. Two agents of the same preset then disagreed about what
        // the pack had shipped.
        if existing.base_key == wanted_key {
            reg.order.retain(|k| k != instance_id);
            reg.order.push(instance_id.to_string());
            return Ok(existing);
        }
        reg.resident.remove(instance_id);
        reg.order.retain(|k| k != instance_id);
    }

    let (base_idx, key) = match base {
        Some((preset, version)) => match load_base(preset, version) {
            Ok(b) => (Some(b), Some(base_key(preset, version))),
            Err(e) => {
                // Degrade, never block: an agent whose base failed to load still works.
                log::warn!("memory: instance '{instance_id}': {e}");
                (None, None)
            }
        },
        None => (None, None),
    };

    let mem = InstanceMemory {
        instance_id: instance_id.to_string(),
        base: base_idx,
        base_key: key,
        delta: Arc::new(RwLock::new(load_delta(instance_id))),
        tombstones: Arc::new(RwLock::new(InstanceMemory::load_tombstones(instance_id))),
    };

    reg.resident.insert(instance_id.to_string(), mem.clone());
    reg.order.push(instance_id.to_string());

    // Evict the least recently used. Its delta is dropped from RAM; anything durable
    // was already on disk, and a base stays alive while another instance holds it.
    while reg.order.len() > reg.limit {
        let evicted = reg.order.remove(0);
        reg.resident.remove(&evicted);
        log::debug!("memory: evicted instance '{evicted}' from the resident set");
    }
    Ok(mem)
}

/// How many replayed events justify writing a snapshot.
///
/// Low enough that a busy agent never replays a long log on the interactive path,
/// high enough that an agent written to once in a while never pays for a snapshot.
const COMPACT_AFTER_EVENTS: usize = 200;

/// Rebuild an agent's own memories from its log.
///
/// **Without this, every learned memory was write-only.** `remember_into_instance`
/// fsyncs an `Upsert` to `memory/instances/<id>/wal.jsonl`, but the delta was
/// constructed empty every time an instance became resident — so a restart, or
/// simply eight other chats pushing this one out of the LRU, made everything the
/// agent had been told unreachable. It stayed on disk, correct and unread.
///
/// Snapshot first if there is one, then replay the log over it. Events already
/// folded into the snapshot replay harmlessly — every variant is idempotent except `Touch`, which may
/// over-count an access.
fn load_delta(instance_id: &str) -> MemoryIndex {
    let dir = crate::paths::memory_instance_dir(instance_id);
    let wal_path = dir.join("wal.jsonl");
    let snapshot_path = dir.join("snapshot.json");
    let mut idx = match crate::memory::wal::read_snapshot(&snapshot_path) {
        Some(s) => MemoryIndex::from_snapshot(s),
        None => MemoryIndex::new(),
    };
    let (events, skipped) = crate::memory::wal::replay(&wal_path);
    let replayed = events.len();
    for e in events {
        idx.apply(e);
    }

    // Fold the log into a snapshot once it is long enough to be worth it.
    //
    // The resident set holds eight agents, so a pod with more than that replays on
    // every miss — and this is the interactive path, at the front of a turn. Without
    // compaction the log only grows, so an agent with ten thousand lifetime writes
    // pays for all ten thousand every time it is recalled into memory.
    //
    // Snapshot first, then truncate: a crash between the two replays a few
    // already-folded events, which is harmless because every variant is idempotent.
    // The reverse order would lose them.
    if replayed >= COMPACT_AFTER_EVENTS {
        let snapshot = idx.snapshot();
        match crate::memory::wal::write_snapshot(&snapshot_path, &snapshot) {
            Ok(()) => {
                if let Err(e) = crate::memory::wal::truncate(&wal_path) {
                    log::warn!("memory: instance '{instance_id}': could not truncate log: {e}");
                } else {
                    log::debug!(
                        "memory: instance '{instance_id}': folded {replayed} event(s) into a snapshot"
                    );
                }
            }
            // Compaction is an optimisation; failing it must never fail a turn.
            Err(e) => log::warn!("memory: instance '{instance_id}': snapshot failed: {e}"),
        }
    }
    if replayed > 0 || skipped > 0 {
        log::debug!(
            "memory: instance '{instance_id}': replayed {replayed} event(s), skipped {skipped}"
        );
    }
    if skipped > 0 {
        log::warn!(
            "memory: {skipped} unreadable line(s) in {} — likely a torn write from an \
             unclean shutdown",
            dir.join("wal.jsonl").display()
        );
    }
    idx
}

/// Instances currently resident, least recently used first.
pub fn resident_instances() -> Vec<String> {
    registry()
        .lock()
        .map(|r| r.order.clone())
        .unwrap_or_default()
}

/// Drop an instance from the resident set (used on delete).
/// Drop a preset's shipped memories from the process cache, and from disk.
///
/// Called on uninstall. Without it the base stays in a process-static map that
/// nothing else can reach, so an agent still resident keeps recalling memories from a
/// pack that was removed — and the built snapshot stays on disk, loadable again by
/// any orphaned instance.
pub fn release_base(preset: &str, version: &str) {
    // Two installed packs can provide the same preset slug — the installer reports
    // that as a collision rather than refusing it — and they would share this
    // directory. Deleting it on one uninstall would take the other's shipped
    // memories with it.
    if crate::agent_packs::list()
        .iter()
        .any(|p| p.manifest.presets.iter().any(|s| s == preset))
    {
        log::debug!("memory: base {preset}@{version} is still provided by an installed pack");
        return;
    }
    let key = base_key(preset, version);
    if let Ok(mut b) = bases().write() {
        b.remove(&key);
    }
    let dir = crate::paths::memory_preset_dir(preset, version);
    if dir.is_dir()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        // Wasted space, not a failed uninstall.
        log::warn!(
            "memory: could not remove base {key} at {}: {e}",
            dir.display()
        );
    }
}

pub fn evict(instance_id: &str) {
    if let Ok(mut reg) = registry().lock() {
        reg.resident.remove(instance_id);
        reg.order.retain(|k| k != instance_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = r#"
{"kind":"Semantic","content":"Amy braises at 2:1 mirepoix to leek.","summary":"braise base","entity":"braising","importance":7.0,"tags":["technique"]}
{"kind":"Procedural","content":"Sear first. The fond is the dish.","summary":"sear first","entity":"braising","importance":8.0}
not json at all
{"content":"Salt early."}
"#;

    #[test]
    fn seed_parsing_skips_bad_lines_and_defaults_sensibly() {
        let (memories, skipped) = parse_seed_file(SEED);
        assert_eq!(memories.len(), 3);
        assert_eq!(
            skipped, 1,
            "one unreadable line, and it must not cost the rest"
        );

        assert_eq!(memories[0].summary, "braise base");
        assert_eq!(memories[0].entity.as_deref(), Some("braising"));
        assert_eq!(memories[0].importance, 7.0);
        assert_eq!(memories[0].source, Source::Seeded);
        assert_eq!(memories[1].kind, MemoryKind::Procedural);

        // A bare record still works: kind and importance fall back.
        assert_eq!(memories[2].kind, MemoryKind::Semantic);
        assert_eq!(memories[2].content, "Salt early.");
    }

    #[test]
    fn seeded_memories_get_distinct_ids() {
        let (memories, _) = parse_seed_file(SEED);
        let ids: HashSet<_> = memories.iter().map(|m| &m.id).collect();
        assert_eq!(ids.len(), memories.len());
    }

    #[test]
    fn base_key_is_version_scoped() {
        assert_eq!(base_key("amy-kitchen", "1.4.0"), "amy-kitchen@1.4.0");
        assert_ne!(
            base_key("amy-kitchen", "1.4.0"),
            base_key("amy-kitchen", "1.5.0")
        );
    }
}
