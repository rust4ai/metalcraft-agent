//! `POST /api/v1/chats/{id}/interrupt` — the stop button's endpoint.
//!
//! What is worth pinning here is what the endpoint *says*, because the client
//! decides what to show the user from it. Stopping an idle chat is a race, not a
//! failure: the turn can finish between the press and the request, and answering
//! `409` there would put an error in the transcript for a turn that ended
//! normally. Stopping a chat that does not exist is a genuine miss and stays a
//! `404`.
//!
//! The busy path (a real turn, actually halted) needs a live model, so it is
//! covered by the guard's unit test in `workshop_api::interrupt_tests` instead.
//!
//! One `#[test]`, like the other chat API tests: `METALCRAFT_DATA_DIR` is
//! process-global and parallel tests would race it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::agent_instance::{AgentInstance, InstanceOrigin};
use metalcraft_agent::agent_preset::AgentPreset;
use metalcraft_agent::workshop_api;
use std::fs;
use std::path::Path;
use tower::ServiceExt;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

async fn post(uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
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
async fn stopping_an_idle_chat_is_an_answer_not_an_error() {
    let data_dir = std::env::temp_dir().join(format!("mc-chat-interrupt-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    write(
        &data_dir.join("personas").join("amy.json"),
        r#"{"name":"amy","description":"t","tools":[],"integrations":[],"skills":[],"system_prompt":"You are amy."}"#,
    );
    let presets_dir = data_dir.join("agent_presets");
    write(
        &presets_dir.join("amy-kitchen.json"),
        r#"{"slug":"amy-kitchen","name":"Amy's Kitchen Agent","description":"A chef agent","default_persona":"amy","personas":[{"slug":"amy","role":"default"}]}"#,
    );
    let preset = AgentPreset::load("amy-kitchen", &presets_dir).expect("load preset");
    let instance = AgentInstance::new(&preset, InstanceOrigin::Workshop);
    instance.save().expect("save instance");

    let (status, chat) = post(
        "/api/v1/chats",
        &format!(r#"{{"instance_id":"{}"}}"#, instance.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{chat}");
    let id = chat["id"].as_str().expect("chat id").to_string();

    let (status, body) = post(&format!("/api/v1/chats/{id}/interrupt"), "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["stopping"], false,
        "nothing was running, and the endpoint must say so rather than fail"
    );

    let (status, body) = post("/api/v1/chats/no-such-chat/interrupt", "").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a chat that does not exist is a real miss: {body}"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
