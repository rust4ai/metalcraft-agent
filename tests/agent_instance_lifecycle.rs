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
use metalcraft_agent::memory::{self, RememberRequest, instance};
use axum::body::Body;
use axum::http::Request;
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

    // ── only ephemeral, idle, unreferenced agents are reaped ───────────────

    let stale = "2020-01-01T00:00:00+00:00".to_string();
    let mk = |persistent: bool, last: &str| {
        let mut i = AgentInstance::new(&preset, InstanceOrigin::Workshop);
        i.persistent = persistent;
        i.last_active_at = last.to_string();
        i.save().unwrap();
        i.id
    };

    let idle_ephemeral = mk(false, &stale);
    let idle_kept = mk(true, &stale);
    let fresh_ephemeral = mk(false, &chrono::Utc::now().to_rfc3339());
    let idle_but_in_use = mk(false, &stale);

    // A rename is a label, not a lifetime. Patching a name used to set `persistent`,
    // so the gesture for "call it something I recognise" quietly changed how long the
    // pod kept it. This one is renamed through the API and reaped all the same.
    let idle_renamed = mk(false, &stale);
    let router = metalcraft_agent::workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/agents/instances/{idle_renamed}"))
            .header("authorization", "Bearer k")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Sunday prep"}"#))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let mut renamed = metalcraft_agent::agent_instance::load(&idle_renamed).expect("load renamed");
    assert_eq!(renamed.name, "Sunday prep");
    assert!(!renamed.persistent, "a rename must not promote");
    // The patch touched it, which is honest — someone just interacted with it. Rewind
    // the clock so what is under test is the flag rather than the timestamp.
    renamed.last_active_at = stale.clone();
    renamed.save().unwrap();

    let report = metalcraft_agent::agent_instance::reap_ephemeral(&[idle_but_in_use.clone()]);

    let mut reaped = report.reaped.clone();
    reaped.sort();
    let mut expected = vec![idle_ephemeral.clone(), idle_renamed.clone()];
    expected.sort();
    assert_eq!(reaped, expected, "the idle ephemeral ones, renamed or not");
    assert!(report.failed.is_empty(), "{:?}", report.failed);

    let left: Vec<String> = metalcraft_agent::agent_instance::list()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert!(!left.contains(&idle_ephemeral));
    assert!(
        !left.contains(&idle_renamed),
        "a name buys an agent nothing — `persistent` is what keeps it"
    );
    assert!(
        left.contains(&idle_kept),
        "an agent marked persistent is kept"
    );
    assert!(
        left.contains(&fresh_ephemeral),
        "a recently used agent is not idle"
    );
    assert!(
        left.contains(&idle_but_in_use),
        "an agent a conversation still points at is kept — deleting it strands the transcript"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
