//! `GET /api/v1/flows` and `GET /api/v1/scheduled-flows` — the two listings a
//! client joins to answer "what is this pod set up to do, and what will it
//! actually do".
//!
//! The properties worth pinning are the ones a UI is built on: an unscheduled
//! flow is still listed (packs ship flows with nothing scheduled, so that is the
//! normal case), a schedule reports which agent it was armed with, and a schedule
//! that can never fire says so rather than merely showing nothing.
//!
//! Own test binary, one `#[test]`: `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so parallel tests in one process would
//! share a data dir whatever they set — the same discipline as
//! `chat_persona_override_test`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metalcraft_agent::{flow_bindings, paths, scheduled_flows, workshop_api};
use metalcraft_flows::{ScheduleSpec, ScheduleTrigger};
use std::fs;

const PRESET: &str = r#"{
  "slug": "amy-kitchen",
  "name": "Amy's Kitchen Agent",
  "description": "A chef agent",
  "default_persona": "amy",
  "personas": [{ "slug": "amy", "role": "default" }]
}"#;

fn flow_json(id: &str, name: &str, stamp: &str) -> String {
    format!(
        r#"{{
      "spec_version": "3", "id": "{id}", "name": "{name}",
      "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-0{stamp}T00:00:00Z",
      "flow": {{ "nodes": [
        {{ "id": "entry", "node_type": "entry", "data": {{ "persona": "amy" }}, "position": [0,0] }},
        {{ "id": "compose", "node_type": "prompt", "data": {{ "persona": "amy", "prompt": "brief me" }}, "position": [1,0] }}
      ], "edges": [] }}
    }}"#
    )
}

fn spec(name: &str, trigger: ScheduleTrigger) -> ScheduleSpec {
    ScheduleSpec {
        trigger,
        name: Some(name.to_string()),
        timezone: None,
        inputs: None,
        persona: None,
    }
}

