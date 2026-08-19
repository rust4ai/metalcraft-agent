//! Wire-up test for the **metalcraft-notes** integration pack.
//!
//! No network. Seeds bundled packs into an isolated data dir, enables
//! `metalcraft-notes`, loads its persona, and asserts every tool resolves to a
//! parseable api-tool config targeting notes.metalcraftai.com's `/api/v1` and
//! authenticating with `$METALCRAFT_TOKEN`.
//!
//! Run: `cargo test --test metalcraft_notes_spice_test`

use std::sync::Once;

use metalcraft_agent::approval::{OperationKind, PermissionLevel};
use metalcraft_agent::persona::Persona;
use metalcraft_agent::{integration_packs, paths, seed};

const PACK_ID: &str = "metalcraft-notes";
const PERSONA_SLUG: &str = "metalcraft-notes-agent";

const EXPECTED_TOOLS: &[&str] = &[
    "mnote_whoami",
    "mnote_list_notes",
    "mnote_get_note",
    "mnote_links",
    "mnote_create_note",
    "mnote_update_note",
    "mnote_delete_note",
    "mnote_list_categories",
    "mnote_create_category",
];

const READ_TOOLS: &[&str] = &[
    "mnote_whoami",
    "mnote_list_notes",
    "mnote_get_note",
    "mnote_list_categories",
    // Reads, but the name matches neither `_list` nor `_get` — the approval rule has to
    // name it explicitly, so assert it rather than trusting the pattern.
    "mnote_links",
];

const WRITE_TOOLS: &[&str] =
    &["mnote_create_note", "mnote_update_note", "mnote_delete_note", "mnote_create_category"];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-mnote-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment; guarded by Once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable metalcraft-notes pack");
    });
}

