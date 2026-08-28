//! The in-memory store: a map of memories, a BM25 inverted index over their
//! text, and the link adjacency.
//!
//! This is the authoritative store — the disk log is how it survives a restart,
//! not where it is queried from. That inverts the usual arrangement, and it is
//! what lets the search be a plain Rust scan with no query language and no new
//! dependency. At the sizes this is bounded to (§ the RAM ceiling in the plan)
//! scoring runs in single-digit milliseconds because every posting list is
//! already resident.
use std::collections::HashMap;

use chrono::Utc;

use super::types::{Event, Link, LinkKind, Memory, MemoryKind, Snapshot, Stats};

/// BM25 term-frequency saturation. 1.2 is the standard default.
const K1: f32 = 1.2;
/// BM25 length normalization. 0.75 is the standard default.
const B: f32 = 0.75;

/// Words carrying no retrieval signal. Deliberately short — an aggressive stop
/// list hurts recall on short memories, where almost every word matters.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "her", "was", "one",
    "our", "out", "his", "has", "had", "him", "she", "its", "who", "did", "yes", "with", "that",
    "this", "from", "they", "will", "would", "there", "their", "what", "about", "which", "when",
    "were", "your", "been", "have", "into", "than", "them", "then", "some", "just", "also",
];

/// Split text into index terms: lowercase, alphanumeric runs, ≥2 chars, no stop
/// words, lightly stemmed. The query side calls the same function, so index and
/// query always agree on what a term is.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| stem(&t.to_lowercase()))
        .filter(|t| t.len() >= 2 && !STOP_WORDS.contains(&t.as_str()))
        .collect()
}

/// A deliberately conservative suffix stripper. Full Porter stemming would be
/// more aggressive than this corpus wants: memories are short and often contain
/// identifiers, where over-stemming destroys precision.
fn stem(word: &str) -> String {
    for suffix in ["ing", "ed", "ly", "es", "s"] {
        // Keep a 3-char floor so "was"→"wa" and "les"→"l" can't happen.
        if word.len() > suffix.len() + 3 && word.ends_with(suffix) {
            return word[..word.len() - suffix.len()].to_string();
        }
    }
    word.to_string()
}

/// A search hit: the memory id and its BM25 score.
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: String,
    pub score: f32,
}

#[derive(Default)]
pub struct MemoryIndex {
    /// Highest sequence number applied. Every new event takes `seq + 1`.
    pub seq: u64,
    memories: HashMap<String, Memory>,
    /// term → [(memory id, term frequency in that memory)]
    inverted: HashMap<String, Vec<(String, u32)>>,
    /// memory id → number of index terms (BM25 length normalization)
    doc_len: HashMap<String, u32>,
    out_links: HashMap<String, Vec<Link>>,
    in_links: HashMap<String, Vec<Link>>,
    /// content hash → memory id, for the exact-duplicate guard
    hashes: HashMap<String, String>,
}

