//! Every tool a seeded persona lists must be a tool this binary can register.
//!
//! This exists because it wasn't. `workshop-agent` listed `flow_install_dependencies`,
//! which no arm of `create_registry_for_with_config` matches — the real tool is
//! `flow_check_dependencies`. Unknown names fall through to the `unknown` arm, which
//! logs a warning and moves on, so the persona simply shipped without the capability
//! while its system prompt kept telling the model to call it. Nothing caught it:
//! `seed_consistency_test` checks personas against *skills*, not tools.
//!
//! Own test binary, one `#[test]`: `paths::data_dir()` caches `METALCRAFT_DATA_DIR`
//! in a `OnceLock`, and this test needs an EMPTY data dir — the `unknown` arm falls
//! back to `HttpApiTool::try_load`, which would otherwise resolve a name against
//! whatever packs happen to be installed on the machine running the test.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn seed_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed")
}

/// `slug -> tools` for every persona this binary ships: `seed/personas/` plus the
/// personas inside embedded agent packs.
fn seeded_persona_tools() -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let mut dirs = vec![seed_dir().join("personas")];
    if let Ok(entries) = std::fs::read_dir(seed_dir().join("agent_packs")) {
        dirs.extend(
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("personas")),
        );
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let slug = path.file_stem().unwrap().to_str().unwrap().to_string();
            let raw = std::fs::read_to_string(&path).expect("read persona");
            let v: serde_json::Value = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
            let tools = v["tools"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            out.insert(slug, tools);
        }
    }
    out
}

/// Tool names contributed declaratively by a shipped pack's integration — one JSON
/// file per tool under `integrations/<id>/api_tools/`. These resolve through
/// `HttpApiTool::try_load` on a pod that has the pack installed, so a persona naming
/// one is correct even though no match arm mentions it.
fn shipped_api_tool_names() -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(packs) = std::fs::read_dir(seed_dir().join("agent_packs")) else {
        return out;
    };
    for pack in packs.filter_map(|e| e.ok()) {
        let Ok(integrations) = std::fs::read_dir(pack.path().join("integrations")) else {
            continue;
        };
        for integration in integrations.filter_map(|e| e.ok()) {
            let Ok(tools) = std::fs::read_dir(integration.path().join("api_tools")) else {
                continue;
            };
            for t in tools.filter_map(|e| e.ok()) {
                let path = t.path();
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    out.insert(stem.to_string());
                }
            }
        }
    }
    out
}

/// A `ToolConfig` complete enough that every config-gated tool registers — without
/// one, `load_skill` / `sub_agent` / `say_to_user` and friends are skipped for a
/// reason that has nothing to do with the name being wrong. `instance_id` matters
/// most: the `mem_*` tools belong to an agent and are dropped without one.
fn full_config() -> metalcraft_agent::tools::ToolConfig {
    metalcraft_agent::tools::ToolConfig {
        api_key: "test-key".into(),
        model_name: "test-model".into(),
        system_prompt: String::new(),
        skills_dir: seed_dir().join("skills"),
        available_skills: Vec::new(),
        reply_sink: None,
        session_binding: None,
        reschedule_depth: 0,
        preset_personas: None,
        instance_id: Some("test-instance".into()),
        interrupt: None,
        turn_plan: Some(metalcraft_agent::turn_plan::SharedTurnPlan::default()),
    }
}

#[test]
fn seeded_personas_only_reference_tools_that_exist() {
    let data_dir = std::env::temp_dir().join(format!("mc-persona-tools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create empty data dir");
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let config = full_config();
    let api_tools = shipped_api_tool_names();
    let personas = seeded_persona_tools();
    assert!(!personas.is_empty(), "no seeded personas found — seed moved?");

    let mut problems = Vec::new();
    for (slug, tools) in &personas {
        let registry = metalcraft_agent::tools::create_registry_for_with_config(tools, Some(&config));
        let registered: HashSet<&str> = registry.names().into_iter().collect();
        for t in tools {
            if !registered.contains(t.as_str()) && !api_tools.contains(t) {
                problems.push(format!(
                    "persona '{slug}' lists tool '{t}', which nothing registers"
                ));
            }
        }
    }

    let _ = std::fs::remove_dir_all(&data_dir);
    problems.sort();
    assert!(
        problems.is_empty(),
        "seeded personas naming tools that would be silently dropped:\n  - {}",
        problems.join("\n  - ")
    );
}
