//! Which agent a flow runs as, end to end.
//!
//! Own test binary: `paths::data_dir()` caches `METALCRAFT_DATA_DIR` in a `OnceLock`,
//! so two tests in one process would share a data dir whatever they set.

use metalcraft_agent::{agent_instance, agent_preset, flow_bindings, paths};
use std::fs;

fn write_preset(slug: &str, default_persona: &str, roster: &[(&str, &str)]) {
    let dir = paths::agent_presets_dir();
    fs::create_dir_all(&dir).unwrap();
    let personas: Vec<serde_json::Value> = roster
        .iter()
        .map(|(s, role)| serde_json::json!({ "slug": s, "role": role }))
        .collect();
    fs::write(
        dir.join(format!("{slug}.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "slug": slug,
            "name": format!("{slug} agent"),
            "description": "test preset",
            "default_persona": default_persona,
            "personas": personas,
            "version": "1.0.0",
        }))
        .unwrap(),
    )
    .unwrap();
}

fn flow(id: &str, personas: &[&str]) -> metalcraft_flows::SavedFlow {
    let mut nodes = vec![serde_json::json!({
        "id": "entry", "node_type": "entry", "position": [0, 0],
        "data": { "persona": personas[0] },
    })];
    for (i, p) in personas.iter().enumerate().skip(1) {
        nodes.push(serde_json::json!({
            "id": format!("n{i}"), "node_type": "prompt", "position": [i, 0],
            "data": { "persona": p, "prompt": "do a thing" },
        }));
    }
    serde_json::from_value(serde_json::json!({
        "spec_version": "2",
        "id": id,
        "name": format!("flow {id}"),
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "enabled": false,
        "flow": { "nodes": nodes, "edges": [] },
        "schedules": [
            { "id": "morning", "name": "Morning", "type": "cron", "cron": "0 8 * * *", "enabled": true },
            { "id": "evening", "name": "Evening", "type": "cron", "cron": "0 18 * * *", "enabled": true },
        ],
    }))
    .expect("fixture flow parses")
}

#[test]
fn binding_arming_and_disarming_a_flow() {
    let data_dir = std::env::temp_dir().join(format!("mc-flowbind-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    write_preset(
        "amy",
        "amy-chef",
        &[("amy-chef", "default"), ("amy-shopper", "subagent")],
    );
    write_preset("mitch", "mitch-critic", &[("mitch-critic", "default")]);

    let brief = flow("brief", &["amy-chef", "amy-shopper"]);

    // ── an unbound flow is the default agent, which is what it always was ─────
    assert_eq!(flow_bindings::preset_for("brief"), agent_preset::DEFAULT_PRESET);
    assert!(!flow_bindings::get("brief").preset.is_some());

    // ── containment is checked at bind time, not at 3am ───────────────────────
    let err = flow_bindings::bind_preset(&brief, "mitch")
        .expect_err("mitch cannot reach amy's personas");
    assert!(err.contains("amy-chef"), "{err}");
    assert!(err.contains("roster"), "{err}");
    assert_eq!(
        flow_bindings::preset_for("brief"),
        agent_preset::DEFAULT_PRESET,
        "a rejected bind must not half-apply"
    );

    flow_bindings::bind_preset(&brief, "amy").expect("amy's roster covers this flow");
    assert_eq!(flow_bindings::preset_for("brief"), "amy");

    // ── arming is what creates the agent ──────────────────────────────────────
    assert!(flow_bindings::instance_for("brief", "morning").is_none());
    let morning = flow_bindings::arm(&brief, "morning", None).expect("arm morning");
    assert!(morning.persistent, "a scheduled agent must outlive a TTL reap");
    assert_eq!(morning.agent_preset, "amy");
    assert_eq!(morning.persona, "amy-chef", "starts at the preset's default");
    assert!(
        matches!(&morning.origin, agent_instance::InstanceOrigin::Flow { flow_id } if flow_id == "brief"),
        "{:?}",
        morning.origin
    );
    assert!(morning.name.contains("Morning"), "named after the schedule: {}", morning.name);
    agent_instance::load(&morning.id).expect("the instance was persisted, not just returned");

    // ── a second schedule on the same flow shares the agent ───────────────────
    // The 18:00 run should remember the 08:00 one; two agents would not.
    let evening = flow_bindings::arm(&brief, "evening", None).expect("arm evening");
    assert_eq!(evening.id, morning.id, "schedules of one flow share an agent by default");

    // ── arming an unknown schedule is an error, not a silent no-op ────────────
    let err = flow_bindings::arm(&brief, "midnight", None).expect_err("no such schedule");
    assert!(err.contains("midnight"), "{err}");

    // ── what is this agent scheduled to do? ───────────────────────────────────
    let scheduled = flow_bindings::flows_for_instance(&morning.id);
    assert_eq!(scheduled.len(), 1);
    assert_eq!(scheduled[0].0, "brief");
    let mut sids = scheduled[0].1.clone();
    sids.sort();
    assert_eq!(sids, vec!["evening", "morning"]);

    // ── attaching to an existing agent instead of minting one ─────────────────
    let other = flow("other", &["amy-chef"]);
    flow_bindings::bind_preset(&other, "amy").unwrap();
    let attached = flow_bindings::arm(&other, "morning", Some(&morning.id)).expect("attach");
    assert_eq!(attached.id, morning.id);
    assert_eq!(
        flow_bindings::flows_for_instance(&morning.id).len(),
        2,
        "one agent now runs both flows"
    );

    // ── disarming keeps the agent and everything it remembers ─────────────────
    flow_bindings::disarm("brief", "evening").unwrap();
    assert!(flow_bindings::instance_for("brief", "evening").is_none());
    assert!(flow_bindings::instance_for("brief", "morning").is_some());
    agent_instance::load(&morning.id).expect("disarm must not delete the agent");

    // ── rebinding to the default keeps armed schedules ────────────────────────
    flow_bindings::unbind("brief").unwrap();
    assert_eq!(flow_bindings::preset_for("brief"), agent_preset::DEFAULT_PRESET);
    assert_eq!(
        flow_bindings::instance_for("brief", "morning"),
        Some(morning.id.clone()),
        "unbinding the preset must not orphan the running agent"
    );

    // ── forgetting a flow drops its bindings entirely ─────────────────────────
    flow_bindings::forget("brief").unwrap();
    assert!(flow_bindings::instance_for("brief", "morning").is_none());
    assert_eq!(
        flow_bindings::flows_for_instance(&morning.id).len(),
        1,
        "only the 'other' flow is left"
    );

    // ── bindings survive a restart ────────────────────────────────────────────
    assert_eq!(flow_bindings::preset_for("other"), "amy");
    assert!(paths::data_dir().join("flow_bindings.json").is_file());

    let _ = fs::remove_dir_all(&data_dir);
}
