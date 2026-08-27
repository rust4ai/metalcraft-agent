//! `PUT /api/v1/flows/{id}` refuses a save built on a stale copy.
//!
//! Two people — or one person on a phone and a desktop — editing the same
//! automation is the ordinary case now that both clients can edit. Without a
//! precondition the second save silently erases the first, and neither person
//! sees anything: the flow simply does not contain what one of them wrote.
//!
//! `updated_at` is the precondition, and the pod owns it. A client sends back
//! the document it loaded; a mismatch means the flow moved underneath it.
//!
//! Own test binary: `paths::data_dir()` caches `METALCRAFT_DATA_DIR` in a
//! `OnceLock`, so parallel tests in one process would share a data dir whatever
//! they set — the same discipline as `flow_validate_test`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::{paths, workshop_api};
use std::fs;

fn flow_json(updated_at: &str, prompt: &str) -> String {
    format!(
        r#"{{
      "spec_version": "3", "id": "brief", "name": "Morning brief",
      "created_at": "2026-01-01T00:00:00Z", "updated_at": "{updated_at}",
      "flow": {{
        "nodes": [
          {{ "id": "entry", "node_type": "entry", "data": {{}} }},
          {{ "id": "compose", "node_type": "prompt", "data": {{ "prompt": "{prompt}" }} }}
        ],
        "edges": [{{ "id": "e1", "source": "entry", "target": "compose" }}]
      }}
    }}"#
    )
}

async fn put(body: &str) -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .method("PUT")
            .uri("/api/v1/flows/brief")
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
async fn a_save_from_a_stale_copy_is_refused_and_the_first_edit_survives() {
    let data_dir = std::env::temp_dir().join(format!("mc-flow-conflict-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    // ── creating: nothing to conflict with ──────────────────────────────────
    let (status, created) = put(&flow_json("2026-01-01T00:00:00Z", "first")).await;
    assert_eq!(status, StatusCode::OK, "{created:#}");

    // The pod stamps `updated_at` itself. A client that could choose its own
    // could hand back the value it read and defeat the check without meaning to.
    let stamped = created["updated_at"].as_str().unwrap().to_string();
    assert_ne!(
        stamped, "2026-01-01T00:00:00Z",
        "the save must be stamped by the pod, not by the caller"
    );

    // ── two people open it, and the first one saves ─────────────────────────
    let both_loaded = stamped.clone();
    let (status, first) = put(&flow_json(&both_loaded, "edited by the first")).await;
    assert_eq!(status, StatusCode::OK, "{first:#}");

    // ── the second saves from the copy they loaded before that ──────────────
    let (status, body) = put(&flow_json(&both_loaded, "edited by the second")).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a save built on a superseded copy must be refused: {body:#}"
    );
    // The message has to be actionable, which means naming that it moved rather
    // than reporting a bare code somebody has to go and interpret.
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("changed since you opened it"), "{error}");
    assert!(error.contains(&both_loaded), "{error}");

    // ── and the first edit is still there ───────────────────────────────────
    // The whole point. A last-writer-wins save would have replaced it with the
    // second person's copy and told nobody.
    let saved: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(paths::flows_dir().join("brief.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved["flow"]["nodes"][1]["data"]["prompt"],
        "edited by the first"
    );

    // ── reloading and saving again works ────────────────────────────────────
    let current = saved["updated_at"].as_str().unwrap();
    let (status, _) = put(&flow_json(current, "edited by the second, properly")).await;
    assert_eq!(status, StatusCode::OK);

    let _ = fs::remove_dir_all(&data_dir);
}
