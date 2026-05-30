//! Tests for `integration_packs::recommended_env` — the "keys these enabled
//! packs still need" signal that drives the key store UI hint. Verifies it
//! aggregates `requires_env` across *enabled* packs only, and that a stored
//! key resolves via `key_store::lookup` (the `configured` flag's source).
//!
//! Single `#[test]` so the process-global `METALCRAFT_DATA_DIR` isn't raced.

use std::fs;
use std::path::PathBuf;

fn write(path: &PathBuf, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

#[test]
fn recommends_env_for_enabled_packs_only() {
    let data_dir = std::env::temp_dir().join(format!("mc-reckeys-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    // Pack A requires two keys; pack B requires one that overlaps with A.
    let pack_a = data_dir.join("integration_packs").join("packa");
    write(
        &pack_a.join("pack.json"),
        r#"{"id":"packa","name":"Pack A","description":"a","version":"1.0.0",
            "requires_env":["SHARED_KEY","A_ONLY_KEY"]}"#,
    );
    let pack_b = data_dir.join("integration_packs").join("packb");
    write(
        &pack_b.join("pack.json"),
        r#"{"id":"packb","name":"Pack B","description":"b","version":"1.0.0",
            "requires_env":["SHARED_KEY"]}"#,
    );

    let state_file = data_dir.join("integration_packs.json");

    // Nothing enabled → no recommendations.
    assert!(metalcraft_agent::integration_packs::recommended_env().is_empty());

    // Enable only pack A. Its two keys are recommended; pack B's are not.
    write(&state_file, r#"{"packa":{"enabled":true}}"#);
    let recs = metalcraft_agent::integration_packs::recommended_env();
    let names: Vec<&str> = recs.iter().map(|(n, _)| n.as_str()).collect();
    // Sorted by key name.
    assert_eq!(names, vec!["A_ONLY_KEY", "SHARED_KEY"]);
    assert_eq!(recs[0].1, vec!["packa"]);

    // Enable pack B too → SHARED_KEY now attributed to both packs (sorted ids).
    write(&state_file, r#"{"packa":{"enabled":true},"packb":{"enabled":true}}"#);
    let recs = metalcraft_agent::integration_packs::recommended_env();
    let shared = recs.iter().find(|(n, _)| n == "SHARED_KEY").unwrap();
    assert_eq!(shared.1, vec!["packa", "packb"]);

    // `configured` source: a stored key resolves, a missing one does not.
    write(
        &data_dir.join("keys.json"),
        r#"{"SHARED_KEY":"some-secret-value"}"#,
    );
    assert!(metalcraft_agent::key_store::lookup("SHARED_KEY").is_some());
    assert!(metalcraft_agent::key_store::lookup("A_ONLY_KEY").is_none());
}
