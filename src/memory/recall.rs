//! Hybrid recall: keyword, vector, and graph signals fused into one ranking.
//!
//! Three retrievers see different things. BM25 is precise on exact wording and
//! useless on paraphrase. Vector search is the reverse. The graph sees neither —
//! it surfaces what the matches are *connected to*, which is often the thing you
//! actually needed. Fusing them beats any one alone, and beats a weighted sum,
//! because the three produce scores on incomparable scales (a BM25 score of 8 and
//! a cosine of 0.8 mean nothing to each other).
//!
//! So this uses **Reciprocal Rank Fusion**: each retriever contributes
//! `1 / (k + rank)`, and only the *ordering* within each list matters. It needs
//! no score normalization and no per-corpus tuning, which is exactly right for a
//! system whose corpus starts empty and grows to whatever it grows to.
//!
//! The last step is a **token budget rather than a top-k**. A flat `LIMIT 15`
//! either wastes context on fifteen one-liners or blows it on fifteen essays;
//! filling to a token budget spends exactly what was allotted.
use std::collections::HashMap;

use super::index::{Hit, MemoryIndex};
use super::types::{Memory, MemoryKind};

/// RRF's smoothing constant. 60 is the value from the original paper and the de
/// facto default; it flattens the difference between ranks 1 and 2 enough that a
/// single retriever cannot dominate the fusion.
const RRF_K: f32 = 60.0;

/// How many candidates each retriever contributes before fusion. Generous
/// relative to the final result count, because fusion's whole value is in
/// reranking across lists — truncating each list too early throws away the
/// agreement signal it exists to find.
const CANDIDATES: usize = 30;
const GRAPH_CANDIDATES: usize = 20;

/// Which retrievers to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// All three, fused. The default.
    Hybrid,
    /// BM25 only — deterministic, no network, no embedding cost.
    Text,
    /// Vector only. Mostly a debugging lens.
    Vector,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "hybrid" | "" => Self::Hybrid,
            "text" | "fts" | "keyword" | "bm25" => Self::Text,
            "vector" | "semantic" | "embedding" => Self::Vector,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Text => "text",
            Self::Vector => "vector",
        }
    }
}

/// Which retrievers found a given memory, and where they ranked it. Surfaced so
/// "why did this come back?" is answerable without re-running the query.
#[derive(Debug, Clone, Default)]
pub struct Signals {
    pub text_rank: Option<usize>,
    pub vector_rank: Option<usize>,
    pub vector_similarity: Option<f32>,
    pub graph_rank: Option<usize>,
}

impl Signals {
    /// Compact provenance, e.g. `"text#1,vector#3"`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(r) = self.text_rank {
            parts.push(format!("text#{}", r + 1));
        }
        if let Some(r) = self.vector_rank {
            parts.push(format!("vector#{}", r + 1));
        }
        if let Some(r) = self.graph_rank {
            parts.push(format!("graph#{}", r + 1));
        }
        parts.join(",")
    }
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub memory: Memory,
    pub score: f32,
    pub signals: Signals,
}

#[derive(Debug, Clone)]
pub struct RecallOptions {
    pub mode: Mode,
    /// Hard cap on results, applied after the token budget.
    pub limit: usize,
    /// Approximate token budget for the returned set. `None` = no budget, use
    /// `limit` alone.
    pub token_budget: Option<usize>,
    pub kind: Option<MemoryKind>,
    /// Memories from this conversation are boosted.
    pub chat_id: Option<String>,
    /// Memories written under this persona are boosted.
    pub persona: Option<String>,
    /// Recall against this agent instance's two layers rather than the pod-global
    /// store. `None` keeps the pre-instance behaviour.
    pub instance_id: Option<String>,
    /// Share of the token budget reserved for what this agent *learned*, as opposed
    /// to what its pack shipped. The remainder goes to the base layer.
    ///
    /// The floor matters: an installed pack must never be able to crowd out the
    /// operator's own memories, so this is clamped to at least `MIN_LEARNED_SHARE`
    /// wherever a preset gets to influence it.
    pub learned_share: f32,
}

