//! Agent packs end to end: build an archive, verify it, install it, break it.
//!
//! One `#[test]` so the process-global `METALCRAFT_DATA_DIR` isn't raced.

use metalcraft_agent::agent_packs::{self, Bundle, bundle, manifest::*};
use std::collections::BTreeMap;
use std::fs;

fn persona(slug: &str, packs: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "name": slug,
        "description": format!("{slug} persona"),
        "tools": ["read_file", "load_skill"],
        "packs": packs,
        "skills": ["knife-skills"],
        "system_prompt": format!("You are {slug}."),
    }))
    .unwrap()
}

fn preset(slug: &str, personas: &[(&str, &str)], packs: &[&str]) -> Vec<u8> {
    let roster: Vec<_> = personas
        .iter()
        .map(|(s, role)| serde_json::json!({ "slug": s, "role": role }))
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "slug": slug,
        "name": "Amy's Kitchen Agent",
        "description": "A chef agent",
        "default_persona": personas[0].0,
        "personas": roster,
        "skills": ["knife-skills"],
        "integration_packs": packs,
        "version": "1.4.0",
    }))
    .unwrap()
}

fn api_tool(name: &str, method: &str, url: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "name": name, "description": name, "method": method, "url": url,
        "headers": { "Authorization": "Bearer $METALCRAFT_TOKEN" },
        "parameters": { "type": "object", "properties": {} },
    }))
    .unwrap()
}

fn pack_manifest(id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "name": id, "description": "t", "version": "1.7.1",
        "requires_env": ["METALCRAFT_TOKEN"],
    }))
    .unwrap()
}

/// A complete, valid archive.
fn good_files() -> BTreeMap<String, Vec<u8>> {
    let mut f = BTreeMap::new();
    f.insert(
        "agent_presets/amy-kitchen.json".into(),
        preset("amy-kitchen", &[("amy", "default"), ("amy-shopper", "subagent")], &["metalcraft-calendar"]),
    );
    f.insert(
        "agent_presets/amy-kitchen/memories.jsonl".into(),
        b"{\"kind\":\"Semantic\",\"content\":\"Amy braises at 2:1 mirepoix to leek.\",\"summary\":\"braise base\",\"entity\":\"braising\",\"importance\":7.0}\n{\"kind\":\"Procedural\",\"content\":\"Sear first.\",\"summary\":\"sear\"}\n".to_vec(),
    );
    f.insert("personas/amy.json".into(), persona("amy", &["metalcraft-calendar"]));
    f.insert("personas/amy-shopper.json".into(), persona("amy-shopper", &["metalcraft-calendar"]));
    f.insert("skills/knife-skills.md".into(), b"# Knife skills\nPinch the blade.\n".to_vec());
    f.insert(
        "integration_packs/metalcraft-calendar/pack.json".into(),
        pack_manifest("metalcraft-calendar"),
    );
    f.insert(
        "integration_packs/metalcraft-calendar/api_tools/mcal_list.json".into(),
        api_tool("mcal_list", "GET", "https://calendar.metalcraftai.com/api/v1/calendars"),
    );
    f.insert(
        "integration_packs/metalcraft-calendar/api_tools/mcal_create.json".into(),
        api_tool("mcal_create", "POST", "https://calendar.metalcraftai.com/api/v1/events"),
    );
    f
}

fn good_manifest() -> AgentPackManifest {
    let mut m = AgentPackManifest::new("amy-kitchen-agent", "Amy's Kitchen Agent", "1.4.0");
    m.handle = Some("amy_kitchen".into());
    m.presets = vec!["amy-kitchen".into()];
    m.provides = Provides {
        personas: vec!["amy".into(), "amy-shopper".into()],
        skills: vec!["knife-skills".into()],
        integration_packs: vec![PackRef {
            id: "metalcraft-calendar".into(),
            version: "1.7.1".into(),
            content_sha256: None,
            source: Some("https://packs.metalcraftai.com".into()),
        }],
    };
    m
}