async fn get(uri: &str) -> (StatusCode, serde_json::Value) {
    let router = workshop_api::build_router("k".into());
    let res = tower::ServiceExt::oneshot(
        router,
        Request::builder()
            .method("GET")
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
async fn the_listings_show_unscheduled_flows_and_which_agent_each_schedule_runs_as() {
    let data_dir = std::env::temp_dir().join(format!("mc-flow-list-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let presets_dir = paths::agent_presets_dir();
    fs::create_dir_all(&presets_dir).unwrap();
    fs::write(presets_dir.join("amy-kitchen.json"), PRESET).unwrap();

    let flows_dir = paths::flows_dir();
    let brief: metalcraft_flows::SavedFlow =
        serde_json::from_str(&flow_json("brief", "Morning brief", "2")).unwrap();
    // Nothing scheduled is the shipped-in-a-pack case, and the one a naive
    // listing drops.
    let prep: metalcraft_flows::SavedFlow =
        serde_json::from_str(&flow_json("prep", "Sunday prep", "1")).unwrap();
    metalcraft_flows::save_flow(&flows_dir, &brief).unwrap();
    metalcraft_flows::save_flow(&flows_dir, &prep).unwrap();

    flow_bindings::bind_preset(&brief, "amy-kitchen").expect("bind");
    let morning = scheduled_flows::arm(scheduled_flows::NewSchedule {
        flow: &brief,
        schedule: spec(
            "Morning brief",
            ScheduleTrigger::Cron {
                cron: "0 0 8 * * *".into(),
            },
        ),
        enabled: true,
        instance: None,
        from_suggestion: None,
        id: None,
    })
    .expect("arm");
    let adhoc = scheduled_flows::arm(scheduled_flows::NewSchedule {
        flow: &brief,
        schedule: spec("On demand", ScheduleTrigger::Manual),
        enabled: true,
        instance: None,
        from_suggestion: None,
        id: None,
    })
    .expect("arm manual");

    // A five-field POSIX cron: this parser wants seconds, so it will never fire.
    // `arm` refuses it — but migration can carry one in, so the listing still has
    // to render it, and it must read as broken rather than as quiet.
    let mut broken = morning.clone();
    broken.id = "sf_broken".into();
    broken.schedule = spec(
        "Five-field cron",
        ScheduleTrigger::Cron {
            cron: "0 8 * * *".into(),
        },
    );
    let refused = scheduled_flows::save(&broken).expect_err("a cron this pod can't parse");
    assert!(refused.contains("invalid cron"), "{refused}");
    metalcraft_flows::save_scheduled_flow(&paths::scheduled_flows_dir(), &broken)
        .expect("but a legacy one can land on disk");

    // ── the flow listing: what this pod can do ────────────────────────────────
    let (status, body) = get("/api/v1/flows").await;
    assert_eq!(status, StatusCode::OK);
    let flows = body["flows"].as_array().expect("flows array");
    assert_eq!(flows.len(), 2, "unscheduled flows are listed too: {body:#}");

    let by_id = |id: &str| {
        flows
            .iter()
            .find(|f| f["id"] == id)
            .unwrap_or_else(|| panic!("flow '{id}' missing from {body:#}"))
    };
    let brief_row = by_id("brief");
    let prep_row = by_id("prep");

    assert_eq!(brief_row["preset"], "amy-kitchen");
    assert_eq!(brief_row["v2"], true);
    assert_eq!(brief_row["node_count"], 2);
    assert_eq!(brief_row["scheduled_count"], 3);
    assert_eq!(brief_row["enabled_count"], 3);

    // Nothing points at `prep`, which is exactly how "this will never run on its
    // own" is now expressed — no flag to disagree with.
    assert_eq!(prep_row["scheduled_count"], 0);
    assert_eq!(prep_row["enabled_count"], 0);
    assert!(
        prep_row["preset"].as_str().is_some_and(|p| !p.is_empty()),
        "an unbound flow still resolves to the default agent: {prep_row:#}"
    );

    // ── the schedule listing: what this pod will do ───────────────────────────
    let (status, body) = get("/api/v1/scheduled-flows").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["scheduled"].as_array().expect("scheduled array");
    assert_eq!(rows.len(), 3, "{body:#}");
    let row = |id: &str| {
        rows.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("schedule '{id}' missing from {body:#}"))
    };

    // The stored document is flattened into the row, so an editor round-trips it.
    let morning_row = row(&morning.id);
    assert_eq!(morning_row["flow_id"], "brief");
    assert_eq!(morning_row["flow_name"], "Morning brief");
    assert_eq!(morning_row["enabled"], true);
    assert_eq!(morning_row["schedule"]["type"], "cron");
    assert_eq!(morning_row["schedule"]["cron"], "0 0 8 * * *");
    assert_eq!(morning_row["schedule"]["name"], "Morning brief");

    // Which agent runs it, by name — the question the listing exists to answer.
    let agent_id = morning.instance_id.as_deref().unwrap();
    assert_eq!(morning_row["instance_id"], agent_id);
    assert!(
        morning_row["instance_name"]
            .as_str()
            .is_some_and(|n| n.contains("Morning")),
        "{morning_row:#}"
    );

    // A cron projects a next fire; a manual trigger has none to project.
    assert!(
        morning_row["next_fire_at"].as_str().is_some(),
        "{morning_row:#}"
    );
    let adhoc_row = row(&adhoc.id);
    assert!(adhoc_row["next_fire_at"].is_null(), "{adhoc_row:#}");
    assert!(
        adhoc_row["description"]
            .as_str()
            .is_some_and(|d| d.contains("Manual")),
        "{adhoc_row:#}"
    );

    // A cron the parser rejects says so in `description` rather than going quiet.
    let broken_row = row("sf_broken");
    assert!(
        broken_row["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("Invalid cron")),
        "{broken_row:#}"
    );
    assert!(broken_row["next_fire_at"].is_null(), "{broken_row:#}");

    // ── filtered by flow, which is how a flow's detail view asks ──────────────
    let (_, body) = get("/api/v1/scheduled-flows?flow_id=prep").await;
    assert_eq!(body["scheduled"].as_array().unwrap().len(), 0);
    let (_, body) = get(&format!("/api/v1/scheduled-flows?instance_id={agent_id}")).await;
    assert_eq!(
        body["scheduled"].as_array().unwrap().len(),
        3,
        "all three schedules of one flow share its agent: {body:#}"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
