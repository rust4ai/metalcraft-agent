//! Which agent a flow runs as, end to end.
//!
//! Own test binary: `paths::data_dir()` caches `METALCRAFT_DATA_DIR` in a `OnceLock`,
//! so two tests in one process would share a data dir whatever they set.

use metalcraft_agent::{agent_instance, agent_preset, flow_bindings, paths, scheduled_flows};
use metalcraft_flows::{ScheduleSpec, ScheduleTrigger};
use std::fs;

/// A cron trigger with a label, which is all these tests need to vary.
fn at(name: &str, cron: &str) -> ScheduleSpec {
    ScheduleSpec {
        trigger: ScheduleTrigger::Cron {
            cron: cron.to_string(),
        },
        name: Some(name.to_string()),
        timezone: None,
        inputs: None,
        persona: None,
    }
}

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
    let flow: metalcraft_flows::SavedFlow = serde_json::from_value(serde_json::json!({
        "spec_version": "3",
        "id": id,
        "name": format!("flow {id}"),
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "flow": { "nodes": nodes, "edges": [] },
    }))
    .expect("fixture flow parses");
    // Saved, because a schedule points at a flow by id and the lookups below read
    // the flows dir rather than taking the value.
    metalcraft_flows::save_flow(&paths::flows_dir(), &flow).expect("save fixture flow");
    flow
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
    assert_eq!(
        flow_bindings::preset_for("brief"),
        agent_preset::DEFAULT_PRESET
    );
    assert!(!flow_bindings::get("brief").preset.is_some());

    // ── containment is checked at bind time, not at 3am ───────────────────────
    let err =
        flow_bindings::bind_preset(&brief, "mitch").expect_err("mitch cannot reach amy's personas");
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
    assert!(
        scheduled_flows::for_flow("brief").is_empty(),
        "nothing is scheduled until somebody schedules it"
    );
    let morning_sf = scheduled_flows::arm(scheduled_flows::NewSchedule {
        flow: &brief,
        schedule: at("Morning", "0 0 8 * * *"),
        enabled: true,
        instance: None,
        from_suggestion: None,
        id: None,
    })
    .expect("arm morning");
    let morning = agent_instance::load(morning_sf.instance_id.as_deref().unwrap())
        .expect("the instance was persisted, not just referenced");
    assert!(
        matches!(&morning.origin, agent_instance::InstanceOrigin::Flow { flow_id } if flow_id == "brief"),
        "the agent belongs to the flow that minted it: {:?}",
        morning.origin
    );
    assert_eq!(morning.agent_preset, "amy");
    assert_eq!(
        morning.persona, "amy-chef",
        "starts at the preset's default"
    );
    assert!(
        matches!(&morning.origin, agent_instance::InstanceOrigin::Flow { flow_id } if flow_id == "brief"),
        "{:?}",
        morning.origin
    );
    assert!(
        morning.name.contains("Morning"),
        "named after the schedule: {}",
        morning.name
    );

    // ── a second schedule on the same flow shares the agent ───────────────────
    // The 18:00 run should remember the 08:00 one; two agents would not.
    let evening_sf = scheduled_flows::arm(scheduled_flows::NewSchedule {
        flow: &brief,
        schedule: at("Evening", "0 0 18 * * *"),
        enabled: true,
        instance: None,
        from_suggestion: None,
        id: None,
    })
    .expect("arm evening");
    assert_eq!(
        evening_sf.instance_id, morning_sf.instance_id,
        "schedules of one flow share an agent by default"
    );
    assert_ne!(evening_sf.id, morning_sf.id, "but are separate documents");

    // ── the containment rule reaches a schedule's own persona override ────────
    let mut outside = at("Outside", "0 0 9 * * *");
    outside.persona = Some("mitch-critic".into());
    let err = scheduled_flows::arm(scheduled_flows::NewSchedule {
        flow: &brief,
        schedule: outside,
        enabled: true,
        instance: None,
        from_suggestion: None,
        id: None,
    })
    .expect_err("mitch-critic is not in amy's roster");
    assert!(err.contains("mitch-critic"), "{err}");

    // ── what is this agent scheduled to do? ───────────────────────────────────
    let scheduled = scheduled_flows::for_instance(&morning.id);
    assert_eq!(scheduled.len(), 2);
    assert!(scheduled.iter().all(|sf| sf.flow_id == "brief"));

    // ── attaching to an existing agent instead of minting one ─────────────────
    let other = flow("other", &["amy-chef"]);
    flow_bindings::bind_preset(&other, "amy").unwrap();
    let attached = scheduled_flows::arm(scheduled_flows::NewSchedule {
        flow: &other,
        schedule: at("Morning", "0 0 8 * * *"),
        enabled: true,
        instance: Some(&morning.id),
        from_suggestion: None,
        id: None,
    })
    .expect("attach");
    assert_eq!(attached.instance_id.as_deref(), Some(morning.id.as_str()));
    assert_eq!(
        scheduled_flows::for_instance(&morning.id).len(),
        3,
        "one agent now runs schedules of both flows"
    );

    // ── disarming keeps the agent and everything it remembers ─────────────────
    scheduled_flows::disarm(&evening_sf.id).unwrap();
    assert!(scheduled_flows::get(&evening_sf.id).is_none());
    assert!(scheduled_flows::get(&morning_sf.id).is_some());
    agent_instance::load(&morning.id).expect("disarm must not delete the agent");

    // ── rebinding to the default keeps armed schedules ────────────────────────
    flow_bindings::unbind("brief").unwrap();
    assert_eq!(
        flow_bindings::preset_for("brief"),
        agent_preset::DEFAULT_PRESET
    );
    assert_eq!(
        scheduled_flows::get(&morning_sf.id)
            .and_then(|sf| sf.instance_id)
            .as_deref(),
        Some(morning.id.as_str()),
        "unbinding the preset must not orphan the running agent"
    );

    // ── deleting a flow's schedules leaves the other flow's alone ─────────────
    assert_eq!(scheduled_flows::forget_flow("brief"), 1);
    assert!(scheduled_flows::get(&morning_sf.id).is_none());
    assert_eq!(
        scheduled_flows::for_instance(&morning.id).len(),
        1,
        "only the 'other' flow's schedule is left"
    );

    // ── bindings and schedules survive a restart ──────────────────────────────
    assert_eq!(flow_bindings::preset_for("other"), "amy");
    assert!(paths::data_dir().join("flow_bindings.json").is_file());
    assert!(
        paths::scheduled_flows_dir()
            .join(format!("{}.json", attached.id))
            .is_file(),
        "one document per schedule, named by its id"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
