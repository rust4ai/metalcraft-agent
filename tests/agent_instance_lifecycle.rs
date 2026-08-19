//! Deleting and reaping agents.
//!
//! **One test function on purpose.** `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so two `#[test]`s in one binary silently
//! share whichever dir was set first — and then interfere, since the reaper walks
//! every instance in it. Same reason `pack_resolution_test` and
//! `memory_layers_test` are each a single test.

use metalcraft_agent::agent_instance::{AgentInstance, InstanceOrigin};
use metalcraft_agent::agent_preset::AgentPreset;
use metalcraft_agent::memory::types::{MemoryKind, Source};
use metalcraft_agent::memory::{self, instance, RememberRequest};
use std::fs;

/// Deleting an agent must take its memory with it, and must not leave it answering
/// recalls from the resident set.
///
/// `delete` used to remove only the record. The memory directory stayed on disk
/// unreachable, and — because nothing evicted it — the deleted agent kept serving
/// recall, held its base alive, and occupied one of the eight LRU slots for the life
/// of the process.
#[tokio::test]
async fn deleting_and_reaping_agents() {
    let data_dir = std::env::temp_dir().join(format!("mc-agent-life-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        std::env::set_var("MEMORY_ENABLED", "1");
    }
    fs::create_dir_all(&data_dir).unwrap();

    let preset: AgentPreset = serde_json::from_str(
        r#"{"slug":"amy-kitchen","name":"Amy","default_persona":"amy-chef",
            "personas":[{"slug":"amy-chef","role":"default"}],"version":"1.4.0"}"#,
    )
    .unwrap();
    let inst = AgentInstance::new(&preset, InstanceOrigin::Workshop);
    inst.save().expect("save");
    let id = inst.id.clone();

    let mut req = RememberRequest::new(MemoryKind::Semantic, "A private thing.", Source::Turn);
    req.instance_id = Some(id.clone());
    memory::remember(req).await.expect("remember");

    let mem_dir = metalcraft_agent::paths::memory_instance_dir(&id);
    assert!(mem_dir.is_dir(), "the agent should have a memory directory");
    assert!(
        instance::resident_instances().iter().any(|r| r == &id),
        "and be resident after writing"
    );

    metalcraft_agent::agent_instance::delete(&id).expect("delete");

    assert!(!mem_dir.exists(), "its memory must not outlive it");
    assert!(
        !instance::resident_instances().iter().any(|r| r == &id),
        "and it must not still be resident, answering recalls"
    );

    // ── only unnamed, idle, unreferenced agents are reaped ──────────────────

    let stale = "2020-01-01T00:00:00+00:00".to_string();
    let mut mk = |persistent: bool, last: &str| {
        let mut i = AgentInstance::new(&preset, InstanceOrigin::Workshop);
        i.persistent = persistent;
        i.last_active_at = last.to_string();
        i.save().unwrap();
        i.id
    };

    let idle_unnamed = mk(false, &stale);
    let idle_named = mk(true, &stale);
    let fresh_unnamed = mk(false, &chrono::Utc::now().to_rfc3339());
    let idle_but_in_use = mk(false, &stale);

    let report = metalcraft_agent::agent_instance::reap_ephemeral(&[idle_but_in_use.clone()]);

    assert_eq!(report.reaped, vec![idle_unnamed.clone()], "only the idle unnamed one");
    assert!(report.failed.is_empty(), "{:?}", report.failed);

    let left: Vec<String> =
        metalcraft_agent::agent_instance::list().into_iter().map(|i| i.id).collect();
    assert!(!left.contains(&idle_unnamed));
    assert!(left.contains(&idle_named), "naming an agent is what keeps it");
    assert!(left.contains(&fresh_unnamed), "a recently used agent is not idle");
    assert!(
        left.contains(&idle_but_in_use),
        "an agent a conversation still points at is kept — deleting it strands the transcript"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