/// Default split: most of the budget to what this agent has learned, the rest to
/// what it was shipped with.
pub const DEFAULT_LEARNED_SHARE: f32 = 0.7;
/// A pack may ask for a different split, but never below this.
pub const MIN_LEARNED_SHARE: f32 = 0.4;

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Hybrid,
            limit: 10,
            token_budget: None,
            kind: None,
            chat_id: None,
            persona: None,
            instance_id: None,
            learned_share: DEFAULT_LEARNED_SHARE,
        }
    }
}

/// Same chars-per-token heuristic `context::estimate_tokens` uses, so the recall
/// budget and the compaction threshold speak the same units.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 1
}

/// Run the full pipeline against an already-locked index.
///
/// Sync and pure given `(index, query, query_vec)`, which is what makes the
/// ranking testable without a network call or a global.
pub fn search_index(
    idx: &MemoryIndex,
    query: &str,
    query_vec: Option<&[f32]>,
    opts: &RecallOptions,
) -> Vec<Scored> {
    let text_hits = match opts.mode {
        Mode::Hybrid | Mode::Text => idx.search(query, CANDIDATES, opts.kind),
        Mode::Vector => Vec::new(),
    };
    let vector_hits = match (opts.mode, query_vec) {
        (Mode::Hybrid | Mode::Vector, Some(v)) => idx.vector_search(v, CANDIDATES, opts.kind),
        _ => Vec::new(),
    };

    // Graph expansion seeds from what the other two found, so it stays tied to
    // the query rather than surfacing the globally best-connected memories.
    let graph_hits = if opts.mode == Mode::Hybrid {
        let seeds: Vec<String> = text_hits
            .iter()
            .chain(vector_hits.iter())
            .map(|h| h.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .take(50)
            .collect();
        idx.graph_expand(&seeds, GRAPH_CANDIDATES)
    } else {
        Vec::new()
    };

    let fused = fuse(idx, &text_hits, &vector_hits, &graph_hits, opts);
    apply_budget(fused, opts)
}

/// Recall across an agent's two layers: what it learned, and what its pack shipped.
///
/// Each layer is ranked independently — scores from different corpora are not
/// comparable, and pooling them lets a large base drown a small delta — then each
/// gets its own slice of the token budget. Delta wins ties by construction: a
/// materialized edit shadows the base record it came from, and tombstoned base ids
/// are dropped entirely.
pub fn search_layers(
    delta: &MemoryIndex,
    base: Option<&MemoryIndex>,
    tombstones: &std::collections::HashSet<String>,
    query: &str,
    query_vec: Option<&[f32]>,
    opts: &RecallOptions,
) -> Vec<Scored> {
    let learned_share = opts.learned_share.clamp(MIN_LEARNED_SHARE, 1.0);

    // Rank within each layer using the full budget, then trim each to its share.
    let mut unbounded = opts.clone();
    unbounded.token_budget = None;
    unbounded.limit = opts.limit.max(1);

    let learned = search_index(delta, query, query_vec, &unbounded);
    let shipped = match base {
        Some(b) => search_index(b, query, query_vec, &unbounded),
        None => Vec::new(),
    };

    // Anything the delta redefines, or the operator forgot, must not resurface.
    let shadowed: std::collections::HashSet<&String> =
        learned.iter().map(|s| &s.memory.id).collect();
    let shipped: Vec<Scored> = shipped
        .into_iter()
        .filter(|s| !tombstones.contains(&s.memory.id) && !shadowed.contains(&s.memory.id))
        .collect();

    match opts.token_budget {
        None => {
            let mut out = learned;
            out.extend(shipped);
            out.truncate(opts.limit.max(1));
            out
        }
        Some(budget) => {
            let learned_budget = (budget as f32 * learned_share).round() as usize;
            let mut out = take_within(learned, learned_budget, opts.limit);
            // Whatever the learned layer didn't spend is available to the base —
            // a new agent with nothing learned yet should still recall its knowledge.
            let spent: usize =
                out.iter().map(|s| estimate_tokens(s.memory.display_text())).sum();
            let remaining = budget.saturating_sub(spent);
            let room = opts.limit.max(1).saturating_sub(out.len());
            out.extend(take_within(shipped, remaining, room));
            out
        }
    }
}

/// Take from `ranked` while it fits the budget and the count.
fn take_within(ranked: Vec<Scored>, budget: usize, limit: usize) -> Vec<Scored> {
    let mut spent = 0usize;
    let mut kept = Vec::new();
    for s in ranked {
        if kept.len() >= limit {
            break;
        }
        let cost = estimate_tokens(s.memory.display_text());
        if spent + cost > budget && !kept.is_empty() {
            break;
        }
        // The first item is taken even if it alone overruns: returning nothing
        // because the best match is long is worse than overspending slightly.
        spent += cost;
        kept.push(s);
    }
    kept
}

/// Reciprocal Rank Fusion plus the relevance boosts.
fn fuse(
    idx: &MemoryIndex,
    text_hits: &[Hit],
    vector_hits: &[Hit],
    graph_hits: &[Hit],
    opts: &RecallOptions,
) -> Vec<Scored> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut signals: HashMap<String, Signals> = HashMap::new();

    for (rank, hit) in text_hits.iter().enumerate() {
        *scores.entry(hit.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32);
        signals.entry(hit.id.clone()).or_default().text_rank = Some(rank);
    }
    for (rank, hit) in vector_hits.iter().enumerate() {
        *scores.entry(hit.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32);
        let s = signals.entry(hit.id.clone()).or_default();
        s.vector_rank = Some(rank);
        s.vector_similarity = Some(hit.score);
    }
    for (rank, hit) in graph_hits.iter().enumerate() {
        *scores.entry(hit.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32);
        signals.entry(hit.id.clone()).or_default().graph_rank = Some(rank);
    }

    let mut out: Vec<Scored> = scores
        .into_iter()
        .filter_map(|(id, base)| {
            let m = idx.get(&id)?;
            if !m.is_live() {
                return None;
            }
            let mut score = base;
            // Pinned means the operator said this always matters.
            if m.pinned {
                score *= 1.5;
            }
            // Same conversation, then same persona: nearer context wins ties.
            if opts.chat_id.is_some() && m.chat_id == opts.chat_id {
                score *= 1.25;
            }
            if opts.persona.is_some() && m.persona == opts.persona {
                score *= 1.15;
            }
            // Importance nudges rather than dominates: 0..10 maps to 0.5x..1.0x,
            // so a low-importance exact match still outranks a high-importance
            // weak one.
            score *= 0.5 + (m.importance.clamp(0.0, 10.0) / 20.0);
            Some(Scored {
                memory: m.clone(),
                score,
                signals: signals.get(&id).cloned().unwrap_or_default(),
            })
        })
        .collect();

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory.id.cmp(&b.memory.id))
    });
    out
}