#[test]
fn an_agent_pack_round_trips_and_refuses_what_it_should() {
    let data_dir = std::env::temp_dir().join(format!("mc-ap5-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    // ── build ───────────────────────────────────────────────────────────────
    let archive = bundle::write(good_manifest(), good_files()).expect("write archive");
    assert!(archive.len() > 200);

    // ── read + verify ───────────────────────────────────────────────────────
    let parsed = Bundle::read(&archive).expect("read archive");
    assert_eq!(parsed.manifest.id, "amy-kitchen-agent");
    assert_eq!(parsed.preset_slug(), Some("amy-kitchen"));
    assert!(parsed.manifest.content_sha256.is_some(), "the builder must pin its contents");

    // Consent is derived from the tools, not from what the author claimed.
    assert_eq!(parsed.consent.domains, vec!["calendar.metalcraftai.com"]);
    assert_eq!(parsed.consent.tools, vec!["mcal_create", "mcal_list"]);
    assert_eq!(
        parsed.consent.mutating_tools,
        vec!["mcal_create"],
        "the dialog must be able to say this agent can write, not just read"
    );
    let env: Vec<&str> = parsed.consent.requires_env.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(env, vec!["METALCRAFT_TOKEN"]);

    // ── tampering ───────────────────────────────────────────────────────────
    let mut tampered = good_files();
    tampered.insert("personas/amy.json".into(), persona("amy-evil", &["metalcraft-calendar"]));
    let mut m = good_manifest();
    m.content_sha256 = Some(bundle::content_hash(&good_files())); // hash of the *original*
    // Rebuild the zip by hand so `write` doesn't helpfully re-pin it.
    let hand_built = {
        let mut f = tampered.clone();
        f.insert("agent_pack.json".into(), serde_json::to_vec(&m).unwrap());
        zip_of(&f)
    };
    let err = Bundle::read(&hand_built).expect_err("a tampered archive must not install");
    assert!(err.contains("does not match"), "{err}");

    // ── validation ──────────────────────────────────────────────────────────
    // A persona the archive doesn't carry.
    let mut missing = good_files();
    missing.remove("personas/amy-shopper.json");
    let err = build_and_read(good_manifest(), missing).expect_err("missing persona must fail");
    assert!(err.contains("amy-shopper"), "{err}");

    // A skill the archive doesn't carry.
    let mut missing = good_files();
    missing.remove("skills/knife-skills.md");
    let err = build_and_read(good_manifest(), missing).expect_err("missing skill must fail");
    assert!(err.contains("knife-skills"), "{err}");

    // An integration pack the archive doesn't vendor — the self-contained rule.
    let mut missing = good_files();
    missing.retain(|k, _| !k.starts_with("integration_packs/"));
    let err = build_and_read(good_manifest(), missing).expect_err("missing pack must fail");
    assert!(err.contains("does not vendor"), "{err}");

    // Containment: a persona reaching a pack its preset never declared.
    let mut escaping = good_files();
    escaping.insert("personas/amy.json".into(), persona("amy", &["metalcraft-calendar", "secret-exfil"]));
    let err = build_and_read(good_manifest(), escaping).expect_err("containment must hold");
    assert!(err.contains("does not declare"), "{err}");

    // Two presets: rejected, one agent per pack.
    let mut two = good_manifest();
    two.presets = vec!["amy-kitchen".into(), "other".into()];
    let err = build_and_read(two, good_files()).expect_err("multi-preset must be refused");
    assert!(err.contains("exactly one"), "{err}");

    // ── install ─────────────────────────────────────────────────────────────
    let report = agent_packs::install(&archive, "bundle").expect("install");
    assert_eq!(report.id, "amy-kitchen-agent");
    assert_eq!(report.presets, vec!["amy-kitchen"]);
    assert_eq!(report.personas.len(), 2);
    assert_eq!(report.skills, vec!["knife-skills"]);
    assert_eq!(report.packs_stored, vec!["metalcraft-calendar"]);
    assert!(report.packs_deduplicated.is_empty());
    assert_eq!(report.memories_indexed, 2, "the preset's memory base is built at install");
    assert_eq!(
        report.missing_env,
        vec!["METALCRAFT_TOKEN"],
        "a missing credential is a warning the report carries, not a failed install"
    );

    // The preset now resolves through the normal layered lookup.
    let installed = agent_packs::find("amy-kitchen-agent").expect("installed");
    assert_eq!(installed.manifest.version, "1.4.0");
    assert!(agent_packs::pack_dir("amy-kitchen-agent").join("agent_presets/amy-kitchen.json").is_file());
    assert!(
        !agent_packs::pack_dir("amy-kitchen-agent").join("integration_packs").exists(),
        "vendored packs live in the content store, not inside the agent pack"
    );

    // The memory base was built and is loadable.
    let base = metalcraft_agent::memory::instance::load_base("amy-kitchen", "1.4.0")
        .expect("base built at install");
    assert_eq!(base.try_read().map(|b| b.len()).unwrap_or(0), 2);

    // ── dedup: a second pack vendoring identical bytes is free ──────────────
    let mut second = good_manifest();
    second.id = "mitch-reviews-agent".into();
    second.name = "Mitch".into();
    let mut files = good_files();
    files.insert(
        "agent_presets/mitch.json".into(),
        preset("mitch", &[("amy", "default")], &["metalcraft-calendar"]),
    );
    files.remove("agent_presets/amy-kitchen.json");
    files.remove("agent_presets/amy-kitchen/memories.jsonl");
    files.remove("personas/amy-shopper.json");
    second.presets = vec!["mitch".into()];
    let archive2 = bundle::write(second, files).expect("write second");
    let report2 = agent_packs::install(&archive2, "bundle").expect("install second");
    assert_eq!(
        report2.packs_deduplicated,
        vec!["metalcraft-calendar"],
        "identical vendored bytes must collapse to one store entry"
    );
    assert!(report2.packs_stored.is_empty());

    let counts = agent_packs::store::refcounts();
    assert_eq!(counts.values().copied().max(), Some(2), "one entry, two referents");

    // ── downgrade ───────────────────────────────────────────────────────────
    let mut older = good_manifest();
    older.version = "1.0.0".into();
    let old_archive = bundle::write(older, good_files()).unwrap();
    let err = agent_packs::install(&old_archive, "bundle").expect_err("downgrade must be refused");
    assert!(err.contains("older than"), "{err}");

    // ── uninstall ───────────────────────────────────────────────────────────
    let r = agent_packs::uninstall("mitch-reviews-agent", false).expect("uninstall");
    assert!(r.orphaned_agents.is_empty());
    assert_eq!(r.packs_freed, 0, "the pack is still referenced by the other agent pack");
    assert!(agent_packs::find("mitch-reviews-agent").is_none());

    let r = agent_packs::uninstall("amy-kitchen-agent", false).expect("uninstall last");
    assert_eq!(r.packs_freed, 1, "the last referent gone means the store entry is collectable");
    assert!(agent_packs::store::refcounts().is_empty());

    let _ = fs::remove_dir_all(&data_dir);
}

fn build_and_read(m: AgentPackManifest, files: BTreeMap<String, Vec<u8>>) -> Result<Bundle, String> {
    let bytes = bundle::write(m, files)?;
    Bundle::read(&bytes)
}

/// Zip a file map verbatim, so a test can construct an archive the builder would
/// never produce (a manifest that lies about its contents).
fn zip_of(files: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    use std::io::{Cursor, Write};
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (path, bytes) in files {
            zip.start_file(path.as_str(), opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}
