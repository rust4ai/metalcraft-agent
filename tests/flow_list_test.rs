//! `GET /api/v1/flows` — the listing the API did not have.
//!
//! Until this endpoint existed a client had to already know a flow's id to see
//! anything, so "what is this pod set up to do" was unanswerable. The two
//! properties worth pinning down are the ones a UI is built on: **disabled flows
//! are listed** (packs ship them disabled, so an unarmed flow is the normal case),
//! and each schedule reports **which agent it was armed with**.
//!
//! Own test binary, one `#[test]`: `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so parallel tests in one process would
//! share a data dir whatever they set — the same discipline as
//! `chat_persona_override_test`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::{flow_bindings, paths, workshop_api};
use std::fs;

const PRESET: &str = r#"{
  "slug": "amy-kitchen",
  "name": "Amy's Kitchen Agent",
  "description": "A chef agent",
  "default_persona": "amy",
  "personas": [{ "slug": "amy", "role": "default" }]
}"#;

/// A v2 flow with a cron schedule, a manual one, and one whose cron cannot parse.
///
/// Note the six fields: the `cron` crate wants seconds, so the five-field POSIX
/// form in `docs/FLOW_SCHEDULES_PLAN.md`'s example is rejected. `"broken"` pins
/// what a client sees when an author writes one anyway.
fn flow_json(id: &str, name: &str, enabled: bool) -> String {
    format!(
        r#"{{
      "spec_version": "2", "id": "{id}", "name": "{name}",
      "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-0{stamp}T00:00:00Z",
      "enabled": {enabled},
      "schedules": [
        {{ "id": "morning", "name": "Morning brief", "type": "cron", "cron": "0 0 8 * * *", "enabled": true }},
        {{ "id": "adhoc", "name": "On demand", "type": "manual", "enabled": true }},
        {{ "id": "broken", "name": "Five-field cron", "type": "cron", "cron": "0 8 * * *", "enabled": true }}
      ],
      "flow": {{ "nodes": [
        {{ "id": "entry", "node_type": "entry", "data": {{ "persona": "amy" }}, "position": [0,0] }},
        {{ "id": "compose", "node_type": "prompt", "data": {{ "persona": "amy", "prompt": "brief me" }}, "position": [1,0] }}
      ], "edges": [] }}
    }}"#,
        stamp = if enabled { "2" } else { "1" },
    )
}

async fn get_flows() -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/flows")
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
async fn the_listing_shows_disabled_flows_and_which_agent_each_schedule_runs_as() {
    let data_dir = std::env::temp_dir().join(format!("mc-flow-list-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let presets_dir = paths::agent_presets_dir();
    fs::create_dir_all(&presets_dir).unwrap();
    fs::write(presets_dir.join("amy-kitchen.json"), PRESET).unwrap();

    let flows_dir = paths::flows_dir();
    let armed: metalcraft_flows::SavedFlow =
        serde_json::from_str(&flow_json("brief", "Morning brief", true)).unwrap();
    // Disabled is the shipped-in-a-pack case, and the one a naive listing drops.
    let dormant: metalcraft_flows::SavedFlow =
        serde_json::from_str(&flow_json("prep", "Sunday prep", false)).unwrap();
    metalcraft_flows::save_flow(&flows_dir, &armed).unwrap();
    metalcraft_flows::save_flow(&flows_dir, &dormant).unwrap();

    flow_bindings::bind_preset(&armed, "amy-kitchen").expect("bind");
    let agent = flow_bindings::arm(&armed, "morning", None).expect("arm");

    let (status, body) = get_flows().await;
    assert_eq!(status, StatusCode::OK);
    let flows = body["flows"].as_array().expect("flows array");

    // Both flows, disabled included.
    assert_eq!(flows.len(), 2, "{body:#}");
    let by_id = |id: &str| {
        flows
            .iter()
            .find(|f| f["id"] == id)
            .unwrap_or_else(|| panic!("flow '{id}' missing from {body:#}"))
    };
    let brief = by_id("brief");
    let prep = by_id("prep");
    assert_eq!(prep["enabled"], false);
    assert_eq!(brief["enabled"], true);

    // The flow's agent, and the fact that it has one at all.
    assert_eq!(brief["preset"], "amy-kitchen");
    assert_eq!(brief["armed"], true);
    assert_eq!(brief["v2"], true);
    assert_eq!(brief["node_count"], 2);

    // An unbound flow still reports the agent it would run as, rather than null.
    assert_eq!(prep["armed"], false);
    assert!(
        prep["preset"].as_str().is_some_and(|p| !p.is_empty()),
        "an unbound flow resolves to the default agent: {prep:#}"
    );

    let schedules = brief["schedules"].as_array().expect("schedules");
    assert_eq!(schedules.len(), 3);
    let morning = &schedules[0];
    let adhoc = &schedules[1];
    let broken = &schedules[2];

    // The armed schedule names its agent; the unarmed one says nothing rather
    // than pointing at the flow's other agent.
    assert_eq!(morning["schedule_id"], serde_json::Value::Null); // flattened spec uses `id`
    assert_eq!(morning["id"], "morning");
    assert_eq!(morning["instance_id"], agent.id);
    assert_eq!(morning["instance_name"], agent.name);
    assert!(adhoc["instance_id"].is_null(), "{adhoc:#}");

    // The flattened spec is the stored shape, so an editor can round-trip it.
    assert_eq!(morning["type"], "cron");
    assert_eq!(morning["cron"], "0 0 8 * * *");
    assert_eq!(morning["name"], "Morning brief");

    // A cron projects a next fire; a manual trigger has none to project.
    assert!(
        morning["next_fire_at"].as_str().is_some(),
        "cron should project a next run: {morning:#}"
    );
    assert!(adhoc["next_fire_at"].is_null(), "{adhoc:#}");
    assert!(
        adhoc["description"]
            .as_str()
            .is_some_and(|d| d.contains("Manual")),
        "{adhoc:#}"
    );

    // A cron the parser rejects says so in `description` rather than going quiet:
    // a schedule that will never fire should be visibly broken, not merely empty.
    assert!(
        broken["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("Invalid cron")),
        "{broken:#}"
    );
    assert!(broken["next_fire_at"].is_null(), "{broken:#}");

    let _ = fs::remove_dir_all(&data_dir);
}
