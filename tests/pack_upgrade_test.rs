//! Regression test for the bug where an updated bundled pack manifest never
//! reached an existing install: `ensure_defaults()` only wrote files that were
//! absent, so a `requires_env` change (e.g. the Solarabase pack shrinking from
//! three env vars to one) kept showing the stale set forever — surviving even an
//! app reboot. Seeding now force-refreshes a pack when the bundled `version` is
//! higher than the installed one, but leaves equal/older installs untouched.
//!
//! Everything runs inside ONE `#[test]` so the process-global
//! `METALCRAFT_DATA_DIR` env var isn't raced by parallel tests.

use std::fs;
use std::path::PathBuf;

fn requires_env(pack_json: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(pack_json).unwrap();
    v["requires_env"]
        .as_array()
        .map(|a| a.iter().map(|k| k.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

#[test]
fn higher_bundled_version_reseeds_pack_but_equal_or_older_is_left_alone() {
    // Isolated data dir for this process.
    let data_dir = std::env::temp_dir().join(format!("mc-upgrade-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let solarabase = data_dir
        .join("integration_packs")
        .join("solarabase")
        .join("pack.json");
    fs::create_dir_all(solarabase.parent().unwrap()).unwrap();

    // Simulate an old install: v1.0.0 of the Solarabase pack declared three env
    // vars. This is exactly the stale manifest that kept showing "needs 3".
    let stale = r#"{
  "id": "solarabase",
  "name": "Solarabase RAG",
  "description": "old",
  "version": "1.0.0",
  "requires_env": ["SOLARABASE_URL", "SOLARABASE_API_KEY", "SOLARABASE_KB_ID"]
}"#;
    fs::write(&solarabase, stale).unwrap();
    assert_eq!(requires_env(&fs::read_to_string(&solarabase).unwrap()).len(), 3);

    // Seeding sees the bundled manifest is newer and force-refreshes the pack.
    metalcraft_agent::seed::ensure_defaults();

    let after = fs::read_to_string(&solarabase).unwrap();
    assert_eq!(
        requires_env(&after),
        vec!["SOLARABASE_API_KEY".to_string()],
        "a higher bundled version must overwrite the stale manifest down to its single env var, got: {after}"
    );

    // Now pretend the install is NEWER than the bundle (a hand-rolled future
    // version): seeding must not clobber it.
    let newer = r#"{
  "id": "solarabase",
  "name": "Solarabase RAG",
  "description": "from the future",
  "version": "99.0.0",
  "requires_env": ["KEEP_ME"]
}"#;
    fs::write(&solarabase, newer).unwrap();
    metalcraft_agent::seed::ensure_defaults();
    assert_eq!(
        requires_env(&fs::read_to_string(&solarabase).unwrap()),
        vec!["KEEP_ME".to_string()],
        "an equal-or-newer install must be left untouched"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