/// Fill to the token budget, then the hard limit.
fn apply_budget(mut ranked: Vec<Scored>, opts: &RecallOptions) -> Vec<Scored> {
    ranked.truncate(opts.limit.max(1));
    let Some(budget) = opts.token_budget else {
        return ranked;
    };
    let mut spent = 0usize;
    let mut kept = Vec::new();
    for s in ranked {
        let cost = estimate_tokens(s.memory.display_text());
        // Always take the top result even if it alone exceeds the budget —
        // returning nothing because the best match is long is worse than
        // overspending slightly.
        if kept.is_empty() || spent + cost <= budget {
            spent += cost;
            kept.push(s);
        } else {
            break;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::{Link, LinkKind, Memory, MemoryKind, Source};

    fn idx_with(memories: Vec<Memory>) -> MemoryIndex {
        let mut idx = MemoryIndex::new();
        for m in memories {
            idx.insert_memory(m);
        }
        idx
    }

    fn mem(content: &str) -> Memory {
        Memory::new(MemoryKind::Semantic, content, Source::Tool)
    }

    #[test]
    fn mode_parses_its_aliases() {
        assert_eq!(Mode::parse("hybrid"), Some(Mode::Hybrid));
        assert_eq!(Mode::parse(""), Some(Mode::Hybrid));
        assert_eq!(Mode::parse("FTS"), Some(Mode::Text));
        assert_eq!(Mode::parse("semantic"), Some(Mode::Vector));
        assert_eq!(Mode::parse("nonsense"), None);
    }

    #[test]
    fn text_mode_finds_keyword_matches() {
        let idx = idx_with(vec![mem("the gateway proxies embeddings"), mem("unrelated weather note")]);
        let opts = RecallOptions { mode: Mode::Text, ..Default::default() };
        let out = search_index(&idx, "gateway embeddings", None, &opts);
        assert_eq!(out.len(), 1);
        assert!(out[0].memory.content.contains("gateway"));
        assert_eq!(out[0].signals.text_rank, Some(0));
        assert_eq!(out[0].signals.vector_rank, None);
    }

    #[test]
    fn vector_mode_finds_a_paraphrase_that_shares_no_keywords() {
        let target = mem("the sky appears azure at midday");
        let target_id = target.id.clone();
        let other = mem("compilation times are dominated by linking");
        let other_id = other.id.clone();
        let mut idx = idx_with(vec![target, other]);
        // Hand-built vectors: the query points at the target, away from the other.
        idx.set_vector(&target_id, vec![1.0, 0.0, 0.0]);
        idx.set_vector(&other_id, vec![0.0, 1.0, 0.0]);

        let opts = RecallOptions { mode: Mode::Vector, ..Default::default() };
        let out = search_index(&idx, "no shared words whatsoever", Some(&[0.9, 0.1, 0.0]), &opts);
        assert_eq!(out.len(), 2, "both have vectors, both score > 0");
        assert_eq!(out[0].memory.id, target_id, "the nearer vector must rank first");
        assert!(out[0].signals.vector_similarity.unwrap() > out[1].signals.vector_similarity.unwrap());
    }

    #[test]
    fn hybrid_rewards_agreement_between_retrievers() {
        // `both` is found by text AND vector; `text_only` only by text and ranks
        // higher there. Fusion should still put `both` first.
        let both = mem("rust ownership rules for pod services");
        let both_id = both.id.clone();
        let text_only = mem("rust rust rust ownership ownership rules rules");
        let text_only_id = text_only.id.clone();
        let mut idx = idx_with(vec![both, text_only]);
        idx.set_vector(&both_id, vec![1.0, 0.0]);
        idx.set_vector(&text_only_id, vec![0.0, 1.0]);

        let opts = RecallOptions { mode: Mode::Hybrid, ..Default::default() };
        let out = search_index(&idx, "rust ownership", Some(&[1.0, 0.0]), &opts);
        assert_eq!(out[0].memory.id, both_id, "found by two retrievers, so it wins");
        assert_eq!(out[0].signals.text_rank, Some(1), "even though text ranked it second");
        assert_eq!(out[0].signals.vector_rank, Some(0));
        assert_eq!(out[1].memory.id, text_only_id);
    }

    #[test]
    fn graph_expansion_surfaces_a_neighbour_that_matches_nothing() {
        let hit = mem("the deploy pipeline uses caddy for tls");
        let hit_id = hit.id.clone();
        let neighbour = mem("completely unmatched text about badgers");
        let neighbour_id = neighbour.id.clone();
        let mut idx = idx_with(vec![hit, neighbour]);
        idx.insert_link(Link {
            src: hit_id.clone(),
            dst: neighbour_id.clone(),
            kind: LinkKind::RelatesTo,
            weight: 1.0,
            created_by: "test".into(),
        });

        let opts = RecallOptions { mode: Mode::Hybrid, ..Default::default() };
        let out = search_index(&idx, "caddy tls", None, &opts);
        let ids: Vec<&str> = out.iter().map(|s| s.memory.id.as_str()).collect();
        assert!(ids.contains(&hit_id.as_str()));
        assert!(ids.contains(&neighbour_id.as_str()), "a linked neighbour is recalled with its seed");
        let n = out.iter().find(|s| s.memory.id == neighbour_id).unwrap();
        assert_eq!(n.signals.graph_rank, Some(0));
        assert_eq!(n.signals.text_rank, None, "it matched no keywords at all");
    }

    #[test]
    fn text_mode_runs_no_graph_expansion() {
        let hit = mem("caddy tls termination");
        let hit_id = hit.id.clone();
        let neighbour = mem("unmatched badgers");
        let neighbour_id = neighbour.id.clone();
        let mut idx = idx_with(vec![hit, neighbour]);
        idx.insert_link(Link {
            src: hit_id, dst: neighbour_id.clone(), kind: LinkKind::RelatesTo,
            weight: 1.0, created_by: "test".into(),
        });
        let opts = RecallOptions { mode: Mode::Text, ..Default::default() };
        let out = search_index(&idx, "caddy tls", None, &opts);
        assert_eq!(out.len(), 1, "text mode is exactly BM25, nothing else");
    }

    #[test]
    fn pinned_memories_are_boosted() {
        let mut plain = mem("deployment notes for the pod");
        plain.importance = 5.0;
        let plain_id = plain.id.clone();
        let mut pinned = mem("deployment notes for the pod, pinned copy");
        pinned.pinned = true;
        pinned.importance = 5.0;
        let pinned_id = pinned.id.clone();

        let idx = idx_with(vec![plain, pinned]);
        let opts = RecallOptions { mode: Mode::Text, ..Default::default() };
        let out = search_index(&idx, "deployment notes pod", None, &opts);
        assert_eq!(out[0].memory.id, pinned_id, "pinned outranks equivalent unpinned");
        assert!(out.iter().any(|s| s.memory.id == plain_id));
    }

    #[test]
    fn same_chat_and_persona_break_ties_toward_nearer_context() {
        let mut here = mem("the config lives in bot_config");
        here.chat_id = Some("chat-1".into());
        let here_id = here.id.clone();
        let elsewhere = mem("the config lives in bot_config too");

        let idx = idx_with(vec![here, elsewhere]);
        let opts = RecallOptions {
            mode: Mode::Text,
            chat_id: Some("chat-1".into()),
            ..Default::default()
        };
        let out = search_index(&idx, "config bot_config", None, &opts);
        assert_eq!(out[0].memory.id, here_id);
    }

    #[test]
    fn importance_nudges_but_does_not_dominate() {
        // A weak keyword match with max importance must not beat a strong match
        // with low importance.
        let mut strong = mem("kubernetes ingress controller configuration");
        strong.importance = 1.0;
        let strong_id = strong.id.clone();
        let mut weak = mem("kubernetes something else entirely about storage volumes and disks");
        weak.importance = 10.0;

        let idx = idx_with(vec![strong, weak]);
        let opts = RecallOptions { mode: Mode::Text, ..Default::default() };
        let out = search_index(&idx, "ingress controller configuration", None, &opts);
        assert_eq!(out[0].memory.id, strong_id);
    }

    #[test]
    fn archived_and_superseded_never_survive_fusion() {
        let mut archived = mem("archived note about pelicans");
        archived.archived_at = Some(chrono::Utc::now());
        let archived_id = archived.id.clone();
        let mut merged = mem("merged note about pelicans");
        merged.superseded_by = Some("somewhere".into());
        let live = mem("live note about pelicans");
        let live_id = live.id.clone();

        let mut idx = idx_with(vec![archived, merged, live]);
        // Even with vectors attached, they must not come back.
        idx.set_vector(&archived_id, vec![1.0, 0.0]);
        let opts = RecallOptions { mode: Mode::Hybrid, ..Default::default() };
        let out = search_index(&idx, "pelicans", Some(&[1.0, 0.0]), &opts);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].memory.id, live_id);
    }

    #[test]
    fn the_token_budget_trims_the_tail() {
        let idx = idx_with(vec![
            mem(&format!("budget alpha {}", "x".repeat(400))),
            mem(&format!("budget beta {}", "y".repeat(400))),
            mem(&format!("budget gamma {}", "z".repeat(400))),
        ]);
        let opts = RecallOptions {
            mode: Mode::Text,
            token_budget: Some(120), // each item costs ~100 tokens
            ..Default::default()
        };
        let out = search_index(&idx, "budget", None, &opts);
        assert_eq!(out.len(), 1, "only one fits in 120 tokens");
    }

    #[test]
    fn the_top_result_is_returned_even_if_it_alone_blows_the_budget() {
        let idx = idx_with(vec![mem(&format!("enormous single memory {}", "x".repeat(4000)))]);
        let opts = RecallOptions { mode: Mode::Text, token_budget: Some(10), ..Default::default() };
        let out = search_index(&idx, "enormous memory", None, &opts);
        assert_eq!(out.len(), 1, "returning nothing would be worse than overspending");
    }

    #[test]
    fn limit_is_respected() {
        let idx = idx_with((0..10).map(|i| mem(&format!("limited item number {i}"))).collect());
        let opts = RecallOptions { mode: Mode::Text, limit: 3, ..Default::default() };
        assert_eq!(search_index(&idx, "limited item", None, &opts).len(), 3);
    }

    #[test]
    fn hybrid_without_a_query_vector_degrades_to_text_plus_graph() {
        let idx = idx_with(vec![mem("degraded mode still finds keywords")]);
        let opts = RecallOptions { mode: Mode::Hybrid, ..Default::default() };
        let out = search_index(&idx, "degraded keywords", None, &opts);
        assert_eq!(out.len(), 1, "no embedding available is not an error");
        assert_eq!(out[0].signals.vector_rank, None);
    }

    #[test]
    fn empty_store_and_empty_query_return_nothing() {
        let idx = MemoryIndex::new();
        let opts = RecallOptions::default();
        assert!(search_index(&idx, "anything", None, &opts).is_empty());
        let idx = idx_with(vec![mem("something")]);
        assert!(search_index(&idx, "", None, &opts).is_empty());
    }

    #[test]
    fn signals_describe_their_provenance() {
        let s = Signals { text_rank: Some(0), vector_rank: Some(2), graph_rank: None, vector_similarity: Some(0.9) };
        assert_eq!(s.describe(), "text#1,vector#3");
        assert_eq!(Signals::default().describe(), "");
    }

    #[test]
    fn kind_filter_applies_to_both_retrievers() {
        let pref = Memory::new(MemoryKind::Preference, "prefers dark mode always", Source::User);
        let pref_id = pref.id.clone();
        let fact = Memory::new(MemoryKind::Semantic, "dark mode is implemented in css", Source::Tool);
        let fact_id = fact.id.clone();
        let mut idx = idx_with(vec![pref, fact]);
        idx.set_vector(&pref_id, vec![1.0, 0.0]);
        idx.set_vector(&fact_id, vec![1.0, 0.0]);

        let opts = RecallOptions {
            mode: Mode::Hybrid,
            kind: Some(MemoryKind::Preference),
            ..Default::default()
        };
        let out = search_index(&idx, "dark mode", Some(&[1.0, 0.0]), &opts);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].memory.id, pref_id);
    }
}
