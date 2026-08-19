//! Core memory types: the record, the graph edge, and the log event.
//!
//! Everything here is `serde`-round-trippable, because the on-disk format *is*
//! these types — an append-only log of [`Event`]s plus a periodic [`Snapshot`].
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What kind of thing a memory is. The vocabulary is closed on purpose: recall
/// boosts, decay exemptions, and the dream's abstraction stage all switch on it,
/// so an open-ended `tag` string would push those decisions into prompt text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Something that happened: a conversation, a task, an event.
    Episodic,
    /// A durable fact about the world or the user's setup.
    Semantic,
    /// A method that worked — how to do something here.
    Procedural,
    /// How the user wants things done.
    Preference,
    /// A person, repo, service, or other named thing.
    Entity,
    /// A pattern the agent noticed across other memories (dream output).
    Insight,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
            Self::Procedural => "procedural",
            Self::Preference => "preference",
            Self::Entity => "entity",
            Self::Insight => "insight",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "episodic" => Self::Episodic,
            "semantic" | "fact" => Self::Semantic,
            "procedural" | "procedure" => Self::Procedural,
            "preference" | "pref" => Self::Preference,
            "entity" => Self::Entity,
            "insight" => Self::Insight,
            _ => return None,
        })
    }

    pub const ALL: [MemoryKind; 6] = [
        Self::Episodic,
        Self::Semantic,
        Self::Procedural,
        Self::Preference,
        Self::Entity,
        Self::Insight,
    ];

    /// Kinds never archived automatically by the decay pass. A preference learned
    /// once and never re-read is exactly what must not be forgotten.
    pub fn exempt_from_decay(&self) -> bool {
        matches!(self, Self::Preference | Self::Procedural | Self::Entity)
    }
}

/// Where a memory came from. Drives confidence defaults and keeps dream output
/// out of the dream's own abstraction input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Distilled from a completed turn.
    Turn,
    /// Rescued from a context-compaction summary.
    Compaction,
    /// Written by the agent through `mem_remember`.
    Tool,
    /// Produced by the nightly dream.
    Dream,
    /// Written by a human through the API.
    User,
    /// Shipped with an agent pack and loaded into a preset's shared base layer.
    /// Never written by a running agent — a base index is immutable, so this
    /// marks "the author gave me this" as distinct from "I learned it".
    Seeded,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Compaction => "compaction",
            Self::Tool => "tool",
            Self::Dream => "dream",
            Self::User => "user",
            Self::Seeded => "seeded",
        }
    }
}

/// A single memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub kind: MemoryKind,
    pub content: String,
    /// Short form, preferred when filling a recall budget. Empty until the dream
    /// writes one.
    #[serde(default)]
    pub summary: String,
    /// Canonical key for entity linking (`"metalcraft-gateway"`, `"Andrew"`).
    #[serde(default)]
    pub entity: Option<String>,
    /// Free-form labels. Authored memories carry the tags their entry was written
    /// with; captured ones are usually untagged. Covered by the search index, so a
    /// tag is a real recall handle rather than decoration.
    #[serde(default)]
    pub tags: Vec<String>,
    /// 0..10. Decayed nightly, boosted by access.
    pub importance: f32,
    /// 0..1. Lowered when the dream finds a contradiction.
    pub confidence: f32,
    /// Exempt from all automatic archival.
    #[serde(default)]
    pub pinned: bool,
    pub source: Source,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub episode_id: Option<String>,
    /// sha256 of the normalized content — the exact-duplicate guard.
    pub content_hash: String,
    /// When the remembered thing was true, if that differs from when it was written.
    #[serde(default)]
    pub occurred_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    #[serde(default)]
    pub access_count: u32,
    /// Set when the dream merges this into another memory. Excluded from recall.
    #[serde(default)]
    pub superseded_by: Option<String>,
    /// Soft delete. Excluded from recall; purged later by the decay pass.
    #[serde(default)]
    pub archived_at: Option<DateTime<Utc>>,
}

/// Normalized sha256 of a memory's content — whitespace-collapsed and
/// lowercased, so trivially-reformatted duplicates collide.
pub fn content_hash(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    hex::encode(digest)
}

impl Memory {
    /// A new memory with sensible defaults; callers override what they know.
    pub fn new(kind: MemoryKind, content: impl Into<String>, source: Source) -> Self {
        let content = content.into();
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            content_hash: content_hash(&content),
            content,
            summary: String::new(),
            entity: None,
            tags: Vec::new(),
            importance: 5.0,
            confidence: 1.0,
            pinned: false,
            source,
            chat_id: None,
            persona: None,
            episode_id: None,
            occurred_at: None,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            access_count: 0,
            superseded_by: None,
            archived_at: None,
        }
    }

    /// Whether this memory can be returned by recall.
    pub fn is_live(&self) -> bool {
        self.archived_at.is_none() && self.superseded_by.is_none()
    }

    /// The text a recall block should show: the summary when the dream has
    /// written one, else the raw content.
    pub fn display_text(&self) -> &str {
        if self.summary.trim().is_empty() { &self.content } else { &self.summary }
    }

    /// The text the search index covers.
    pub fn indexable(&self) -> String {
        let mut out = format!("{} {}", self.content, self.summary);
        if let Some(e) = &self.entity {
            out.push(' ');
            out.push_str(e);
        }
        for t in &self.tags {
            out.push(' ');
            out.push_str(t);
        }
        out
    }
}

/// A typed, directed, weighted edge between two memories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    RelatesTo,
    Supersedes,
    Contradicts,
    CausedBy,
    PartOf,
    DerivedFrom,
    AboutEntity,
}

