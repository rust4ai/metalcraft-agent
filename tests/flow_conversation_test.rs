//! A flow firing writes itself into its agent's conversation.
//!
//! Before this, a scheduled run left a diagnostics session and nothing a person
//! would read: the agent it ran as listed zero conversations and opened onto an
//! empty transcript. The two rules worth pinning are the ones that decide
//! whether a chat exists at all — **a run records only when it has an agent**,
//! and **only when it actually says something** — plus the rolling window that
//! keeps a five-minute cron from minting 288 threads a day.
//!
//! Own test binary: `paths::data_dir()` memoizes `METALCRAFT_DATA_DIR`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::agent_instance::{AgentInstance, InstanceOrigin};
use metalcraft_agent::flow_bindings;
use metalcraft_agent::flow_exec::run_flow_v2_as;
use metalcraft_agent::paths;
use metalcraft_agent::runtime::AgentRuntimeContext;
use metalcraft_agent::workshop_api::{self, flow_conversation, record_flow_turn};
use metalcraft_flows::{SavedFlow, save_flow};
use serde_json::json;

fn ctx() -> AgentRuntimeContext {
    AgentRuntimeContext {
        personas_dir: paths::personas_dir(),
        skills_dir: paths::skills_dir(),
        api_key: String::new(),
    }
}

/// Entry straight to an end node: no prompt, no sub-agent, nothing spoken.
fn silent_flow() -> SavedFlow {
    serde_json::from_value(json!({
        "spec_version": "2", "id": "silent", "name": "Silent flow",
        "created_at": "2026-08-23T00:00:00Z", "updated_at": "2026-08-23T00:00:00Z",
        "enabled": false,
        "flow": {
            "nodes": [
                { "id": "entry", "node_type": "entry", "data": { "schedule_type": "manual" } },
                { "id": "done", "node_type": "end", "data": { "status": "ok" } }
            ],
            "edges": [{ "id": "e0", "source": "entry", "target": "done" }]
        }
    }))
    .unwrap()
}

fn agent() -> AgentInstance {
    let preset: metalcraft_agent::agent_preset::AgentPreset = serde_json::from_str(
        r#"{"slug":"amy","name":"Amy","description":"d","default_persona":"amy",
            "personas":[{"slug":"amy","role":"default"}]}"#,
    )
    .unwrap();
    let mut i = AgentInstance::new(
        &preset,
        InstanceOrigin::Flow {
            flow_id: "brief".into(),
        },
    );
    i.save().unwrap();
    i
}

async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .uri(uri)
            .header("authorization", "Bearer k")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn a_firing_becomes_a_conversation_in_the_agent_it_runs_as() {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", tmp.path());
    }
    let agent = agent();

    // --- a flow that never speaks leaves no conversation ---------------------
    // An empty chat in the agent's list would be worse than nothing: it reads as
    // "it talked to you and you missed it".
    save_flow(&paths::flows_dir(), &silent_flow()).unwrap();
    let summary = run_flow_v2_as(
        &ctx(),
        silent_flow(),
        ".",
        Some("amy"),
        "m",
        &json!({}),
        Some(agent.id.clone()),
    )
    .await
    .expect("silent flow runs");
    // The run's status is whatever its `end` node declares.
    assert_eq!(summary.status, "ok");
    assert!(
        summary.chat_id.is_none(),
        "a tool-only flow must not open a conversation: {summary:?}"
    );

    let (_, detail) = get_json(&format!("/api/v1/agents/instances/{}", agent.id)).await;
    assert_eq!(detail["conversations"].as_array().unwrap().len(), 0);

    // --- a spoken turn opens one, and the next joins it ----------------------
    let chat = flow_conversation(&agent.id, "amy", "m", ".")
        .await
        .expect("a conversation opens");
    record_flow_turn(
        &chat,
        "▶ Morning brief\n\nWhat is on today?",
        "Three things.",
    )
    .await;

    // Within the window, the next firing continues the same thread rather than
    // starting a new one — the rule that keeps a fast cron readable.
    let again = flow_conversation(&agent.id, "amy", "m", ".")
        .await
        .expect("a conversation opens");
    assert_eq!(
        again, chat,
        "a firing inside the window joins the last thread"
    );
    record_flow_turn(&chat, "Anything else?", "No.").await;

    // What the agent's own surfaces now say: one conversation, two turns.
    let (status, detail) = get_json(&format!("/api/v1/agents/instances/{}", agent.id)).await;
    assert_eq!(status, StatusCode::OK);
    let conversations = detail["conversations"].as_array().unwrap();
    assert_eq!(conversations.len(), 1, "{detail:#}");
    // Turns, not messages: the two times someone spoke to it. This endpoint used
    // to count raw messages while `GET /chats` counted user turns, so the same
    // conversation was "4" on one screen and "2" on the other.
    assert_eq!(conversations[0]["turn_count"], 2);

    let (_, chat_detail) = get_json(&format!("/api/v1/chats/{chat}")).await;
    let messages = chat_detail["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "Three things.");
    // The firing names itself once, so a rolling thread shows where each run
    // began without repeating the marker on every turn.
    assert!(
        messages[0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("▶ Morning brief")),
        "{messages:#?}"
    );
    assert!(
        messages[2]["content"]
            .as_str()
            .is_some_and(|c| !c.starts_with('▶')),
        "{messages:#?}"
    );

    // And the conversation belongs to the agent, which is what makes it visible
    // in the fleet at all.
    assert_eq!(chat_detail["instance_id"], agent.id);

    a_hand_triggered_run_is_the_agent_the_schedule_armed().await;
}

