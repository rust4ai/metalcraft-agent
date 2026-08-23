//! Wire-up test for the **metalcraft-code** integration.
//!
//! No network. Seeds bundled packs into an isolated data dir, enables
//! `metalcraft-code`, loads its persona, and asserts every tool resolves to a
//! parseable api-tool config targeting code.metalcraftai.com's `/api/v1` and
//! authenticating with `$METALCRAFT_TOKEN`. Also checks the approval policy:
//! reads auto-approve; clone/write/exec/git/etc. require approval.
//!
//! Run: `cargo test --test metalcraft_code_spice_test`

use std::sync::Once;

use metalcraft_agent::approval::{OperationKind, PermissionLevel};
use metalcraft_agent::persona::Persona;
use metalcraft_agent::{integrations, paths, seed};

const PACK_ID: &str = "metalcraft-code";
const PERSONA_SLUG: &str = "metalcraft-code-agent";

const EXPECTED_TOOLS: &[&str] = &[
    "mcode_whoami",
    "mcode_list_installations",
    "mcode_list_repos",
    "mcode_list_workspaces",
    "mcode_create_workspace",
    "mcode_get_workspace",
    "mcode_delete_workspace",
    "mcode_wake_workspace",
    "mcode_hibernate_workspace",
    "mcode_clone",
    "mcode_read_file",
    "mcode_list_dir",
    "mcode_write_file",
    "mcode_delete_path",
    "mcode_exec",
    "mcode_build",
    "mcode_test",
    "mcode_git",
    "mcode_configure_actions",
    "mcode_expose",
    "mcode_list_runs",
    "mcode_get_run",
];

const READ_TOOLS: &[&str] = &[
    "mcode_whoami",
    "mcode_list_installations",
    "mcode_list_repos",
    "mcode_list_workspaces",
    "mcode_get_workspace",
    "mcode_read_file",
    "mcode_list_dir",
    "mcode_list_runs",
    "mcode_get_run",
];

const WRITE_TOOLS: &[&str] = &[
    "mcode_create_workspace",
    "mcode_delete_workspace",
    "mcode_wake_workspace",
    "mcode_hibernate_workspace",
    "mcode_clone",
    "mcode_write_file",
    "mcode_delete_path",
    "mcode_exec",
    "mcode_build",
    "mcode_test",
    "mcode_git",
    "mcode_configure_actions",
    "mcode_expose",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-mcode-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment; guarded by Once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        seed::ensure_defaults();
        integrations::set_enabled(PACK_ID, true).expect("enable metalcraft-code pack");
    });
}

#[test]
fn metalcraft_code_pack_wires_up() {
    init();

    assert!(
        integrations::is_enabled(PACK_ID),
        "pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("metalcraft-code-agent persona should resolve from the enabled pack");
    assert!(persona.integrations.iter().any(|p| p == PACK_ID));
    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "missing expected tool `{tool}`"
        );
    }
    assert!(resolved.iter().any(|t| t == "load_skill"));
    assert!(persona.skills.iter().any(|s| s == "metalcraft-code"));

    let api_tools_dir = paths::api_tools_dir();
    for tool in EXPECTED_TOOLS {
        let (path, _origin) =
            integrations::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .unwrap_or_else(|| panic!("api tool `{tool}` should resolve"));
        let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("`{tool}` invalid JSON: {e}"));
        assert_eq!(cfg["name"], *tool);
        assert!(
            cfg["url"]
                .as_str()
                .is_some_and(|u| u.contains("https://code.metalcraftai.com")),
            "`{tool}` should target the fixed code.metalcraftai.com base"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$METALCRAFT_TOKEN")),
            "`{tool}` should authenticate with $METALCRAFT_TOKEN"
        );
    }

    // Per-workspace tools address the workspace by {id}.
    for tool in [
        "mcode_get_workspace",
        "mcode_clone",
        "mcode_exec",
        "mcode_git",
    ] {
        let (p, _) =
            integrations::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .expect("resolves");
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(
            cfg["url"].as_str().is_some_and(|u| u.contains("{id}")),
            "`{tool}` should address the workspace by {{id}}"
        );
    }

    let pack = integrations::find_installed(PACK_ID).expect("pack installed");
    let readme = pack.readme().expect("README");
    assert!(readme.contains("METALCRAFT_TOKEN") && readme.contains("code.metalcraftai.com"));
    assert_eq!(
        pack.item_slugs("api_tools", "json").len(),
        EXPECTED_TOOLS.len()
    );

    let recommended = integrations::recommended_env();
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