#[test]
fn metalcraft_notes_pack_wires_up() {
    init();

    assert!(integration_packs::is_enabled(PACK_ID), "pack should be enabled after init()");

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("metalcraft-notes-agent persona should resolve from the enabled pack");
    assert!(persona.packs.iter().any(|p| p == PACK_ID));
    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(resolved.iter().any(|t| t == tool), "missing expected tool `{tool}`");
    }
    assert!(resolved.iter().any(|t| t == "load_skill"));
    assert!(persona.skills.iter().any(|s| s == "metalcraft-notes"));

    let api_tools_dir = paths::api_tools_dir();
    for tool in EXPECTED_TOOLS {
        let (path, _origin) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .unwrap_or_else(|| panic!("api tool `{tool}` should resolve"));
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("`{tool}` invalid JSON: {e}"));
        assert_eq!(cfg["name"], *tool);
        assert!(
            cfg["url"].as_str().is_some_and(|u| u.contains("https://notes.metalcraftai.com/api/v1")),
            "`{tool}` should target the fixed notes.metalcraftai.com/api/v1 base"
        );
        assert!(
            cfg["headers"]["Authorization"].as_str().is_some_and(|h| h.contains("$METALCRAFT_TOKEN")),
            "`{tool}` should authenticate with $METALCRAFT_TOKEN"
        );
    }

    // Per-note tools address the note by {slug}.
    for tool in ["mnote_get_note", "mnote_update_note", "mnote_delete_note", "mnote_links"] {
        let (p, _) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .expect("resolves");
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(
            cfg["url"].as_str().is_some_and(|u| u.contains("{slug}")),
            "`{tool}` should address the note by {{slug}}"
        );
    }

    let pack = integration_packs::find_installed(PACK_ID).expect("pack installed");
    let readme = pack.readme().expect("README");
    assert!(readme.contains("METALCRAFT_TOKEN") && readme.contains("notes.metalcraftai.com"));
    assert_eq!(pack.item_slugs("api_tools", "json").len(), EXPECTED_TOOLS.len());

    // ── The pack must TEACH wikilinks, not merely expose the endpoint ──────────────
    //
    // The `[[slug]]` machinery in metalcraft-notes is worthless if the agent — the app's
    // primary author — is never told the syntax exists. A model that hasn't been told
    // writes "see the Q3 plan note" and creates zero edges. That failure is silent: every
    // call succeeds, the notes look fine, and the graph just stays empty. These assertions
    // are the tripwire, so a future prompt tidy-up can't quietly undo it.
    let tool_json = |tool: &str| -> String {
        let (p, _) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .expect("resolves");
        std::fs::read_to_string(&p).unwrap()
    };

    for tool in ["mnote_create_note", "mnote_update_note"] {
        let cfg: serde_json::Value = serde_json::from_str(&tool_json(tool)).unwrap();
        let desc = cfg["description"].as_str().unwrap();
        assert!(desc.contains("[[slug]]"), "`{tool}` must teach the [[slug]] link syntax");
        assert!(
            desc.contains('|') && desc.to_lowercase().contains("break"),
            "`{tool}` must warn that a pipe in the display text voids the link"
        );
    }

    // create_note must expose an explicit slug, or "create the note this broken link
    // points at" is impossible from the agent side.
    let create: serde_json::Value = serde_json::from_str(&tool_json("mnote_create_note")).unwrap();
    assert!(
        create["parameters"]["properties"]["slug"].is_object(),
        "mnote_create_note must accept an explicit `slug`"
    );

    let skill_src = pack
        .item_slugs("skills", "md")
        .iter()
        .find(|s| *s == "metalcraft-notes")
        .map(|_| {
            let (p, _) = integration_packs::resolve_file(
                &paths::skills_dir(),
                "skills",
                "metalcraft-notes.md",
            )
            .expect("skill resolves");
            std::fs::read_to_string(p).unwrap()
        })
        .expect("skill ships with the pack");
    // Assert the load-bearing FACTS, not just that the string "[[slug]]" appears somewhere
    // — an earlier version of this check passed even with the whole linking section
    // deleted, because the syntax was incidentally mentioned in a workflow bullet.
    for needle in [
        "[[slug]]",              // the syntax
        "[[slug|Display Text]]", // the alias form
        "mnote_links",           // how to traverse
        "broken",                // forward links / creating the note a link points at
        "silently",              // the pipe/bracket warning
    ] {
        assert!(
            skill_src.contains(needle),
            "the skill must document {needle:?} — the linking section looks gutted"
        );
    }

    let persona_src = {
        let (p, _) = integration_packs::resolve_file(
            &paths::personas_dir(),
            "personas",
            "metalcraft-notes-agent.json",
        )
        .expect("persona resolves");
        std::fs::read_to_string(p).unwrap()
    };
    // Check the SYSTEM PROMPT specifically. Reading the whole file would pass on the
    // `description` field alone, which the model never sees.
    let persona_json: serde_json::Value = serde_json::from_str(&persona_src).unwrap();
    let system_prompt = persona_json["system_prompt"].as_str().unwrap();
    for needle in ["[[slug]]", "mnote_links"] {
        assert!(
            system_prompt.contains(needle),
            "the persona SYSTEM PROMPT must teach {needle:?} — this is what the model reads"
        );
    }

    let recommended = integration_packs::recommended_env();
    assert!(
        recommended
            .iter()
            .any(|(v, packs)| v == "METALCRAFT_TOKEN" && packs.iter().any(|p| p == PACK_ID)),
        "pack should recommend METALCRAFT_TOKEN"
    );

    let args = serde_json::json!({});
    for tool in READ_TOOLS {
        assert_eq!(
            OperationKind::classify(tool, &args).default_permission(),
            PermissionLevel::AutoApprove,
            "read tool `{tool}` should auto-approve"
        );
    }
    for tool in WRITE_TOOLS {
        assert_eq!(
            OperationKind::classify(tool, &args).default_permission(),
            PermissionLevel::RequiresApproval,
            "mutating tool `{tool}` should require approval"
        );
    }
}
