//! Tests for `integrations::recommended_env` — the "keys these packs still need"
//! signal that drives the key store UI hint. Verifies it aggregates `requires_env`
//! across every installed pack with the right attribution, and that a stored key
//! resolves via `key_store::lookup` (the `configured` flag's source).
//!
//! Single `#[test]` so the process-global `METALCRAFT_DATA_DIR` isn't raced.

use std::fs;

#[test]
fn recommends_env_from_installed_packs_with_attribution() {
    let data_dir = std::env::temp_dir().join(format!("mc-reckeys-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    // Pack A requires two keys; pack B requires one that overlaps with A. Both go in
    // through the normal installer — the only way a pack reaches a pod — so what is
    // being read back is a real install rather than a hand-placed directory.
    for (id, name, env) in [
        ("packa", "Pack A", r#"["SHARED_KEY","A_ONLY_KEY"]"#),
        ("packb", "Pack B", r#"["SHARED_KEY"]"#),
    ] {
        let manifest: metalcraft_agent::agent_packs::AgentPackManifest =
            serde_json::from_str(&format!(
                r#"{{"manifest_version":2,"id":"{id}","name":"{name}","description":"x",
                     "version":"1.0.0","presets":["{id}"]}}"#
            ))
            .unwrap();
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            format!("agent_presets/{id}.json"),
            format!(
                r#"{{"manifest_version":2,"slug":"{id}","name":"{name}","default_persona":"{id}-agent",
                     "personas":[{{"slug":"{id}-agent","role":"default"}}],"integrations":["{id}"]}}"#
            )
            .into_bytes(),
        );
        files.insert(
            format!("personas/{id}-agent.json"),
            format!(
                r#"{{"name":"{name} Agent","description":"x","system_prompt":"you are {id}",
                     "tools":[],"packs":["{id}"]}}"#
            )
            .into_bytes(),
        );
        files.insert(
            format!("integrations/{id}/integration.json"),
            format!(
                r#"{{"id":"{id}","name":"{name}","description":"x","version":"1.0.0",
                     "requires_env":{env}}}"#
            )
            .into_bytes(),
        );
        let bytes = metalcraft_agent::agent_packs::bundle::write(manifest, files).unwrap();
        metalcraft_agent::agent_packs::install(&bytes, "test").unwrap();
    }

    // Both packs are installed, so both contribute — enable/disable is retired and
    // an installed pack is available. What still matters, and is what this test is
    // really for, is the *attribution*: which pack wants each key.
    let recs = metalcraft_agent::integrations::recommended_env();
    let names: Vec<&str> = recs.iter().map(|(n, _)| n.as_str()).collect();
    // Sorted by key name.
    assert_eq!(names, vec!["A_ONLY_KEY", "SHARED_KEY"]);
    assert_eq!(
        recs[0].1,
        vec!["packa"],
        "a key only one pack wants names only that pack"
    );
    let shared = recs.iter().find(|(n, _)| n == "SHARED_KEY").unwrap();
    assert_eq!(
        shared.1,
        vec!["packa", "packb"],
        "a shared key names both, sorted"
    );

    // `configured` source: a stored key resolves, a missing one does not.
    fs::write(
        data_dir.join("keys.json"),
        r#"{"SHARED_KEY":"some-secret-value"}"#,
    )
    .unwrap();
    assert!(metalcraft_agent::key_store::lookup("SHARED_KEY").is_some());
    assert!(metalcraft_agent::key_store::lookup("A_ONLY_KEY").is_none());
}
