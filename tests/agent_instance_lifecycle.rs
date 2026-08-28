//! Deleting agents, and reaping the sessions they leave behind.
//!
//! The rule these pin: **an agent is never deleted on a timer, and a session is**
//! — at 30 days of inactivity. An agent is the memory of a relationship, so
//! losing one costs everything it learned; a transcript is a record of what
//! happened, and it ages out.
//!
//! **One test function on purpose.** `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so two `#[test]`s in one binary silently
//! share whichever dir was set first — and then interfere, since the sweep walks
//! every session in it. Same reason `pack_resolution_test` and
//! `memory_layers_test` are each a single test.

use axum::body::Body;
use axum::http::Request;
use metalcraft_agent::agent_instance::{AgentInstance, InstanceOrigin};
use metalcraft_agent::agent_preset::AgentPreset;
use metalcraft_agent::memory::types::{MemoryKind, Source};
use metalcraft_agent::memory::{self, RememberRequest, instance};
use std::fs;

/// Deleting an agent must take its memory with it, and must not leave it answering
/// recalls from the resident set.
///
/// `delete` used to remove only the record. The memory directory stayed on disk
/// unreachable, and — because nothing evicted it — the deleted agent kept serving
/// recall, held its base alive, and occupied one of the eight LRU slots for the life
/// of the process.
#[tokio::test]
async fn deleting_agents_and_reaping_their_sessions() {
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

    // ── an agent is never reaped, however long it has been quiet ───────────

    let stale = "2020-01-01T00:00:00+00:00".to_string();
    let mk = |last: &str| {
        let mut i = AgentInstance::new(&preset, InstanceOrigin::Workshop);
        i.last_active_at = last.to_string();
        i.save().unwrap();
        i.id
    };

    let ancient = mk(&stale);
    let fresh = mk(&chrono::Utc::now().to_rfc3339());

    // A rename is a label, not a lifetime. It used to set a `persistent` flag, so
    // the gesture for "call it something I recognise" quietly changed how long the
    // pod kept it. There is no such flag now, and the PATCH cannot carry one.
    let renamed = mk(&stale);
    let router = metalcraft_agent::workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/agents/instances/{renamed}"))
            .header("authorization", "Bearer k")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Sunday prep","persistent":true}"#))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let renamed_agent = metalcraft_agent::agent_instance::load(&renamed).expect("load renamed");
    assert_eq!(renamed_agent.name, "Sunday prep");
    let wire = serde_json::to_value(&renamed_agent).unwrap();
    assert!(
        wire.get("persistent").is_none(),
        "a lifetime flag nothing honours must not be on the wire: {wire}"
    );

    // ── sessions age out; the agents that wrote them do not ─────────────────

    let chats = metalcraft_agent::paths::chats_dir();
    fs::create_dir_all(&chats).unwrap();
    let write_chat = |chat_id: &str, instance: &str, days_ago: i64| {
        let path = chats.join(format!("{chat_id}.json"));
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": chat_id,
                "instance_id": instance,
                "persona_slug": "amy-chef",
                "model_name": "gpt-5",
                "cwd": ".",
                "created_at": "2020-01-01T00:00:00Z",
                "messages": []
            }))
            .unwrap(),
        )
        .unwrap();
        // The sweep reads the file's mtime — the clock the pod already keeps,
        // rewritten after every turn — so that is what the fixture has to set.
        //
        // Through `touch` rather than a crate: setting an mtime needs `utimensat`,
        // which std does not expose, and a dependency added for one line of one
        // test is a worse trade than shelling out to a POSIX tool.
        let when = chrono::Utc::now() - chrono::Duration::days(days_ago);
        let stamp = when.format("%Y%m%d%H%M.%S").to_string();
        let touched = std::process::Command::new("touch")
            .arg("-t")
            .arg(&stamp)
            .arg(&path)
            .status()
            .expect("touch");
        assert!(touched.success(), "could not backdate {}", path.display());
        path
    };

    let old_chat = write_chat("chat_old", &ancient, 45);
    let recent_chat = write_chat("chat_recent", &ancient, 2);

    let report = metalcraft_agent::workshop_api::reap_stale_chats().await;
    assert_eq!(report.reaped, vec!["chat_old".to_string()], "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(!old_chat.exists(), "a session idle for 45 days is gone");
    assert!(recent_chat.exists(), "a session used this week is kept");

    // The point of the whole arrangement: the agent that wrote the deleted
    // session is untouched, and so is everything it learned.
    for id in [&ancient, &fresh, &renamed] {
        assert!(
            metalcraft_agent::agent_instance::load(id).is_ok(),
            "agent {id} must survive: nothing deletes one on a timer"
        );
    }

    let _ = fs::remove_dir_all(&data_dir);
}
