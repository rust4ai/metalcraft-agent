//! Wire-up test for the **metalcraft-meet** integration pack.
//!
//! No network. Seeds the bundled packs into an isolated data dir, enables
//! `metalcraft-meet`, loads the `metalcraft-meet-agent` persona, and asserts every tool it
//! resolves is a parseable api-tool config targeting meet.metalcraftai.com's `/api/v1` and
//! authenticating with `$METALCRAFT_TOKEN`.
//!
//! Run: `cargo test --test metalcraft_meet_spice_test`

use std::sync::Once;

use metalcraft_agent::approval::{OperationKind, PermissionLevel};
use metalcraft_agent::persona::Persona;
use metalcraft_agent::{integration_packs, paths, seed};

const PACK_ID: &str = "metalcraft-meet";
const PERSONA_SLUG: &str = "metalcraft-meet-agent";

const EXPECTED_TOOLS: &[&str] = &[
    "mmeet_whoami",
    "mmeet_schedule_meeting",
    "mmeet_list_meetings",
    "mmeet_get_meeting",
    "mmeet_update_meeting",
    "mmeet_cancel_meeting",
    "mmeet_add_participants",
];

/// Reads — should auto-approve.
const READ_TOOLS: &[&str] = &["mmeet_whoami", "mmeet_list_meetings", "mmeet_get_meeting"];

/// Mutations (create the meeting, write the calendar, email people) — require approval.
const WRITE_TOOLS: &[&str] = &[
    "mmeet_schedule_meeting",
    "mmeet_update_meeting",
    "mmeet_cancel_meeting",
    "mmeet_add_participants",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-mmeet-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable metalcraft-meet pack");
    });
}

#[test]
fn metalcraft_meet_pack_wires_up() {
    init();

    assert!(integration_packs::is_enabled(PACK_ID), "pack should be enabled after init()");

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("metalcraft-meet-agent persona should resolve from the enabled pack");
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the metalcraft-meet pack via `packs`"
    );
    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "persona's resolved tools are missing expected tool `{tool}`"
        );
    }
    assert!(resolved.iter().any(|t| t == "load_skill"));
    assert!(persona.skills.iter().any(|s| s == "metalcraft-meet"));

    let api_tools_dir = paths::api_tools_dir();
    for tool in EXPECTED_TOOLS {
        let (path, _origin) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .unwrap_or_else(|| panic!("api tool `{tool}` should resolve from the pack"));
        let raw = std::fs::read_to_string(&path).expect("read api tool config");
        let cfg: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("api tool `{tool}` is not valid JSON: {e}"));
        assert_eq!(cfg["name"], *tool, "api tool `{tool}` name should match filename");
        assert!(
            cfg["url"]
                .as_str()
                .is_some_and(|u| u.contains("https://meet.metalcraftai.com/api/v1")),
            "api tool `{tool}` should target the fixed meet.metalcraftai.com/api/v1 base"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$METALCRAFT_TOKEN")),
            "api tool `{tool}` should authenticate with $METALCRAFT_TOKEN"
        );
    }

    // Write tools that carry a body map params into it.
    for tool in ["mmeet_schedule_meeting", "mmeet_update_meeting", "mmeet_add_participants"] {
        let (p, _) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .expect("write tool resolves");
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(cfg["body_mapping"], "params", "`{tool}` should map params into the body");
    }

    // Slug-scoped tools must carry the {slug} path placeholder.
    for tool in ["mmeet_get_meeting", "mmeet_update_meeting", "mmeet_cancel_meeting", "mmeet_add_participants"] {
        let (p, _) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .expect("slug tool resolves");
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(
            cfg["url"].as_str().is_some_and(|u| u.contains("{slug}")),
            "`{tool}` should address the meeting by {{slug}}"
        );
    }

    let pack = integration_packs::find_installed(PACK_ID).expect("pack installed");
    let readme = pack.readme().expect("pack should ship a README");
    assert!(
        readme.contains("METALCRAFT_TOKEN") && readme.contains("meet.metalcraftai.com"),
        "README should explain the token and the fixed meet.metalcraftai.com base"
    );
    assert_eq!(pack.item_slugs("api_tools", "json").len(), EXPECTED_TOOLS.len());
    assert!(pack.item_slugs("personas", "json").iter().any(|s| s == PERSONA_SLUG));
    assert!(pack.item_slugs("skills", "md").iter().any(|s| s == "metalcraft-meet"));

    let recommended = integration_packs::recommended_env();
    assert!(
        recommended
            .iter()
            .any(|(v, packs)| v == "METALCRAFT_TOKEN" && packs.iter().any(|p| p == PACK_ID)),
        "pack should recommend METALCRAFT_TOKEN"
    );

    // Approval gating: reads auto-approve, mutations require approval.
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
