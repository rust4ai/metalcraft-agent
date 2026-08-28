//! Two-layer instance memory: a shared, immutable base plus a per-agent delta.
//!
//! One `#[test]` so the process-global `METALCRAFT_DATA_DIR` isn't raced, matching
//! `pack_resolution_test`.

use metalcraft_agent::memory::index::MemoryIndex;
use metalcraft_agent::memory::instance::{self, InstanceMemory};
use metalcraft_agent::memory::recall::{self, RecallOptions};
use metalcraft_agent::memory::types::{Memory, MemoryKind, Source};
use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use tokio::sync::RwLock;

const SEED: &str = r#"{"kind":"Semantic","content":"Amy braises at 2:1 mirepoix to leek.","summary":"braise base ratio","entity":"braising","importance":7.0,"tags":["technique"]}
{"kind":"Procedural","content":"Sear the meat first; the fond is the dish.","summary":"sear before braising","entity":"braising","importance":8.0}
{"kind":"Semantic","content":"Sourdough hydration above 80% needs a stiffer starter.","summary":"hydration","entity":"sourdough","importance":6.0}
"#;

fn learned(content: &str, summary: &str) -> Memory {
    let mut m = Memory::new(MemoryKind::Episodic, content, Source::Turn);
    m.summary = summary.to_string();
    m
}

