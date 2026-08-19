//! What an agent learns has to still be there tomorrow.
//!
//! This exists because it wasn't. `mem_remember` fsynced an `Upsert` to the agent's
//! own log, and `handle_for` then built the delta **empty** every time the agent
//! became resident. So everything an agent was told was write-only: a restart — or
//! simply eight other chats pushing this one out of the LRU — made it unreachable.
//! The bytes stayed on disk, correct and unread, and `instance_view` reported
//! `learned: 0`.
//!
//! Own test binary: `paths::data_dir()` caches `METALCRAFT_DATA_DIR` in a `OnceLock`.

use metalcraft_agent::agent_instance::{AgentInstance, InstanceOrigin};
use metalcraft_agent::agent_preset::AgentPreset;
use metalcraft_agent::memory::types::{MemoryKind, Source};
use metalcraft_agent::memory::{self, instance, RememberRequest};
use std::fs;

const SEED: &str = r#"{"kind":"Semantic","content":"Amy braises at 2:1 mirepoix to leek.","summary":"braise ratio","importance":7.0}
"#;

#[tokio::test]
async fn a_learned_memory_survives_eviction() {
    let data_dir = std::env::temp_dir().join(format!("mc-mem-durable-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        std::env::set_var("MEMORY_ENABLED", "1");
    }
    fs::create_dir_all(&data_dir).unwrap();

    // A preset with a shipped base, and an agent made from it.
    let preset: AgentPreset = serde_json::from_str(
        r#"{"slug":"amy-kitchen","name":"Amy","default_persona":"amy-chef",
            "personas":[{"slug":"amy-chef","role":"default"}],"version":"1.4.0"}"#,
    )
    .unwrap();
    let seed_file = data_dir.join("seed.jsonl");
    fs::write(&seed_file, SEED).unwrap();
    instance::build_base("amy-kitchen", "1.4.0", &seed_file).expect("build base");

    let inst = AgentInstance::new(&preset, InstanceOrigin::Workshop);
    inst.save().expect("save instance");
    let id = inst.id.clone();

    // ── tell the agent something ────────────────────────────────────────────
    let mut req = RememberRequest::new(
        MemoryKind::Semantic,
        "Andrew is allergic to shellfish.",
        Source::Turn,
    );
    req.summary = Some("shellfish allergy".into());
    req.instance_id = Some(id.clone());
    let remembered = memory::remember(req).await.expect("remember");
    assert!(!remembered.deduplicated);
    let learned_id = remembered.memory.id.clone();

    let view = memory::instance_view(&id, 10).await;
    assert_eq!(view.learned, 1, "it should be there immediately");
    assert_eq!(view.shipped, 1, "and the shipped base should be visible too");

    // ── evict it, exactly as the LRU does when other chats arrive ───────────
    instance::evict(&id);
    assert!(
        !instance::resident_instances().iter().any(|r| r == &id),
        "the instance should be out of the resident set"
    );

    // ── and it is still there ───────────────────────────────────────────────
    let view = memory::instance_view(&id, 10).await;
    assert_eq!(
        view.learned, 1,
        "a learned memory must survive leaving the resident set — this is the bug"
    );
    assert!(
        view.sample.iter().any(|m| m.text.contains("shellfish")),
        "and be readable, not merely counted: {:?}",
        view.sample
    );
    assert!(
        memory::instance_get(&id, &learned_id).await.is_some(),
        "it must still be addressable by its original id"
    );

    // ── forgetting must survive too ─────────────────────────────────────────
    // A purge used to be RAM-only, so the next replay brought the memory straight
    // back: a forget that lasted only until the agent left the resident set.
    memory::instance_forget(&id, &learned_id).await.expect("forget");
    instance::evict(&id);
    let view = memory::instance_view(&id, 10).await;
    assert_eq!(view.learned, 0, "a forgotten memory must stay forgotten");
    assert!(
        !view.sample.iter().any(|m| m.text.contains("shellfish")),
        "and must not reappear in recall: {:?}",
        view.sample
    );

    // ── the shipped base is untouched by any of it ──────────────────────────
    assert_eq!(view.shipped, 1, "forgetting a learned memory must not disturb the base");

    let _ = fs::remove_dir_all(&data_dir);
}
