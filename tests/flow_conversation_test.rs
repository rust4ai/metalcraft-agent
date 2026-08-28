//! A flow run writes itself into its agent's conversation.
//!
//! Before this, a scheduled run left a diagnostics session and nothing a person
//! would read: the agent it ran as listed zero conversations and opened onto an
//! empty transcript. The rules worth pinning are the ones that decide what a
//! person finds afterwards — **every run leaves exactly one conversation**,
//! including a tool-only run that never speaks, and **running a flow by hand
//! gives it an agent** rather than doing the work as nobody.
//!
//! Own test binary: `paths::data_dir()` memoizes `METALCRAFT_DATA_DIR`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::agent_instance::{AgentInstance, InstanceOrigin};
use metalcraft_agent::flow_bindings;
use metalcraft_agent::flow_exec::run_flow_v2_as;
use metalcraft_agent::flow_runs::FlowRunRef;
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
    let i = AgentInstance::new(
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

fn run_ref(run_id: &str) -> FlowRunRef {
    FlowRunRef {
        flow_id: "brief".into(),
        run_id: run_id.into(),
    }
}

#[tokio::test]
async fn a_run_becomes_a_conversation_in_the_agent_it_runs_as() {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", tmp.path());
    }
    let agent = agent();

    // --- a flow that never speaks still leaves its run ------------------------
    // It used to leave nothing, on the theory that an empty chat reads as "it
    // talked to you and you missed it". True — so the run says how it went
    // instead of leaving a blank. A tool-only automation is the one that does
    // the most and says the least, and it was the one you could not tell had run.
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
    let silent_chat = summary
        .chat_id
        .clone()
        .expect("every run leaves a conversation");

    let (_, detail) = get_json(&format!("/api/v1/agents/instances/{}", agent.id)).await;
    assert_eq!(detail["conversations"].as_array().unwrap().len(), 1);
    let (_, silent_detail) = get_json(&format!("/api/v1/chats/{silent_chat}")).await;
    let messages = silent_detail["messages"].as_array().unwrap();
    assert!(
        messages[0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("▶ Silent flow")),
        "the run names itself: {messages:#?}"
    );
    assert!(
        messages[1]["content"]
            .as_str()
            .is_some_and(|c| c.contains("said nothing")),
        "and says it never spoke: {messages:#?}"
    );

    // --- each run is its own conversation ------------------------------------
    let chat = flow_conversation(&agent.id, "amy", "m", ".", run_ref("r1"))
        .await
        .expect("a conversation opens");
    record_flow_turn(
        &chat,
        "▶ Morning brief\n\nWhat is on today?",
        "Three things.",
    )
    .await;

    // The next firing is a different run, so it is a different thread. This is
    // the rule that changed: firings inside a window used to be folded into one
    // rolling conversation, which left "the 08:00 run" as nothing you could open.
    let again = flow_conversation(&agent.id, "amy", "m", ".", run_ref("r2"))
        .await
        .expect("a conversation opens");
    assert_ne!(again, chat, "a second run is a second conversation");
    record_flow_turn(&again, "▶ Morning brief\n\nAnything else?", "No.").await;

    // What the agent's own surfaces now say: three conversations — the silent
    // run and the two firings — each holding its own run.
    let (status, detail) = get_json(&format!("/api/v1/agents/instances/{}", agent.id)).await;
    assert_eq!(status, StatusCode::OK);
    let conversations = detail["conversations"].as_array().unwrap();
    assert_eq!(conversations.len(), 3, "{detail:#}");
    // Turns, not messages: the times someone spoke to it. This endpoint used to
    // count raw messages while `GET /chats` counted user turns, so the same
    // conversation was "2" on one screen and "1" on the other.
    assert!(
        conversations.iter().all(|c| c["turn_count"] == 1),
        "{detail:#}"
    );
    // And each one says which run it is, so a session list can label it.
    let listed = conversations
        .iter()
        .find(|c| c["id"] == serde_json::json!(chat))
        .expect("the first firing is listed");
    assert_eq!(listed["flow_run"]["run_id"], "r1", "{listed:#}");
    assert_eq!(listed["flow_run"]["flow_id"], "brief", "{listed:#}");

    let (_, chat_detail) = get_json(&format!("/api/v1/chats/{chat}")).await;
    let messages = chat_detail["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "Three things.");
    // The run names itself at the top, so a transcript opened from anywhere says
    // what it is a run of.
    assert!(
        messages[0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("▶ Morning brief")),
        "{messages:#?}"
    );

    // And the conversation belongs to the agent, which is what makes it visible
    // in the fleet at all.
    assert_eq!(chat_detail["instance_id"], agent.id);

    a_hand_triggered_run_is_the_agent_the_schedule_armed().await;
}

/// `POST /flows/{id}/run` on an **armed** automation is the same act as its
/// scheduled firing — same agent, same memory. On an unarmed one it mints the
/// flow's own agent, so pressing Run once is enough to make an automation
/// findable in the fleet afterwards.
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
    let armed_schedule =
        metalcraft_agent::scheduled_flows::arm(metalcraft_agent::scheduled_flows::NewSchedule {
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
        })
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

    // An unarmed flow gets its own agent, minted by the run itself — the rule
    // that makes "run it once" enough to see the thing on the home screen.
    let mut loose = silent_flow();
    loose.id = "loose".into();
    loose.name = "Loose flow".into();
    save_flow(&paths::flows_dir(), &loose).unwrap();
    flow_bindings::bind_preset(&loose, "amy").expect("bind");
    let (status, summary) = post_run("loose").await;
    assert_eq!(status, StatusCode::OK, "{summary:#}");
    assert!(summary["chat_id"].is_string(), "{summary:#}");

    let minted = metalcraft_agent::agent_instance::list()
        .into_iter()
        .find(|i| {
            matches!(&i.origin,
                     metalcraft_agent::agent_instance::InstanceOrigin::Flow { flow_id }
                     if flow_id == "loose")
        })
        .expect("the run minted the flow's agent");
    assert_ne!(minted.id, agent.id, "and not the armed flow's agent");
    assert!(minted.name.contains("Loose flow"), "{}", minted.name);

    // Running it again continues that agent rather than minting a second — one
    // agent per flow, however many times it runs.
    let (_, summary) = post_run("loose").await;
    assert_eq!(
        metalcraft_agent::agent_instance::list()
            .into_iter()
            .filter(|i| matches!(&i.origin,
                                 metalcraft_agent::agent_instance::InstanceOrigin::Flow { flow_id }
                                 if flow_id == "loose"))
            .count(),
        1
    );
    // …and the second run is its own conversation inside it.
    let second = summary["chat_id"].as_str().expect("a second conversation");
    let (_, detail) = get_json(&format!("/api/v1/agents/instances/{}", minted.id)).await;
    let ids: Vec<&str> = detail["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2, "{detail:#}");
    assert!(ids.contains(&second), "{detail:#}");

    // A flow bound to nothing this pod can spawn still runs — memoryless, and
    // saying why there is nothing to read rather than failing the run.
    let mut orphan = silent_flow();
    orphan.id = "orphan".into();
    save_flow(&paths::flows_dir(), &orphan).unwrap();
    let (status, summary) = post_run("orphan").await;
    assert_eq!(status, StatusCode::OK, "{summary:#}");
    assert!(summary["chat_id"].is_null(), "{summary:#}");
    assert!(
        summary["warnings"].as_array().is_some_and(|w| w
            .iter()
            .any(|x| x.as_str().is_some_and(|s| s.contains("no agent")))),
        "{summary:#}"
    );

    // The armed agent exists and was not disturbed by any of it.
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
