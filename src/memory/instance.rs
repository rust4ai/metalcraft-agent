//! Per-instance memory: a shared, immutable **base** plus a writable **delta**.
//!
//! ## Why two layers rather than a copy
//!
//! The obvious implementation of "an agent starts out knowing what its pack shipped"
//! is to copy the pack's memories into the new instance. Across twenty instances of
//! one preset that is twenty copies of every record and vector, and instance creation
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
        let json = serde_json::to_string_pretty(&Tombstones { ids })
            .map_err(|e| format!("failed to serialize tombstones: {e}"))?;
        // tmp + rename: a torn write must not lose the whole set.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
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

/// Build a preset's base index from a `memories.jsonl` and persist it as a snapshot.
///
/// Runs **once per `preset@version`** at pack install — not per instance — so twenty
/// agents share one embedding bill and one copy on disk.
pub fn build_base(preset: &str, version: &str, seed_file: &Path) -> Result<usize, String> {
    let contents = std::fs::read_to_string(seed_file)
        .map_err(|e| format!("failed to read {}: {e}", seed_file.display()))?;
    let (memories, skipped) = parse_seed_file(&contents);
    let count = memories.len();

    let mut idx = MemoryIndex::new();
    for m in memories {
        idx.insert_memory(m);
    }

    let dir = crate::paths::memory_preset_dir(preset, version);
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let snapshot = idx.snapshot(None, None);
    let json = serde_json::to_string(&snapshot)
        .map_err(|e| format!("failed to serialize base snapshot: {e}"))?;
    let path = dir.join("snapshot.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("failed to finalize {}: {e}", path.display()))?;

    if skipped > 0 {
        log::warn!("memory: {skipped} unreadable line(s) in {}", seed_file.display());
    }
    log::info!("memory: built base {preset}@{version} with {count} memories");
    // Drop any cached copy so a rebuild is picked up.
    bases().write().ok().map(|mut b| b.remove(&base_key(preset, version)));
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
    let contents = std::fs::read_to_string(&path)
        .map_err(|_| format!("no base memory built for {key}"))?;
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
    bases().read().map(|b| b.keys().cloned().collect()).unwrap_or_default()
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
pub fn handle_for(
    instance_id: &str,
    base: Option<(&str, &str)>,
) -> Result<InstanceMemory, String> {
    let mut reg = registry().lock().map_err(|_| "memory registry poisoned".to_string())?;

    if let Some(existing) = reg.resident.get(instance_id).cloned() {
        reg.order.retain(|k| k != instance_id);
        reg.order.push(instance_id.to_string());
        return Ok(existing);
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
        delta: Arc::new(RwLock::new(MemoryIndex::new())),
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

/// Instances currently resident, least recently used first.
pub fn resident_instances() -> Vec<String> {
    registry().lock().map(|r| r.order.clone()).unwrap_or_default()
}

/// Drop an instance from the resident set (used on delete).
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
        assert_eq!(skipped, 1, "one unreadable line, and it must not cost the rest");

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
        assert_ne!(base_key("amy-kitchen", "1.4.0"), base_key("amy-kitchen", "1.5.0"));
    }
}