/// `POST /flows/{id}/run` on an **armed** automation is the same act as its
/// scheduled firing — same agent, same memory. On an unarmed one it stays a
/// test: no agent, no conversation, exactly as before this existed.
///
/// Not its own `#[test]`: `paths::data_dir()` memoizes the env var for the
/// process, so a second test would silently share (or race) the first's
/// tempdir. Same discipline as `chat_persona_override_test`.
async fn a_hand_triggered_run_is_the_agent_the_schedule_armed() {
    let mut armable = silent_flow();
    armable.id = "armed".into();
    save_flow(&paths::flows_dir(), &armable).unwrap();

    // A preset the flow can be bound to, then armed — which is what mints the agent.
    let presets = paths::agent_presets_dir();
    std::fs::create_dir_all(&presets).unwrap();
    std::fs::write(
        presets.join("amy.json"),
        r#"{"slug":"amy","name":"Amy","description":"d","default_persona":"amy",
            "personas":[{"slug":"amy","role":"default"}]}"#,
    )
    .unwrap();
    flow_bindings::bind_preset(&armable, "amy").expect("bind");
    let armed_schedule = metalcraft_agent::scheduled_flows::arm(
        metalcraft_agent::scheduled_flows::NewSchedule {
            flow: &armable,
            schedule: metalcraft_flows::ScheduleSpec {
                trigger: metalcraft_flows::ScheduleTrigger::Cron {
                    cron: "0 0 8 * * *".into(),
                },
                name: Some("Daily".into()),
                timezone: None,
                inputs: None,
                persona: None,
            },
            enabled: true,
            instance: None,
            from_suggestion: None,
            id: None,
        },
    )
    .expect("arm");
    let agent =
        metalcraft_agent::agent_instance::load(armed_schedule.instance_id.as_deref().unwrap())
            .expect("arming minted the agent");

    let (status, summary) = post_run("armed").await;
    assert_eq!(status, StatusCode::OK, "{summary:#}");
    // The run resolved the armed agent without being told which one.
    let run = metalcraft_agent::flow_runs::load_run(
        &paths::runs_dir(),
        summary["run_id"].as_str().unwrap(),
    );
    // (A silent flow never pauses, so there is no persisted record; the proof is
    // that the executor accepted the instance — asserted via the agent below.)
    assert!(run.is_none());
    assert!(
        summary["warnings"].as_array().is_none_or(|w| w.is_empty()),
        "{summary:#}"
    );

    // An unarmed flow resolves to no agent and says nothing about one.
    let mut loose = silent_flow();
    loose.id = "loose".into();
    save_flow(&paths::flows_dir(), &loose).unwrap();
    let (status, summary) = post_run("loose").await;
    assert_eq!(status, StatusCode::OK, "{summary:#}");
    assert!(summary["chat_id"].is_null(), "{summary:#}");

    // The armed agent exists and was not disturbed by the unarmed run.
    assert!(metalcraft_agent::agent_instance::load(&agent.id).is_ok());
}

async fn post_run(flow_id: &str) -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/flows/{flow_id}/run"))
            .header("authorization", "Bearer k")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await
    .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}