impl LinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RelatesTo => "relates_to",
            Self::Supersedes => "supersedes",
            Self::Contradicts => "contradicts",
            Self::CausedBy => "caused_by",
            Self::PartOf => "part_of",
            Self::DerivedFrom => "derived_from",
            Self::AboutEntity => "about_entity",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "relates_to" | "related" => Self::RelatesTo,
            "supersedes" => Self::Supersedes,
            "contradicts" => Self::Contradicts,
            "caused_by" => Self::CausedBy,
            "part_of" => Self::PartOf,
            "derived_from" => Self::DerivedFrom,
            "about_entity" => Self::AboutEntity,
            _ => return None,
        })
    }

    /// Links that make their target load-bearing — a memory on the receiving end
    /// of one of these is never auto-archived, because something else depends on
    /// it for provenance.
    pub fn protects_target(&self) -> bool {
        matches!(self, Self::DerivedFrom | Self::Supersedes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub src: String,
    pub dst: String,
    pub kind: LinkKind,
    #[serde(default = "one")]
    pub weight: f32,
    /// `"tool"` | `"dream"` | `"heuristic"`.
    pub created_by: String,
}

fn one() -> f32 {
    1.0
}

/// One line of the append-only log.
///
/// The log exists rather than a rewritten JSON array (the shape `scheduled_tasks`
/// uses) because recall touches `last_accessed_at` on every turn, and rewriting
/// the whole store for that would be O(n) per turn. Appending is O(1); the dream
/// folds the log into a fresh snapshot nightly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Event {
    Upsert { seq: u64, at: DateTime<Utc>, memory: Box<Memory> },
    Link { seq: u64, at: DateTime<Utc>, link: Link },
    Unlink { seq: u64, at: DateTime<Utc>, src: String, dst: String, kind: LinkKind },
    /// Batched access bookkeeping from a recall.
    Touch { seq: u64, at: DateTime<Utc>, ids: Vec<String> },
    Archive { seq: u64, at: DateTime<Utc>, id: String },
    Purge { seq: u64, at: DateTime<Utc>, id: String },
}

impl Event {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Upsert { seq, .. }
            | Self::Link { seq, .. }
            | Self::Unlink { seq, .. }
            | Self::Touch { seq, .. }
            | Self::Archive { seq, .. }
            | Self::Purge { seq, .. } => *seq,
        }
    }
}

/// The periodic full-state file the dream writes, so boot doesn't replay all
/// history. `embed_model`/`embed_dims` are recorded here (unused until vectors
/// land in Phase 2) so a model change can be *detected* rather than silently
/// comparing incompatible vectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub written_at: DateTime<Utc>,
    #[serde(default)]
    pub embed_model: Option<String>,
    #[serde(default)]
    pub embed_dims: Option<usize>,
    pub memories: Vec<Memory>,
    #[serde(default)]
    pub links: Vec<Link>,
}

/// What `mem_stats` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub total: usize,
    pub live: usize,
    pub archived: usize,
    pub superseded: usize,
    pub pinned: usize,
    pub by_kind: Vec<(String, usize)>,
    pub links: usize,
    pub seq: u64,
    pub log_events: u64,
    pub approx_bytes: usize,
    /// How many live memories have an embedding — the coverage number that says
    /// whether hybrid recall is actually working.
    pub vectors: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_ignores_whitespace_and_case() {
        assert_eq!(content_hash("Hello   world"), content_hash("hello world"));
        assert_eq!(content_hash("a\n b"), content_hash("A B"));
        assert_ne!(content_hash("hello world"), content_hash("hello worlds"));
    }

    #[test]
    fn kind_round_trips() {
        for k in MemoryKind::ALL {
            assert_eq!(MemoryKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(MemoryKind::parse("FACT"), Some(MemoryKind::Semantic));
        assert_eq!(MemoryKind::parse("nonsense"), None);
    }

    #[test]
    fn link_kind_round_trips() {
        for k in [
            LinkKind::RelatesTo, LinkKind::Supersedes, LinkKind::Contradicts,
            LinkKind::CausedBy, LinkKind::PartOf, LinkKind::DerivedFrom, LinkKind::AboutEntity,
        ] {
            assert_eq!(LinkKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn new_memory_is_live_and_hashed() {
        let m = Memory::new(MemoryKind::Semantic, "the sky is blue", Source::Tool);
        assert!(m.is_live());
        assert_eq!(m.content_hash, content_hash("the sky is blue"));
        assert_eq!(m.display_text(), "the sky is blue");
    }

    #[test]
    fn summary_wins_for_display_when_present() {
        let mut m = Memory::new(MemoryKind::Episodic, "a very long story", Source::Turn);
        m.summary = "short".into();
        assert_eq!(m.display_text(), "short");
    }

    #[test]
    fn decay_exemptions() {
        assert!(MemoryKind::Preference.exempt_from_decay());
        assert!(MemoryKind::Procedural.exempt_from_decay());
        assert!(MemoryKind::Entity.exempt_from_decay());
        assert!(!MemoryKind::Episodic.exempt_from_decay());
        assert!(!MemoryKind::Insight.exempt_from_decay());
    }

    #[test]
    fn events_expose_their_seq() {
        let m = Memory::new(MemoryKind::Semantic, "x", Source::Tool);
        let e = Event::Upsert { seq: 7, at: Utc::now(), memory: Box::new(m) };
        assert_eq!(e.seq(), 7);
        let t = Event::Touch { seq: 9, at: Utc::now(), ids: vec![] };
        assert_eq!(t.seq(), 9);
    }
}
