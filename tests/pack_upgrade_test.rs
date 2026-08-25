//! Seeding a first-party pack is an **install**, and it obeys install's rules.
//!
//! The bug this descends from: `ensure_defaults()` only wrote files that were
//! absent, so an updated bundled manifest never reached an existing install — a
//! pack that shrank from three env vars to one kept showing three forever. That was
//! fixed with a version gate, and the gate now lives where every other pack's does,
//! in `agent_packs::install`: a higher bundled version upgrades, an equal one is
//! skipped, and a *newer* install is never silently downgraded.
//!
//! Uses the bundled `metalcraft-email` pack as the vehicle (any seeded agent pack
//! would do).
//!
//! Everything runs inside ONE `#[test]` so the process-global
//! `METALCRAFT_DATA_DIR` env var isn't raced by parallel tests.

use std::fs;

const PACK_ID: &str = "metalcraft-email";

#[test]
fn seeding_installs_the_bundled_pack_and_never_downgrades_a_newer_one() {
    // Isolated data dir for this process.
    let data_dir = std::env::temp_dir().join(format!("mc-upgrade-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    // 1. A fresh pod installs the seeded agent pack, and the integration it vendors
    //    resolves out of the content store with the bundled manifest.
    metalcraft_agent::seed::ensure_defaults();

    let installed = metalcraft_agent::agent_packs::find(PACK_ID).expect("seeded pack installs");
    let bundled_version = installed.manifest.version.clone();

    let integration =
        metalcraft_agent::integrations::find_installed(PACK_ID).expect("vendored integration");
    assert_eq!(
        integration.manifest.requires_env,
        vec!["METALCRAFT_TOKEN".to_string()],
        "the installed manifest is the bundled one, not a stale copy"
    );

    // 2. Seeding again is a no-op: same version in, same version out. (The skip is
    //    what keeps `install`'s store garbage-collection off the boot path.)
    metalcraft_agent::seed::ensure_defaults();
    assert_eq!(
        metalcraft_agent::agent_packs::find(PACK_ID)
            .expect("still installed")
            .manifest
            .version,
        bundled_version
    );

    // 3. An install NEWER than the bundle — a hand-rolled or registry-installed
    //    version — must survive a boot. Downgrading someone's pack because the
    //    binary shipped an older copy is the failure this guards.
    let manifest_path = data_dir
        .join("agent_packs")
        .join(PACK_ID)
        .join("agent_pack.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["version"] = serde_json::json!("99.0.0");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    metalcraft_agent::seed::ensure_defaults();
    assert_eq!(
        metalcraft_agent::agent_packs::find(PACK_ID)
            .expect("still installed")
            .manifest
            .version,
        "99.0.0",
        "an equal-or-newer install must be left untouched"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