impl MemoryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild from a snapshot, discarding any current state.
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        let mut idx = Self::new();
        for m in snapshot.memories {
            idx.insert_memory(m);
        }
        for l in snapshot.links {
            idx.insert_link(l);
        }
        idx.seq = snapshot.seq;
        idx
    }

    /// Apply one log event.
    ///
    /// Every variant is idempotent under replay **except** `Touch`, which
    /// increments `access_count`. Re-applying the tail after a crash can
    /// therefore over-count accesses slightly; that number only feeds a decay
    /// boost, so the cost is a memory surviving marginally longer than it earned.
    pub fn apply(&mut self, event: Event) {
        let seq = event.seq();
        match event {
            Event::Upsert { memory, .. } => self.insert_memory(*memory),
            Event::Link { link, .. } => self.insert_link(link),
            Event::Unlink { src, dst, kind, .. } => self.remove_link(&src, &dst, kind),
            Event::Touch { ids, at, .. } => {
                for id in ids {
                    if let Some(m) = self.memories.get_mut(&id) {
                        m.last_accessed_at = at;
                        m.access_count = m.access_count.saturating_add(1);
                    }
                }
            }
            Event::Archive { id, at, .. } => {
                if let Some(m) = self.memories.get_mut(&id) {
                    m.archived_at = Some(at);
                    m.updated_at = at;
                }
            }
            Event::Purge { id, .. } => self.purge(&id),
        }
        self.seq = self.seq.max(seq);
    }

    /// Insert or replace a memory, keeping the inverted index consistent.
    pub fn insert_memory(&mut self, memory: Memory) {
        // Replacing an existing id means retiring its old postings first,
        // otherwise stale terms keep matching forever.
        if let Some(old) = self.memories.get(&memory.id) {
            let old = old.clone();
            self.unindex(&old);
            self.hashes.remove(&old.content_hash);
        }
        self.index(&memory);
        self.hashes
            .insert(memory.content_hash.clone(), memory.id.clone());
        self.memories.insert(memory.id.clone(), memory);
    }

    fn index(&mut self, m: &Memory) {
        let terms = tokenize(&m.indexable());
        self.doc_len.insert(m.id.clone(), terms.len() as u32);
        let mut freq: HashMap<String, u32> = HashMap::new();
        for t in terms {
            *freq.entry(t).or_insert(0) += 1;
        }
        for (term, tf) in freq {
            self.inverted
                .entry(term)
                .or_default()
                .push((m.id.clone(), tf));
        }
    }

    fn unindex(&mut self, m: &Memory) {
        for term in tokenize(&m.indexable()) {
            if let Some(postings) = self.inverted.get_mut(&term) {
                postings.retain(|(id, _)| id != &m.id);
                if postings.is_empty() {
                    self.inverted.remove(&term);
                }
            }
        }
        self.doc_len.remove(&m.id);
    }

    pub fn insert_link(&mut self, link: Link) {
        let dup = self
            .out_links
            .get(&link.src)
            .is_some_and(|v| v.iter().any(|l| l.dst == link.dst && l.kind == link.kind));
        if dup {
            return;
        }
        self.out_links
            .entry(link.src.clone())
            .or_default()
            .push(link.clone());
        self.in_links
            .entry(link.dst.clone())
            .or_default()
            .push(link);
    }

    pub fn remove_link(&mut self, src: &str, dst: &str, kind: LinkKind) {
        if let Some(v) = self.out_links.get_mut(src) {
            v.retain(|l| !(l.dst == dst && l.kind == kind));
        }
        if let Some(v) = self.in_links.get_mut(dst) {
            v.retain(|l| !(l.src == src && l.kind == kind));
        }
    }

    /// Remove a memory and everything pointing at it.
    pub fn purge(&mut self, id: &str) {
        if let Some(m) = self.memories.remove(id) {
            self.unindex(&m);
            self.hashes.remove(&m.content_hash);
        }
        self.out_links.remove(id);
        self.in_links.remove(id);
        for v in self.out_links.values_mut() {
            v.retain(|l| l.dst != id);
        }
        for v in self.in_links.values_mut() {
            v.retain(|l| l.src != id);
        }
    }


    /// One-hop graph expansion from a seed set, ranked by total edge weight back
    /// to the seeds. Query-aware: this surfaces what the matches are *connected
    /// to*, not whatever is globally most connected.
    pub fn graph_expand(&self, seeds: &[String], limit: usize) -> Vec<Hit> {
        if seeds.is_empty() {
            return Vec::new();
        }
        let seed_set: std::collections::HashSet<&str> = seeds.iter().map(|s| s.as_str()).collect();
        let mut scores: HashMap<String, f32> = HashMap::new();
        for seed in seeds {
            for l in self
                .links_from(seed)
                .iter()
                .chain(self.links_to(seed).iter())
            {
                let other = if l.src == *seed { &l.dst } else { &l.src };
                if seed_set.contains(other.as_str()) {
                    continue;
                }
                match self.memories.get(other) {
                    Some(m) if m.is_live() => {
                        *scores.entry(other.clone()).or_insert(0.0) += l.weight
                    }
                    _ => {}
                }
            }
        }
        let mut hits: Vec<Hit> = scores
            .into_iter()
            .map(|(id, score)| Hit { id, score })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(limit);
        hits
    }

    pub fn get(&self, id: &str) -> Option<&Memory> {
        self.memories.get(id)
    }

    /// The memory with this exact content, if one already exists.
    pub fn by_hash(&self, hash: &str) -> Option<&Memory> {
        self.hashes.get(hash).and_then(|id| self.memories.get(id))
    }

    pub fn len(&self) -> usize {
        self.memories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.memories.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Memory> {
        self.memories.values()
    }

    pub fn links_from(&self, id: &str) -> &[Link] {
        self.out_links.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn links_to(&self, id: &str) -> &[Link] {
        self.in_links.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// BM25 search over live memories. Returns up to `limit` hits, best first.
    ///
    /// `kind` optionally restricts to one kind. Archived and superseded memories
    /// are never returned: a merged-away memory reappearing in recall is exactly
    /// the confusion the merge existed to remove.
    pub fn search(&self, query: &str, limit: usize, kind: Option<MemoryKind>) -> Vec<Hit> {
        let terms = tokenize(query);
        if terms.is_empty() {
            return Vec::new();
        }

        let live: Vec<&Memory> = self.memories.values().filter(|m| m.is_live()).collect();
        let n = live.len() as f32;
        if n == 0.0 {
            return Vec::new();
        }
        let total_len: u32 = live.iter().filter_map(|m| self.doc_len.get(&m.id)).sum();
        let avgdl = (total_len as f32 / n).max(1.0);

        let mut scores: HashMap<&str, f32> = HashMap::new();
        for term in &terms {
            let Some(postings) = self.inverted.get(term) else {
                continue;
            };
            // df counts postings including archived docs. The skew is tiny and
            // uniform across terms, and recomputing an exact live df per query
            // would cost a full scan per term for no ranking benefit.
            let df = postings.len() as f32;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (id, tf) in postings {
                let Some(m) = self.memories.get(id) else {
                    continue;
                };
                if !m.is_live() {
                    continue;
                }
                if let Some(k) = kind
                    && m.kind != k
                {
                    continue;
                }
                let dl = *self.doc_len.get(id).unwrap_or(&1) as f32;
                let tf = *tf as f32;
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl);
                *scores.entry(m.id.as_str()).or_insert(0.0) += idf * (tf * (K1 + 1.0)) / denom;
            }
        }

        let mut hits: Vec<Hit> = scores
            .into_iter()
            .map(|(id, score)| Hit {
                id: id.to_string(),
                score,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(limit);
        hits
    }

    /// Record that these memories were surfaced. Returns the ids that existed, so
    /// the caller only logs a `Touch` for real ones.
    pub fn touch(&mut self, ids: &[String]) -> Vec<String> {
        let now = Utc::now();
        let mut touched = Vec::new();
        for id in ids {
            if let Some(m) = self.memories.get_mut(id) {
                m.last_accessed_at = now;
                m.access_count = m.access_count.saturating_add(1);
                touched.push(id.clone());
            }
        }
        touched
    }

    /// Materialize current state for the snapshot writer.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            seq: self.seq,
            written_at: Utc::now(),
            memories: self.memories.values().cloned().collect(),
            links: self.out_links.values().flatten().cloned().collect(),
        }
    }

    pub fn stats(&self, log_events: u64) -> Stats {
        let mut by_kind: Vec<(String, usize)> = MemoryKind::ALL
            .iter()
            .map(|k| {
                (
                    k.as_str().to_string(),
                    self.memories.values().filter(|m| m.kind == *k).count(),
                )
            })
            .filter(|(_, n)| *n > 0)
            .collect();
        by_kind.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

        let approx_bytes = self
            .memories
            .values()
            .map(|m| m.content.len() + m.summary.len() + 256)
            .sum::<usize>()
            + self
                .inverted
                .iter()
                .map(|(t, p)| t.len() + p.len() * 48)
                .sum::<usize>();

        Stats {
            total: self.memories.len(),
            live: self.memories.values().filter(|m| m.is_live()).count(),
            archived: self
                .memories
                .values()
                .filter(|m| m.archived_at.is_some())
                .count(),
            superseded: self
                .memories
                .values()
                .filter(|m| m.superseded_by.is_some())
                .count(),
            pinned: self.memories.values().filter(|m| m.pinned).count(),
            by_kind,
            links: self.out_links.values().map(|v| v.len()).sum(),
            seq: self.seq,
            log_events,
            approx_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::{Memory, Source};

    fn mem(content: &str, kind: MemoryKind) -> Memory {
        Memory::new(kind, content, Source::Tool)
    }

    #[test]
    fn tokenize_drops_stop_words_and_short_tokens() {
        let t = tokenize("The quick brown fox, a dog!");
        assert!(!t.contains(&"the".to_string()));
        assert!(!t.contains(&"a".to_string()));
        assert!(t.contains(&"quick".to_string()));
        assert!(t.contains(&"brown".to_string()));
    }

    #[test]
    fn stemming_has_a_length_floor() {
        // "was" must not become "wa"; "running" may become "runn".
        assert_eq!(stem("was"), "was");
        assert_eq!(stem("les"), "les");
        assert_eq!(stem("running"), "runn");
        assert_eq!(stem("deployed"), "deploy");
    }

    #[test]
    fn query_and_index_agree_after_stemming() {
        let mut idx = MemoryIndex::new();
        idx.insert_memory(mem("deployed the gateway yesterday", MemoryKind::Episodic));
        // "deploying" and "deployed" both stem to "deploy".
        assert_eq!(idx.search("deploying", 5, None).len(), 1);
    }

    #[test]
    fn search_ranks_the_better_match_first() {
        let mut idx = MemoryIndex::new();
        idx.insert_memory(mem(
            "rust is the language we use for pod services",
            MemoryKind::Semantic,
        ));
        idx.insert_memory(mem(
            "rust rust rust everywhere in this codebase",
            MemoryKind::Semantic,
        ));
        idx.insert_memory(mem(
            "completely unrelated note about weather",
            MemoryKind::Semantic,
        ));

        let hits = idx.search("rust", 10, None);
        assert_eq!(hits.len(), 2, "the unrelated memory must not match");
        let top = idx.get(&hits[0].id).unwrap();
        assert!(
            top.content.starts_with("rust rust rust"),
            "term frequency should win"
        );
    }

    #[test]
    fn search_can_filter_by_kind() {
        let mut idx = MemoryIndex::new();
        idx.insert_memory(mem("prefers rust over go", MemoryKind::Preference));
        idx.insert_memory(mem("rust compiles slowly", MemoryKind::Semantic));
        assert_eq!(idx.search("rust", 10, None).len(), 2);
        let prefs = idx.search("rust", 10, Some(MemoryKind::Preference));
        assert_eq!(prefs.len(), 1);
        assert_eq!(idx.get(&prefs[0].id).unwrap().kind, MemoryKind::Preference);
    }

    #[test]
    fn archived_and_superseded_are_never_returned() {
        let mut idx = MemoryIndex::new();
        let mut archived = mem("archived thing about badgers", MemoryKind::Episodic);
        archived.archived_at = Some(Utc::now());
        let mut merged = mem("superseded thing about badgers", MemoryKind::Episodic);
        merged.superseded_by = Some("other".into());
        let live = mem("live thing about badgers", MemoryKind::Episodic);
        idx.insert_memory(archived);
        idx.insert_memory(merged);
        idx.insert_memory(live.clone());

        let hits = idx.search("badgers", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, live.id);
    }

    #[test]
    fn empty_query_and_empty_store_return_nothing() {
        let mut idx = MemoryIndex::new();
        assert!(idx.search("anything", 5, None).is_empty());
        idx.insert_memory(mem("something", MemoryKind::Semantic));
        assert!(idx.search("", 5, None).is_empty());
        assert!(
            idx.search("the and but", 5, None).is_empty(),
            "all-stop-word query"
        );
    }

    #[test]
    fn replacing_a_memory_retires_its_old_terms() {
        let mut idx = MemoryIndex::new();
        let mut m = mem("original content mentioning zebras", MemoryKind::Semantic);
        idx.insert_memory(m.clone());
        assert_eq!(idx.search("zebras", 5, None).len(), 1);

        m.content = "rewritten content mentioning giraffes".into();
        m.content_hash = crate::memory::types::content_hash(&m.content);
        idx.insert_memory(m);

        assert!(
            idx.search("zebras", 5, None).is_empty(),
            "stale term must not match"
        );
        assert_eq!(idx.search("giraffes", 5, None).len(), 1);
        assert_eq!(idx.len(), 1, "replacement must not duplicate");
    }

    #[test]
    fn purge_removes_the_memory_and_its_edges() {
        let mut idx = MemoryIndex::new();
        let a = mem("alpha about okapi", MemoryKind::Semantic);
        let b = mem("beta about okapi", MemoryKind::Semantic);
        let (aid, bid) = (a.id.clone(), b.id.clone());
        idx.insert_memory(a);
        idx.insert_memory(b);
        idx.insert_link(Link {
            src: aid.clone(),
            dst: bid.clone(),
            kind: LinkKind::RelatesTo,
            weight: 1.0,
            created_by: "test".into(),
        });
        assert_eq!(idx.links_from(&aid).len(), 1);
        assert_eq!(idx.links_to(&bid).len(), 1);

        idx.purge(&bid);
        assert!(idx.get(&bid).is_none());
        assert_eq!(idx.search("okapi", 5, None).len(), 1);
        assert!(
            idx.links_from(&aid).is_empty(),
            "dangling edge must be swept"
        );
    }

    #[test]
    fn duplicate_links_are_not_stored_twice() {
        let mut idx = MemoryIndex::new();
        let a = mem("a", MemoryKind::Semantic);
        let b = mem("b", MemoryKind::Semantic);
        let (aid, bid) = (a.id.clone(), b.id.clone());
        idx.insert_memory(a);
        idx.insert_memory(b);
        let link = Link {
            src: aid.clone(),
            dst: bid.clone(),
            kind: LinkKind::RelatesTo,
            weight: 1.0,
            created_by: "test".into(),
        };
        idx.insert_link(link.clone());
        idx.insert_link(link);
        assert_eq!(idx.links_from(&aid).len(), 1);
    }

    #[test]
    fn hash_lookup_finds_an_exact_duplicate() {
        let mut idx = MemoryIndex::new();
        let m = mem("the gateway proxies embeddings", MemoryKind::Semantic);
        let hash = m.content_hash.clone();
        let id = m.id.clone();
        idx.insert_memory(m);
        assert_eq!(idx.by_hash(&hash).map(|m| m.id.clone()), Some(id));
        // Reformatted duplicate hashes the same.
        assert!(
            idx.by_hash(&crate::memory::types::content_hash(
                "The   Gateway\nproxies embeddings"
            ))
            .is_some()
        );
    }

    #[test]
    fn snapshot_round_trips_through_the_index() {
        let mut idx = MemoryIndex::new();
        let a = mem("first about pangolins", MemoryKind::Semantic);
        let b = mem("second about pangolins", MemoryKind::Episodic);
        let (aid, bid) = (a.id.clone(), b.id.clone());
        idx.insert_memory(a);
        idx.insert_memory(b);
        idx.insert_link(Link {
            src: aid,
            dst: bid,
            kind: LinkKind::RelatesTo,
            weight: 1.0,
            created_by: "test".into(),
        });
        idx.seq = 11;

        let snap = idx.snapshot();
        let rebuilt = MemoryIndex::from_snapshot(snap);
        assert_eq!(rebuilt.len(), 2);
        assert_eq!(rebuilt.seq, 11);
        assert_eq!(
            rebuilt.search("pangolins", 5, None).len(),
            2,
            "index rebuilds from snapshot"
        );
        assert_eq!(rebuilt.stats(0).links, 1);
    }

    #[test]
    fn apply_is_idempotent_for_upsert() {
        let mut idx = MemoryIndex::new();
        let m = mem("idempotent content", MemoryKind::Semantic);
        let ev = Event::Upsert {
            seq: 1,
            at: Utc::now(),
            memory: Box::new(m),
        };
        idx.apply(ev.clone());
        idx.apply(ev);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.seq, 1);
    }

    #[test]
    fn apply_archive_then_search_excludes_it() {
        let mut idx = MemoryIndex::new();
        let m = mem("soon to be archived quokka", MemoryKind::Episodic);
        let id = m.id.clone();
        idx.apply(Event::Upsert {
            seq: 1,
            at: Utc::now(),
            memory: Box::new(m),
        });
        assert_eq!(idx.search("quokka", 5, None).len(), 1);
        idx.apply(Event::Archive {
            seq: 2,
            at: Utc::now(),
            id: id.clone(),
        });
        assert!(idx.search("quokka", 5, None).is_empty());
        assert!(idx.get(&id).is_some(), "archive is soft — the record stays");
    }

    #[test]
    fn touch_bumps_access_count() {
        let mut idx = MemoryIndex::new();
        let m = mem("touched", MemoryKind::Semantic);
        let id = m.id.clone();
        idx.insert_memory(m);
        let touched = idx.touch(&[id.clone(), "missing".into()]);
        assert_eq!(touched, vec![id.clone()], "unknown ids are not reported");
        assert_eq!(idx.get(&id).unwrap().access_count, 1);
    }

    #[test]
    fn stats_counts_by_state() {
        let mut idx = MemoryIndex::new();
        let mut pinned = mem("pinned pref", MemoryKind::Preference);
        pinned.pinned = true;
        let mut archived = mem("archived note", MemoryKind::Episodic);
        archived.archived_at = Some(Utc::now());
        idx.insert_memory(pinned);
        idx.insert_memory(archived);
        idx.insert_memory(mem("plain fact", MemoryKind::Semantic));

        let s = idx.stats(7);
        assert_eq!(s.total, 3);
        assert_eq!(s.live, 2);
        assert_eq!(s.archived, 1);
        assert_eq!(s.pinned, 1);
        assert_eq!(s.log_events, 7);
        assert!(s.approx_bytes > 0);
    }
}
