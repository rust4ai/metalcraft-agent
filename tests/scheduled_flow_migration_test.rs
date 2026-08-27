//! Lifting scheduling out of pre-v3 flow documents.
//!
//! The property everything here defends: **migration never starts something that
//! was not already running.** A pod upgrading overnight must wake up doing
//! exactly what it was doing, and a flow that was switched off must stay off even
//! though the switch it was off by no longer exists.
//!
//! Own test binary, one `#[test]`: `paths::data_dir()` caches
//! `METALCRAFT_DATA_DIR` in a `OnceLock`, so parallel tests in one process would
//! share a data dir whatever they set.

use metalcraft_agent::{paths, scheduled_flows};
use std::fs;

/// A pre-v3 flow document, with whatever scheduling the caller wants on it.
fn legacy_flow(id: &str, spec_version: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut doc = serde_json::json!({
        "spec_version": spec_version,
        "id": id,
        "name": format!("Flow {id}"),
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "flow": { "nodes": [
            { "id": "entry", "node_type": "entry", "data": {}, "position": [0, 0] },
            { "id": "p", "node_type": "prompt", "data": { "prompt": "hi" }, "position": [1, 0] }
        ], "edges": [ { "id": "e", "source": "entry", "target": "p" } ] }
    });
    let (serde_json::Value::Object(base), serde_json::Value::Object(more)) = (&mut doc, extra)
    else {
        panic!("objects");
    };
    for (k, v) in more {
        base.insert(k, v);
    }
    doc
}

fn write_flow(doc: &serde_json::Value) {
    let dir = paths::flows_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{}.json", doc["id"].as_str().unwrap())),
        serde_json::to_vec_pretty(doc).unwrap(),
    )
    .unwrap();
}

/// The legacy arming record: `flow id → { preset, instances: { schedule id → agent } }`.
fn write_bindings(bindings: serde_json::Value) {
    fs::create_dir_all(paths::data_dir()).unwrap();
    fs::write(
        paths::data_dir().join("flow_bindings.json"),
        serde_json::to_vec_pretty(&serde_json::json!({ "flows": bindings })).unwrap(),
    )
    .unwrap();
}

fn read_flow_json(id: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(paths::flows_dir().join(format!("{id}.json"))).unwrap())
        .unwrap()
}

