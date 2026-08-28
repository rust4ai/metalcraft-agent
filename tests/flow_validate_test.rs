//! `POST /api/v1/flows/validate` — what is wrong with a graph, without saving it.
//!
//! `PUT /flows/{id}` has always validated before saving and remains the
//! authority. This endpoint exists so an editor can answer the question *while
//! someone is still typing*, which is the only point at which the answer can
//! still change what they do.
//!
//! The properties worth pinning are the ones an editor is built on: an invalid
//! graph is a **200 with `valid: false`**, not a 400 — the check ran, and what it
//! found is the body — and validating never writes anything, which is the whole
//! difference between this and a save.
//!
//! Own test binary: `paths::data_dir()` caches `METALCRAFT_DATA_DIR` in a
//! `OnceLock`, so parallel tests in one process would share a data dir whatever
//! they set — the same discipline as `flow_list_test`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::{paths, workshop_api};
use std::fs;

/// A graph that validates: one entry, one prompt, an edge between them.
const GOOD: &str = r#"{
  "spec_version": "3", "id": "brief", "name": "Morning brief",
  "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
  "flow": {
    "nodes": [
      { "id": "entry", "node_type": "entry", "data": {}, "position": [0, 0] },
      { "id": "compose", "node_type": "prompt", "data": { "prompt": "brief me" }, "position": [250, 0] }
    ],
    "edges": [
      { "id": "e1", "source": "entry", "target": "compose" }
    ]
  }
}"#;

async fn post(uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn validating_reports_what_is_wrong_and_writes_nothing() {
    let data_dir = std::env::temp_dir().join(format!("mc-flow-validate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    let flows_dir = paths::flows_dir();

    // ── a good graph ────────────────────────────────────────────────────────
    let (status, body) = post("/api/v1/flows/validate", GOOD).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], true, "{body:#}");
    assert_eq!(body["errors"].as_array().unwrap().len(), 0, "{body:#}");

    // Validating is not saving. An editor checks on every keystroke it can
    // afford to; if that persisted, typing would arm half-written automations.
    assert!(
        !flows_dir.join("brief.json").exists(),
        "validating must not write the flow"
    );

    // ── an edge pointing at a node that does not exist ──────────────────────
    let dangling = GOOD.replace(r#""target": "compose""#, r#""target": "nowhere""#);
    let (status, body) = post("/api/v1/flows/validate", &dangling).await;
    // Deliberately 200: an invalid graph is the *expected* answer to this
    // question, not a malformed request. A 400 here would make an editor's error
    // path indistinguishable from the pod being unreachable.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], false, "{body:#}");
    let errors = body["errors"].as_array().unwrap();
    assert!(!errors.is_empty(), "{body:#}");
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().contains("nowhere")),
        "the reason should name the node it could not find: {body:#}"
    );

    // ── two entry nodes ─────────────────────────────────────────────────────
    let two_entries = GOOD.replace(
        r#"{ "id": "compose", "node_type": "prompt", "data": { "prompt": "brief me" }, "position": [250, 0] }"#,
        r#"{ "id": "second", "node_type": "entry", "data": {}, "position": [250, 0] }"#,
    );
    let two_entries = two_entries.replace(r#""target": "compose""#, r#""target": "second""#);
    let (_, body) = post("/api/v1/flows/validate", &two_entries).await;
    assert_eq!(body["valid"], false, "{body:#}");

    // ── a vendor node type is not an error ──────────────────────────────────
    // SPEC §5.2: any `vendor:name` is valid and must round-trip. An editor that
    // reported someone's `slack:send_message` as broken would be telling them to
    // delete a node the runtime is perfectly happy with.
    let vendor = GOOD.replace(
        r#""node_type": "prompt""#,
        r#""node_type": "slack:send_message""#,
    );
    let (_, body) = post("/api/v1/flows/validate", &vendor).await;
    assert_eq!(body["valid"], true, "{body:#}");

    // ── the same graph still saves, and the save agrees ─────────────────────
    let router = workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .method("PUT")
            .uri("/api/v1/flows/brief")
            .header("authorization", "Bearer k")
            .header("content-type", "application/json")
            .body(Body::from(GOOD.to_string()))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(flows_dir.join("brief.json").exists(), "the save writes");

    let _ = fs::remove_dir_all(&data_dir);
}
