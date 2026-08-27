//! Agent presets and instances against a real data dir.
//!
//! Everything runs inside ONE `#[test]` so the process-global `METALCRAFT_DATA_DIR`
//! isn't raced by parallel tests — the same discipline as `pack_resolution_test`.

use metalcraft_agent::agent_instance::{self, AgentInstance, InstanceOrigin};
use metalcraft_agent::agent_preset::AgentPreset;
use std::fs;
use std::path::PathBuf;

fn write(path: &PathBuf, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

const AMY: &str = r#"{
  "slug": "amy-kitchen",
  "name": "Amy's Kitchen Agent",
  "description": "A chef agent",
  "default_persona": "amy",
  "version": "1.4.0",
  "personas": [
    { "slug": "amy", "role": "default" },
    { "slug": "amy-shopper", "role": "subagent" },
    { "slug": "amy-critic", "role": "internal" }
  ]
}"#;

const GENERAL: &str = r#"{
  "slug": "general-agent",
  "name": "General Agent",
  "description": "The default",
  "default_persona": "orchestrator-agent",
  "personas": [{ "slug": "orchestrator-agent", "role": "default" }]
}"#;

const PACK_PRESET: &str = r#"{
  "slug": "shared",
  "name": "Shared",
  "description": "provided by a pack",
  "default_persona": "orchestrator-agent"
}"#;

#[test]
fn presets_resolve_and_instances_group_conversations() {
    let data_dir = std::env::temp_dir().join(format!("mc-preset-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let presets_dir = data_dir.join("agent_presets");
    write(&presets_dir.join("amy-kitchen.json"), AMY);
    write(&presets_dir.join("general-agent.json"), GENERAL);

    // ── presets ──────────────────────────────────────────────────────────────
    let amy = AgentPreset::load("amy-kitchen", &presets_dir).expect("load amy");
    assert_eq!(amy.default_persona, "amy");
    assert_eq!(amy.callable_personas(), vec!["amy", "amy-shopper"]);
    assert!(
        amy.allows_persona("amy-critic"),
        "internal is reachable, not offered"
    );
    assert!(
        !amy.allows_persona("orchestrator-agent"),
        "outside the roster"
    );

    let slugs = AgentPreset::list_available(&presets_dir);
    assert!(slugs.contains(&"amy-kitchen".to_string()));
    assert!(slugs.contains(&"general-agent".to_string()));

    let summaries = AgentPreset::list_summaries(&presets_dir);
    let amy_summary = summaries.iter().find(|s| s.slug == "amy-kitchen").unwrap();
    assert_eq!(amy_summary.persona_count, 3);
    assert!(!amy_summary.read_only, "user-local presets are editable");

    assert!(
        AgentPreset::load("nope", &presets_dir).is_err(),
        "a missing preset is an error, not a silent default"
    );

    // ── ambiguity: two installed agent packs providing the same slug ─────────
    for pack in ["packa", "packb"] {
        let root = data_dir.join("agent_packs").join(pack);
        write(
            &root.join("agent_pack.json"),
            &format!(
                r#"{{"manifest_version":2,"id":"{pack}","name":"{pack}","description":"t",
                     "version":"1.0.0","presets":["shared"]}}"#
            ),
        );
        write(&root.join("agent_presets").join("shared.json"), PACK_PRESET);
    }

    let err = AgentPreset::load("shared", &presets_dir)
        .expect_err("two packs providing one slug must not silently resolve");
    assert!(err.contains("ambiguous"), "{err}");
    assert!(
        err.contains("packa/shared") && err.contains("packb/shared"),
        "names both: {err}"
    );

    // A user-local copy wins outright — it is the operator's own file.
    write(&presets_dir.join("shared.json"), PACK_PRESET);
    assert!(
        AgentPreset::load("shared", &presets_dir).is_ok(),
        "a local file must resolve the ambiguity, not inherit it"
    );

    // ── instances ────────────────────────────────────────────────────────────
    let mut chat = AgentInstance::new(&amy, InstanceOrigin::Workshop);
    chat.name = "Sunday prep".into();
    chat.save().expect("save instance");

    let reloaded = agent_instance::load(&chat.id).expect("load instance");
    assert_eq!(reloaded.name, "Sunday prep");
    assert_eq!(reloaded.agent_preset, "amy-kitchen");
    assert_eq!(reloaded.persona, "amy");
    assert_eq!(reloaded.created_from_version.as_deref(), Some("1.4.0"));

    // A sender binds to one agent and keeps it across calls — that's the continuity
    // starting a new session after a quiet gap would otherwise destroy.
    let first = agent_instance::for_gateway_sender("sms-amy", "gw-sms-amy-1", "Amy", "amy-kitchen")
        .expect("bind sender");
    let second =
        agent_instance::for_gateway_sender("sms-amy", "gw-sms-amy-1", "Amy", "amy-kitchen")
            .expect("rebind sender");
    assert_eq!(
        first.id, second.id,
        "a sender must not mint a new agent per conversation"
    );
    // No lifetime flag to check any more: every agent is kept, so the thing worth
    // pinning is that this one is *the same* agent, which is the continuity.

    assert_eq!(
        first.origin,
        InstanceOrigin::Gateway {
            channel: "sms-amy".into(),
            sender: Some("gw-sms-amy-1".into()),
        }
    );

    // A second person on the same channel is a different agent, with a memory of
    // their own — the whole reason this is keyed by sender.
    let other = agent_instance::for_gateway_sender("sms-amy", "gw-sms-amy-2", "Bo", "amy-kitchen")
        .expect("bind second sender");
    assert_ne!(
        first.id, other.id,
        "two people on one channel must not share an agent"
    );

    assert!(agent_instance::list().len() >= 2);

    // ── backfill: legacy chats get an agent ──────────────────────────────────
    let chats_dir = data_dir.join("chats");
    write(
        &chats_dir.join("legacy-1.json"),
        r#"{"id":"legacy-1","persona_slug":"orchestrator-agent","model_name":"gpt-5.4",
            "cwd":".","created_at":"2026-01-01T00:00:00Z","messages":[]}"#,
    );
    let report = agent_instance::backfill_from_chats(&chats_dir).expect("backfill");
    assert_eq!(report.migrated, 1);
    assert_eq!(report.skipped, 0);

    let doc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(chats_dir.join("legacy-1.json")).unwrap())
            .unwrap();
    let bound = doc["instance_id"]
        .as_str()
        .expect("chat must now name its agent");
    let inst = agent_instance::load(bound).expect("backfilled instance exists");
    assert_eq!(inst.persona, "orchestrator-agent");
    assert_eq!(
        inst.agent_preset, "general-agent",
        "binds to the preset whose default persona it used"
    );
    assert_eq!(
        inst.created_at, "2026-01-01T00:00:00Z",
        "keeps the chat's own age"
    );

    // Idempotent: a second run must not mint a duplicate agent.
    let again = agent_instance::backfill_from_chats(&chats_dir).expect("backfill again");
    assert_eq!(again.migrated, 0);
    assert_eq!(again.already_bound, 1);

    // ── delete keeps transcripts ─────────────────────────────────────────────
    agent_instance::delete(&chat.id).expect("delete");
    assert!(agent_instance::load(&chat.id).is_err());
    assert!(
        chats_dir.join("legacy-1.json").exists(),
        "deleting an agent must not delete chats"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
