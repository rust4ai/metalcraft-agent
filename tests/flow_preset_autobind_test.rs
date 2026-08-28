//! Installing a flow binds it to an agent that can actually run it.
//!
//! The containment rule (a flow may only name personas from its preset's roster) is
//! what makes the arm dialog's consent summary constructible. Its cost is that a flow
//! naming a specialist is unarmable until somebody works out which preset covers it —
//! and the one scheduled template the pod ships, `morning-brief`, is exactly that
//! case: it names `morning-briefer`, which the deliberately-small default agent
//! cannot reach.
//!
//! Widening the default roster is the wrong fix. `morning-briefer` uses
//! `metalcraft-calendar`, and a persona may only use packs its preset declares, so it
//! would drag a whole integration into the minimal agent. Choosing the right preset
//! at install is the fix.

use std::fs;

fn write(path: &std::path::Path, s: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, s).unwrap();
}

#[test]
fn a_flow_is_bound_to_a_preset_that_can_reach_its_personas() {
    let data_dir = std::env::temp_dir().join(format!("mc-autobind-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe { std::env::set_var("METALCRAFT_DATA_DIR", &data_dir) };
    fs::create_dir_all(&data_dir).unwrap();

    let presets = metalcraft_agent::paths::agent_presets_dir();
    // The minimal default: no specialists.
    write(
        &presets.join("general-agent.json"),
        r#"{"slug":"general-agent","name":"General Agent","default_persona":"orchestrator-agent",
            "personas":[{"slug":"orchestrator-agent","role":"default"}]}"#,
    );
    // A bigger one that can reach the briefer.
    write(
        &presets.join("briefing-agent.json"),
        r#"{"slug":"briefing-agent","name":"Briefing Agent","default_persona":"orchestrator-agent",
            "personas":[{"slug":"orchestrator-agent","role":"default"},
                        {"slug":"morning-briefer","role":"subagent"}]}"#,
    );

    let briefing = metalcraft_flows::SavedFlow {
        spec_version: "2".into(),
        id: "morning-brief".into(),
        name: "Morning brief".into(),
        created_at: "2026-08-19T00:00:00Z".into(),
        updated_at: "2026-08-19T00:00:00Z".into(),
        requires: None,
        flow: serde_json::from_str(
            r#"{"nodes":[{"id":"entry","node_type":"entry","data":{}},
                         {"id":"n1","node_type":"prompt",
                          "data":{"persona":"morning-briefer","prompt":"Brief me"}}],
                "edges":[{"id":"e1","source":"entry","target":"n1"}]}"#,
        )
        .unwrap(),
    };

    let chosen = metalcraft_agent::flow_bindings::bind_to_a_capable_preset(&briefing);
    assert_eq!(
        chosen.as_deref(),
        Some("briefing-agent"),
        "the default cannot reach 'morning-briefer', so the installer must pick one that can"
    );
    assert_eq!(
        metalcraft_agent::flow_bindings::preset_for("morning-brief"),
        "briefing-agent"
    );

    // And it can now be scheduled, which is the whole point.
    metalcraft_flows::save_flow(&metalcraft_agent::paths::flows_dir(), &briefing)
        .expect("save the flow a schedule will point at");
    let scheduled =
        metalcraft_agent::scheduled_flows::arm(metalcraft_agent::scheduled_flows::NewSchedule {
            flow: &briefing,
            schedule: metalcraft_flows::ScheduleSpec {
                trigger: metalcraft_flows::ScheduleTrigger::Cron {
                    cron: "0 0 8 * * *".into(),
                },
                name: Some("Morning".into()),
                timezone: None,
                inputs: None,
                persona: None,
            },
            enabled: true,
            instance: None,
            from_suggestion: None,
            id: None,
        })
        .expect("a bound flow arms");
    let agent =
        metalcraft_agent::agent_instance::load(scheduled.instance_id.as_deref().unwrap()).unwrap();
    assert!(
        matches!(&agent.origin, metalcraft_agent::agent_instance::InstanceOrigin::Flow { flow_id }
                 if flow_id == "morning-brief"),
        "the agent arming minted belongs to the flow: {:?}",
        agent.origin
    );

    // A flow the default *can* run stays with the default — an unremarkable flow
    // should belong to the unremarkable agent, not to whichever preset sorts first.
    let plain = metalcraft_flows::SavedFlow {
        id: "plain".into(),
        flow: serde_json::from_str(
            r#"{"nodes":[{"id":"entry","node_type":"entry","data":{}},
                         {"id":"n1","node_type":"prompt",
                          "data":{"persona":"orchestrator-agent","prompt":"hi"}}],
                "edges":[{"id":"e1","source":"entry","target":"n1"}]}"#,
        )
        .unwrap(),
        ..briefing.clone()
    };
    assert_eq!(
        metalcraft_agent::flow_bindings::bind_to_a_capable_preset(&plain).as_deref(),
        Some("general-agent")
    );

    // Nothing can run a flow naming a persona no preset has — and saying so at
    // install beats a containment error at 3am when the cron fires.
    let stray = metalcraft_flows::SavedFlow {
        id: "stray".into(),
        flow: serde_json::from_str(
            r#"{"nodes":[{"id":"entry","node_type":"entry","data":{}},
                         {"id":"n1","node_type":"prompt",
                          "data":{"persona":"nobody","prompt":"hi"}}],
                "edges":[{"id":"e1","source":"entry","target":"n1"}]}"#,
        )
        .unwrap(),
        ..briefing
    };
    assert_eq!(
        metalcraft_agent::flow_bindings::bind_to_a_capable_preset(&stray),
        None
    );

    let _ = fs::remove_dir_all(&data_dir);
}
