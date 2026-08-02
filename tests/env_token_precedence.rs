//! Proves the *real* `key_store::lookup` path (paths → keys.json → env) makes an
//! injected `METALCRAFT_TOKEN` win over a stale stored one — the property the k3s
//! control plane relies on when it injects a freshly minted token into the pod.
//!
//! Its own test binary so the process-global env / data-dir mutation is isolated
//! and single-threaded (data_dir() caches via OnceCell on first use).
use std::fs;

#[test]
fn injected_env_token_beats_stale_keys_json() {
    let dir = std::env::temp_dir().join(format!("mc-agent-envtok-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("keys.json"), r#"{"METALCRAFT_TOKEN":"mck_stale_stored"}"#).unwrap();

    // SAFETY: single-threaded test binary; set the data dir before the first
    // paths::data_dir() call (cached via OnceCell) so lookup reads our keys.json.
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &dir);
        std::env::set_var("METALCRAFT_TOKEN", "mck_injected_by_pod");
    }

    // Injected env token wins over the stale stored value.
    assert_eq!(
        metalcraft_agent::key_store::lookup("METALCRAFT_TOKEN").as_deref(),
        Some("mck_injected_by_pod"),
        "the pod-injected env token must beat a stale keys.json entry",
    );

    // A normal (non-authoritative) key stays store-first: the stored value wins
    // even though an env var of the same name exists.
    unsafe { std::env::set_var("SOME_OTHER_KEY", "from-env") }
    fs::write(
        dir.join("keys.json"),
        r#"{"METALCRAFT_TOKEN":"mck_stale_stored","SOME_OTHER_KEY":"from-store"}"#,
    )
    .unwrap();
    assert_eq!(
        metalcraft_agent::key_store::lookup("SOME_OTHER_KEY").as_deref(),
        Some("from-store"),
        "ordinary keys must remain store-first",
    );

    // With no env token, METALCRAFT_TOKEN falls back to the stored value (a
    // self-hosted user who pasted their own PAT).
    unsafe { std::env::remove_var("METALCRAFT_TOKEN") }
    assert_eq!(
        metalcraft_agent::key_store::lookup("METALCRAFT_TOKEN").as_deref(),
        Some("mck_stale_stored"),
    );

    fs::remove_dir_all(&dir).ok();
}
