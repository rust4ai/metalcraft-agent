//! The HTTP half of the resident-memory work: the instrument, and the two
//! ceilings that live on the router.
//!
//! Split from `memory_bounds_test.rs` because that binary installs a counting
//! global allocator and drives no runtime; this one needs tokio and an axum
//! router and would only add noise to those measurements.
//!
//! What is worth pinning here is mostly *the instrument*. `GET /api/v1/metrics`
//! is how anyone decides whether a pod's memory problem is chat residency or
//! allocator high-water, so a version of it that quietly reports zeros is worse
//! than not having it: it would answer "not the chats" to every question. Hence
//! the first test asserts on a chat it actually created.
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

/// Send one request through a fresh router.
///
/// A new router each time is fine and deliberate: the chat store and the
/// broadcaster registry are both process-global `OnceLock`s, so every router
/// this builds talks to the same state a real pod would have.
async fn send(req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let res = workshop_api::build_router("k".into())
        .oneshot(req)
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

fn authed(method: &str, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer k")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn metrics() -> serde_json::Value {
    let (status, body) = send(authed("GET", "/api/v1/metrics", "")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn the_memory_instrument_and_its_ceilings() {
    let data_dir = std::env::temp_dir().join(format!("mc-mem-api-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    // SAFETY: set before the first `data_dir()` call, which caches it.
    unsafe { std::env::set_var("METALCRAFT_DATA_DIR", &data_dir) };

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

    // ── The instrument reports the pod it is actually in ────────────────
    let before = metrics().await;
    assert_eq!(before["chats"]["resident"], 0);
    assert!(
        before["limits"]["max_tool_result_bytes"].as_u64().unwrap() > 0,
        "the limits block must echo what the pod is running with: {before}"
    );

    let (status, chat) = send(authed(
        "POST",
        "/api/v1/chats",
        &format!(r#"{{"instance_id":"{}"}}"#, instance.id),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{chat}");
    let id = chat["id"].as_str().expect("chat id").to_string();

    let after = metrics().await;
    assert_eq!(
        after["chats"]["resident"], 1,
        "a chat was created and the instrument did not see it: {after}"
    );
    assert_eq!(
        after["chats"]["unmeasured"], 0,
        "nothing held a session lock, so nothing should have been skipped: {after}"
    );
    assert_eq!(after["chats"]["turns_in_flight"], 0);

    // ── The event bus goes when the last subscriber does ────────────────
    //
    // The registry was append-only, so this is the regression that matters: a
    // 64-slot ring of events per chat id the process ever touched, on a pod
    // where gateway ids churn all day. The cleanup hangs off the SSE stream's
    // `Drop`, which is easy to break by refactoring the handler and impossible
    // to notice by hand.
    assert_eq!(
        metrics().await["broadcasters"]["count"],
        0,
        "no bus should exist before anyone subscribes"
    );

    let res = workshop_api::build_router("k".into())
        .oneshot(authed("GET", &format!("/api/v1/chats/{id}/events"), ""))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Holding the response body open is what holds the subscription open.
    let subscription = res.into_body();
    let during = metrics().await;
    assert_eq!(
        during["broadcasters"], serde_json::json!({"count": 1, "subscribed": 1}),
        "an open SSE stream should be one subscribed bus: {during}"
    );

    drop(subscription);
    // The cleanup is spawned from `Drop` rather than done inside it, so it lands
    // on the next pass of the scheduler rather than synchronously.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let idle = metrics().await;
    assert_eq!(
        idle["broadcasters"], serde_json::json!({"count": 0, "subscribed": 0}),
        "the bus outlived its last subscriber: {idle}"
    );

    // ── Deleting a chat takes its bus and its residency with it ─────────
    let (status, _) = send(authed("DELETE", &format!("/api/v1/chats/{id}"), "")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let gone = metrics().await;
    assert_eq!(gone["chats"]["resident"], 0, "{gone}");
    assert_eq!(gone["broadcasters"]["count"], 0, "{gone}");

    // ── A request body has a ceiling ────────────────────────────────────
    //
    // `/webhook/gateway` is the one that matters: it is unauthenticated by
    // design (provenance comes from the per-channel HMAC), so without a limit
    // anyone who can reach the pod can ask it to hold an arbitrary amount of
    // memory before the signature is ever checked.
    let limit = metalcraft_agent::resources::max_request_body_bytes();
    let oversized = "x".repeat(limit + 1_024);
    let res = workshop_api::build_router("k".into())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/gateway")
                .header("content-type", "application/json")
                .body(Body::from(oversized))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "an over-limit body should be refused before it is read"
    );

    // A body under the ceiling still reaches the handler, which then rejects it
    // on its own terms — the limit must not be doing the handler's job.
    let res = workshop_api::build_router("k".into())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/gateway")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"not":"a valid webhook"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        res.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "an ordinary body was refused as oversized"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
