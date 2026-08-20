//! Upgrading a pre-0.30 pod must not cost it the tools it already had.
//!
//! "Integration pack" became "integration" in 0.30 and four paths moved with it.
//! The dangerous one is the quietest: an installed agent pack records which store
//! entries it uses in `<data>/agent_packs/<id>/integration_packs.json`, and
//! `store::read_refs` degrades a missing file to *no refs*. Read past it and every
//! installed agent pack resolves zero integrations — the agent loses every HTTP
//! tool, silently, with every file still on disk under its old name.

use std::fs;

#[test]
fn a_pre_030_data_dir_keeps_working_after_the_rename() {
    let data_dir = std::env::temp_dir().join(format!("mc-legacy-paths-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe { std::env::set_var("METALCRAFT_DATA_DIR", &data_dir) };
    fs::create_dir_all(&data_dir).unwrap();

    // ── lay out a pod exactly as 0.29 left it ───────────────────────────────
    let sha = "a".repeat(64);
    let store_entry = data_dir.join("pack_store").join(&sha);
    fs::create_dir_all(store_entry.join("api_tools")).unwrap();
    fs::write(
        store_entry.join("pack.json"),
        br#"{"id":"acme","name":"Acme","description":"d","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        store_entry.join("api_tools/acme_ping.json"),
        br#"{"name":"acme_ping","description":"p","method":"GET","url":"https://acme.example/ping","parameters":{"type":"object","properties":{}}}"#,
    )
    .unwrap();

    let pack_dir = data_dir.join("agent_packs").join("acme-agent");
    fs::create_dir_all(&pack_dir).unwrap();
    fs::write(
        pack_dir.join("agent_pack.json"),
        format!(
            r#"{{"manifest_version":1,"id":"acme-agent","name":"Acme","version":"1.0.0","presets":["acme"]}}"#
        ),
    )
    .unwrap();
    fs::write(
        pack_dir.join("integration_packs.json"),
        format!(r#"{{"acme":"{sha}"}}"#),
    )
    .unwrap();

    // A side-loaded integration, and the enable-state file.
    fs::create_dir_all(data_dir.join("integration_packs").join("legacy-thing")).unwrap();
    fs::write(
        data_dir.join("integration_packs").join("legacy-thing").join("pack.json"),
        br#"{"id":"legacy-thing","name":"Legacy","description":"d","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(data_dir.join("integration_packs.json"), b"{}").unwrap();

    // ── upgrade ─────────────────────────────────────────────────────────────
    metalcraft_agent::paths::migrate_legacy_integration_paths();

    // The refs file moved, so the agent pack still knows what it vendored…
    let refs = metalcraft_agent::agent_packs::store::read_refs("acme-agent");
    assert_eq!(
        refs.get("acme").map(String::as_str),
        Some(sha.as_str()),
        "an upgraded pod must keep its agent packs' integration refs"
    );

    // …and the store entry those refs point at is where the new code looks, so
    // the tools actually resolve. This is the assertion that fails without the
    // migration, and it fails as "the agent has no tools", not as an error.
    assert!(
        metalcraft_agent::agent_packs::store::resolve("acme").is_some(),
        "the vendored integration must still resolve after the rename"
    );

    // The side-loaded integration survived too — under both new names, directory
    // and manifest.
    assert!(data_dir.join("integrations/legacy-thing/integration.json").is_file());
    assert!(!data_dir.join("integrations/legacy-thing/pack.json").exists());
    // …and so did the manifest inside the stored entry, which is what
    // `store::resolve` actually looks for.
    assert!(data_dir.join("integration_store").join(&sha).join("integration.json").is_file());
    assert!(data_dir.join("integrations.json").is_file());
    assert!(!data_dir.join("pack_store").exists(), "the old paths are gone, not copied");

    // ── idempotent ──────────────────────────────────────────────────────────
    metalcraft_agent::paths::migrate_legacy_integration_paths();
    assert!(metalcraft_agent::agent_packs::store::resolve("acme").is_some());

    // ── and it never clobbers ───────────────────────────────────────────────
    // If both names somehow exist, the live one wins and nothing is overwritten.
    fs::create_dir_all(data_dir.join("pack_store")).unwrap();
    fs::write(data_dir.join("pack_store/marker"), b"old").unwrap();
    metalcraft_agent::paths::migrate_legacy_integration_paths();
    assert!(
        data_dir.join("pack_store/marker").is_file(),
        "a rename must never overwrite a live directory to tidy up a name"
    );
    assert!(metalcraft_agent::agent_packs::store::resolve("acme").is_some());

    let _ = fs::remove_dir_all(&data_dir);
}
