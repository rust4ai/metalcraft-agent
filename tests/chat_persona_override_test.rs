//! Starting a chat as a specific persona must not repoint the agent.
//!
//! `POST /api/v1/chats` accepts an optional `persona_slug` — the escape hatch for
//! clients that still name a persona directly. It used to write that choice back to
//! the agent instance, so starting one conversation as a named persona silently
//! changed that agent's persona for every conversation after it. Nobody asked for
//! that; moving an agent's persona is `PATCH /api/v1/agents/{id}`.
//!
//! Everything runs inside ONE `#[test]` so the process-global `METALCRAFT_DATA_DIR`
//! isn't raced by parallel tests — the same discipline as `agent_preset_instance_test`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::agent_instance::{self, AgentInstance, InstanceOrigin};
use metalcraft_agent::agent_preset::AgentPreset;
use metalcraft_agent::workshop_api;
use std::fs;
use std::path::Path;
use tower::ServiceExt;

const PRESET: &str = r#"{
  "slug": "amy-kitchen",
  "name": "Amy's Kitchen Agent",
  "description": "A chef agent",
  "default_persona": "amy",
  "personas": [
    { "slug": "amy", "role": "default" },
    { "slug": "amy-shopper", "role": "subagent" }
  ]
}"#;

fn persona_json(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","description":"t","tools":[],"integrations":[],"skills":[],"system_prompt":"You are {name}."}}"#
    )
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// `POST /api/v1/chats` with the given JSON body.
async fn create_chat(body: &str) -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/chats")
                .header("authorization", "Bearer k")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn an_explicit_persona_scopes_to_the_chat_not_the_agent() {
    let data_dir = std::env::temp_dir().join(format!("mc-chat-persona-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let presets_dir = data_dir.join("agent_presets");
    write(&presets_dir.join("amy-kitchen.json"), PRESET);
    let personas_dir = data_dir.join("personas");
    write(&personas_dir.join("amy.json"), &persona_json("amy"));
    write(
        &personas_dir.join("amy-shopper.json"),
        &persona_json("amy-shopper"),
    );

    let preset = AgentPreset::load("amy-kitchen", &presets_dir).expect("load preset");
    let instance = AgentInstance::new(&preset, InstanceOrigin::Workshop);
    instance.save().expect("save instance");
    assert_eq!(
        instance.persona, "amy",
        "an instance starts at the preset default"
    );

    // The regression: continue this agent, but start the chat as another persona in
    // its roster.
    let (status, chat) = create_chat(&format!(
        r#"{{"instance_id":"{}","persona_slug":"amy-shopper"}}"#,
        instance.id
    ))
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an in-roster persona is allowed: {chat}"
    );
    assert_eq!(
        chat["persona_slug"], "amy-shopper",
        "the conversation runs as the persona that was asked for"
    );

    let after = agent_instance::load(&instance.id).expect("reload instance");
    assert_eq!(
        after.persona, "amy",
        "the agent itself must be untouched — one chat is not a persona move"
    );

    // A second chat with nothing named still gets the agent's own persona, which is
    // the property the old behaviour destroyed.
    let (status, plain) = create_chat(&format!(r#"{{"instance_id":"{}"}}"#, instance.id)).await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert_eq!(
        plain["persona_slug"], "amy",
        "the next conversation must not inherit the previous one's override"
    );

    // Containment still holds: outside the roster is refused, not silently accepted.
    let (status, err) = create_chat(&format!(
        r#"{{"instance_id":"{}","persona_slug":"orchestrator-agent"}}"#,
        instance.id
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    let _ = fs::remove_dir_all(&data_dir);
}
