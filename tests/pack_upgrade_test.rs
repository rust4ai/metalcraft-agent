//! Regression test for the bug where an updated bundled pack manifest never
//! reached an existing install: `ensure_defaults()` only wrote files that were
//! absent, so a `requires_env` change (e.g. a pack shrinking from three env
//! vars to one) kept showing the stale set forever — surviving even an app
//! reboot. Seeding now force-refreshes a pack when the bundled `version` is
//! higher than the installed one, but leaves equal/older installs untouched.
//!
//! Uses the bundled `metalcraft-notes` pack as the vehicle (any embedded pack
//! whose bundled `requires_env` differs from a synthetic stale one would do).
//!
//! Everything runs inside ONE `#[test]` so the process-global
//! `METALCRAFT_DATA_DIR` env var isn't raced by parallel tests.

use std::fs;

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

    let notes = data_dir
        .join("integration_packs")
        .join("metalcraft-notes")
        .join("pack.json");
    fs::create_dir_all(notes.parent().unwrap()).unwrap();

    // Simulate an old install: an early version of the notes pack declared three
    // env vars. This is exactly the stale manifest that kept showing "needs 3".
    let stale = r#"{
  "id": "metalcraft-notes",
  "name": "Metalcraft Notes",
  "description": "old",
  "version": "0.1.0",
  "requires_env": ["NOTES_URL", "NOTES_API_KEY", "NOTES_DB_ID"]
}"#;
    fs::write(&notes, stale).unwrap();
    assert_eq!(requires_env(&fs::read_to_string(&notes).unwrap()).len(), 3);

    // Seeding sees the bundled manifest is newer and force-refreshes the pack.
    metalcraft_agent::seed::ensure_defaults();

    let after = fs::read_to_string(&notes).unwrap();
    assert_eq!(
        requires_env(&after),
        vec!["METALCRAFT_TOKEN".to_string()],
        "a higher bundled version must overwrite the stale manifest down to its single env var, got: {after}"
    );

    // Now pretend the install is NEWER than the bundle (a hand-rolled future
    // version): seeding must not clobber it.
    let newer = r#"{
  "id": "metalcraft-notes",
  "name": "Metalcraft Notes",
  "description": "from the future",
  "version": "99.0.0",
  "requires_env": ["KEEP_ME"]
}"#;
    fs::write(&notes, newer).unwrap();
    metalcraft_agent::seed::ensure_defaults();
    assert_eq!(
        requires_env(&fs::read_to_string(&notes).unwrap()),
        vec!["KEEP_ME".to_string()],
        "an equal-or-newer install must be left untouched"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