/// Migrated schedules of one flow, ordered by their legacy key so assertions can
/// name them. Ids are generated, so the key is the only stable handle here.
fn by_key(flow_id: &str) -> Vec<(String, metalcraft_flows::ScheduledFlow)> {
    let mut v: Vec<(String, metalcraft_flows::ScheduledFlow)> = scheduled_flows::for_flow(flow_id)
        .into_iter()
        .map(|sf| (sf.from_suggestion.clone().unwrap_or_default(), sf))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

#[test]
fn pre_v3_flows_migrate_without_starting_or_stopping_anything() {
    let data_dir = std::env::temp_dir().join(format!("mc-sf-migrate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    // 1. Two crons, one of them switched off, on a flow that was running. One is
    //    armed to an agent.
    write_flow(&legacy_flow(
        "brief",
        "2",
        serde_json::json!({
            "enabled": true,
            "schedules": [
                { "id": "morning", "name": "Morning brief", "type": "cron", "cron": "0 0 8 * * *",
                  "timezone": "America/Detroit", "persona": "briefer", "inputs": { "depth": "short" } },
                { "id": "evening", "type": "cron", "cron": "0 0 18 * * *", "enabled": false }
            ]
        }),
    ));

    // 2. A flow whose master switch was off. Both schedules look live; neither was.
    write_flow(&legacy_flow(
        "dormant",
        "2",
        serde_json::json!({
            "enabled": false,
            "schedules": [
                { "id": "a", "type": "cron", "cron": "0 0 8 * * *", "enabled": true },
                { "id": "b", "type": "minutes", "interval": 5, "enabled": true }
            ]
        }),
    ));

    // 3. A v1 flow with its trigger on the entry node.
    let mut v1 = legacy_flow("legacy", "1", serde_json::json!({ "enabled": true }));
    v1["flow"]["nodes"][0]["data"] = serde_json::json!({
        "schedule_type": "cron", "cron": "0 0 9 * * *", "persona": "briefer", "max_steps": 40
    });
    write_flow(&v1);

    // 4. Manual schedules: one armed, one not.
    write_flow(&legacy_flow(
        "handrun",
        "2",
        serde_json::json!({
            "enabled": true,
            "schedules": [
                { "id": "armed-manual", "type": "manual" },
                { "id": "loose-manual", "type": "manual" }
            ]
        }),
    ));

    // 5. A flow that was never scheduled at all.
    write_flow(&legacy_flow("plain", "2", serde_json::json!({})));

    write_bindings(serde_json::json!({
        "brief": { "preset": "amy", "instances": { "morning": "inst_morning" } },
        "handrun": { "preset": "amy", "instances": { "armed-manual": "inst_hand" } },
    }));

    let report = scheduled_flows::migrate_from_flows();
    assert_eq!(report.failed, 0, "{report:?}");

    // ── 1. both schedules migrate; only the one that was firing still fires ──
    let brief = by_key("brief");
    assert_eq!(brief.len(), 2, "{brief:#?}");
    let (morning_key, morning) = &brief[1];
    let (evening_key, evening) = &brief[0];
    assert_eq!(morning_key, "morning");
    assert_eq!(evening_key, "evening");

    assert!(morning.enabled);
    assert!(!evening.enabled, "a schedule that was off stays off");
    assert_eq!(morning.instance_id.as_deref(), Some("inst_morning"));
    assert!(
        evening.instance_id.is_none(),
        "an unarmed schedule gains no agent"
    );
    assert_eq!(
        morning.schedule.timezone.as_deref(),
        Some("America/Detroit")
    );
    assert_eq!(morning.schedule.persona.as_deref(), Some("briefer"));
    assert_eq!(
        morning.schedule.inputs,
        Some(serde_json::json!({ "depth": "short" }))
    );
    assert!(morning.id.starts_with("sf_"), "ids are generated: {}", morning.id);

    // The flow itself is now a graph and nothing else.
    let doc = read_flow_json("brief");
    assert_eq!(doc["spec_version"], "3");
    assert!(doc.get("schedules").is_none());
    assert!(doc.get("enabled").is_none());

    // ── 2. a flow that was off migrates to schedules that are off ───────────
    let dormant = by_key("dormant");
    assert_eq!(dormant.len(), 2);
    assert!(
        dormant.iter().all(|(_, sf)| !sf.enabled),
        "the master switch was off, so nothing may start firing: {dormant:#?}"
    );

    // ── 3. the v1 entry-node trigger becomes one schedule ────────────────────
    let legacy = by_key("legacy");
    assert_eq!(legacy.len(), 1);
    let (key, sf) = &legacy[0];
    assert_eq!(key, "default");
    assert!(sf.enabled);
    assert_eq!(sf.schedule.persona.as_deref(), Some("briefer"));
    assert!(matches!(
        &sf.schedule.trigger,
        metalcraft_flows::ScheduleTrigger::Cron { cron } if cron == "0 0 9 * * *"
    ));
    // The dead scheduling keys are stripped; everything else on the node stays.
    let entry = &read_flow_json("legacy")["flow"]["nodes"][0]["data"];
    assert!(entry.get("schedule_type").is_none());
    assert!(entry.get("cron").is_none());
    assert_eq!(entry["max_steps"], 40);
    assert_eq!(entry["persona"], "briefer");

    // ── 4. an armed manual schedule is real state; an unarmed one is not ─────
    let handrun = by_key("handrun");
    assert_eq!(
        handrun.len(),
        1,
        "only the armed manual schedule is worth a document: {handrun:#?}"
    );
    assert_eq!(handrun[0].0, "armed-manual");
    assert_eq!(handrun[0].1.instance_id.as_deref(), Some("inst_hand"));

    // ── 5. a flow nobody scheduled gains nothing, and still exists ───────────
    assert!(scheduled_flows::for_flow("plain").is_empty());
    assert_eq!(read_flow_json("plain")["spec_version"], "3");

    // ── the legacy arming record is drained, and the preset kept ─────────────
    let bindings: serde_json::Value = serde_json::from_slice(
        &fs::read(paths::data_dir().join("flow_bindings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bindings["flows"]["brief"]["preset"], "amy");
    assert!(
        bindings["flows"]["brief"].get("instances").is_none(),
        "the per-schedule agent map has moved onto the schedules: {bindings:#}"
    );

    // ── 6. running it again changes nothing ──────────────────────────────────
    let before: Vec<String> = scheduled_flows::list().into_iter().map(|sf| sf.id).collect();
    let again = scheduled_flows::migrate_from_flows();
    assert_eq!(again.created, 0, "a second pass must mint nothing");
    assert_eq!(again.failed, 0);
    let after: Vec<String> = scheduled_flows::list().into_iter().map(|sf| sf.id).collect();
    assert_eq!(before, after, "ids are generated, so a re-run must not duplicate");

    // ── 7. an unnamed schedule keeps its old id as a label ───────────────────
    // `evening` shipped without a `name`; without this it would migrate into a
    // blank row in every listing.
    assert_eq!(evening.schedule.name.as_deref(), Some("evening"));
    assert_eq!(morning.schedule.name.as_deref(), Some("Morning brief"));

    // ── a broken document is left alone rather than half-migrated ────────────
    fs::write(paths::flows_dir().join("garbage.json"), "{ not json").unwrap();
    let report = scheduled_flows::migrate_from_flows();
    assert_eq!(report.failed, 1);
    assert_eq!(report.created, 0);
    assert_eq!(
        fs::read_to_string(paths::flows_dir().join("garbage.json")).unwrap(),
        "{ not json",
        "an unreadable flow is not rewritten"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