#[tokio::test]
async fn base_and_delta_compose_into_one_agent_memory() {
    let data_dir = std::env::temp_dir().join(format!("mc-mem-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    // ── build a preset base, once per (preset, version) ──────────────────────
    let seed_file = data_dir.join("seed-memories.jsonl");
    fs::create_dir_all(&data_dir).unwrap();
    fs::write(&seed_file, SEED).unwrap();

    let count = instance::build_base("amy-kitchen", "1.4.0", &seed_file).expect("build base");
    assert_eq!(count, 3);

    // Loading twice must hand back the *same* index — twenty agents, one copy.
    let first = instance::load_base("amy-kitchen", "1.4.0").expect("load base");
    let second = instance::load_base("amy-kitchen", "1.4.0").expect("load base again");
    assert!(
        Arc::ptr_eq(&first, &second),
        "base layers must be shared, not re-read per instance"
    );
    assert_eq!(first.read().await.len(), 3);

    assert!(
        instance::load_base("amy-kitchen", "9.9.9").is_err(),
        "a version with no built base is an error, not silently empty"
    );

    // ── an instance is a delta over that base ───────────────────────────────
    let mem = instance::handle_for("inst_a", Some(("amy-kitchen", "1.4.0"))).expect("handle");
    assert_eq!(
        mem.visible_count().await,
        3,
        "a new agent already knows what it shipped with"
    );
    assert!(
        mem.delta.read().await.is_empty(),
        "and has learned nothing yet — O(1) creation"
    );

    // Two agents of the same preset share the base but not the delta.
    let other = instance::handle_for("inst_b", Some(("amy-kitchen", "1.4.0"))).expect("handle b");
    mem.delta
        .write()
        .await
        .insert_memory(learned("Andrew hates cilantro.", "cilantro"));
    assert_eq!(mem.visible_count().await, 4);
    assert_eq!(
        other.visible_count().await,
        3,
        "one agent's learning must not leak into another"
    );

    // ── forgetting ──────────────────────────────────────────────────────────
    let base_id = first.read().await.iter().next().unwrap().id.clone();
    let delta_id = mem.delta.read().await.iter().next().unwrap().id.clone();

    assert_eq!(
        mem.forget(&delta_id).await.expect("forget own"),
        instance::Forgotten::Purged,
        "an agent's own memory is simply gone"
    );
    assert_eq!(
        mem.forget(&base_id).await.expect("forget shipped"),
        instance::Forgotten::Tombstoned,
        "a shipped memory is tombstoned — the shared base is never mutated"
    );
    assert!(!mem.is_visible(&base_id).await);
    assert!(
        other.is_visible(&base_id).await,
        "the other agent still sees it"
    );
    assert_eq!(first.read().await.len(), 3, "the base itself is untouched");

    // The tombstone is durable: a fresh handle after eviction still honours it.
    instance::evict("inst_a");
    let reloaded = instance::handle_for("inst_a", Some(("amy-kitchen", "1.4.0"))).expect("reload");
    assert!(
        !reloaded.is_visible(&base_id).await,
        "a forgotten memory must stay forgotten"
    );

    // ── layered recall ──────────────────────────────────────────────────────
    let mut delta = MemoryIndex::new();
    delta.insert_memory(learned("Andrew braises on Sundays.", "sunday braising"));
    let base_idx = first.read().await;
    let tombs: HashSet<String> = HashSet::new();

    let opts = RecallOptions {
        limit: 10,
        ..Default::default()
    };
    let hits = recall::search_layers(&delta, Some(&base_idx), &tombs, "braising", &opts);
    let summaries: Vec<&str> = hits.iter().map(|h| h.memory.display_text()).collect();
    assert!(
        summaries.contains(&"sunday braising"),
        "learned memories surface: {summaries:?}"
    );
    assert!(
        summaries.contains(&"braise base ratio"),
        "shipped memories surface: {summaries:?}"
    );

    // A tombstoned base memory must not come back through recall.
    let braise_id = base_idx
        .iter()
        .find(|m| m.summary == "braise base ratio")
        .map(|m| m.id.clone())
        .unwrap();
    let mut tombs2 = HashSet::new();
    tombs2.insert(braise_id.clone());
    let hits = recall::search_layers(&delta, Some(&base_idx), &tombs2, "braising", &opts);
    assert!(
        !hits.iter().any(|h| h.memory.id == braise_id),
        "recall must respect tombstones, not just visible_count"
    );

    // Recall works with no base at all — an agent that only knows what it learned.
    let hits = recall::search_layers(&delta, None, &tombs, "braising", &opts);
    assert_eq!(hits.len(), 1);

    // ── the budget split protects the operator's own memories ───────────────
    let mut fat_base = MemoryIndex::new();
    for i in 0..50 {
        let mut m = Memory::new(
            MemoryKind::Semantic,
            format!("Shipped fact {i} about braising, padded out to consume budget."),
            Source::Seeded,
        );
        m.summary = format!("shipped {i} braising");
        fat_base.insert_memory(m);
    }
    let mut small_delta = MemoryIndex::new();
    small_delta.insert_memory(learned("Andrew braises on Sundays.", "sunday braising"));

    let budgeted = RecallOptions {
        limit: 20,
        token_budget: Some(60),
        ..Default::default()
    };
    let hits = recall::search_layers(
        &small_delta,
        Some(&fat_base),
        &tombs,
        "braising",
        &budgeted,
    );
    assert!(
        hits.iter()
            .any(|h| h.memory.display_text() == "sunday braising"),
        "a large shipped corpus must not crowd out what this agent learned: {:?}",
        hits.iter()
            .map(|h| h.memory.display_text())
            .collect::<Vec<_>>()
    );

    // A pack asking for an unreasonable share is clamped, not obeyed.
    let greedy = RecallOptions {
        limit: 20,
        token_budget: Some(60),
        learned_share: 0.0,
        ..Default::default()
    };
    let hits = recall::search_layers(
        &small_delta,
        Some(&fat_base),
        &tombs,
        "braising",
        &greedy,
    );
    assert!(
        hits.iter()
            .any(|h| h.memory.display_text() == "sunday braising"),
        "learned_share must be clamped to a floor so a preset cannot silence the operator"
    );

    // ── LRU eviction keeps the resident set bounded ─────────────────────────
    drop(base_idx);
    for i in 0..12 {
        instance::handle_for(&format!("inst_lru_{i}"), None).expect("handle");
    }
    let resident = instance::resident_instances();
    assert!(
        resident.len() <= 8,
        "resident set must stay bounded, got {}",
        resident.len()
    );
    assert!(
        resident.iter().any(|k| k == "inst_lru_11"),
        "the most recent instance must still be resident"
    );

    let _ = fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn an_instance_with_no_base_is_valid() {
    // Not every preset ships memories, and a legacy pod has none at all. That must
    // be an ordinary agent, not an error.
    let mem = InstanceMemory {
        instance_id: "inst_bare".into(),
        base: None,
        base_key: None,
        delta: Arc::new(RwLock::new(MemoryIndex::new())),
        tombstones: Arc::new(RwLock::new(HashSet::new())),
    };
    assert_eq!(mem.visible_count().await, 0);
    assert!(
        mem.forget("nope").await.is_err(),
        "forgetting nothing is an error, not a crash"
    );
}
