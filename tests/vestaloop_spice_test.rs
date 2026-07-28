//! Wire-up test for the **vestaloop** integration pack.
//!
//! No network. Seeds the bundled packs into an isolated data dir, enables
//! `vestaloop`, loads the `vestaloop-agent` persona, and asserts every tool it
//! resolves is a parseable api-tool config that targets the portal's `/api/v1`
//! REST API and authenticates with `$VESTALOOP_API_KEY`. Proves the manifest,
//! persona, skill, README, and api tools all agree — without any live call.
//!
//! Run: `cargo test --test vestaloop_spice_test`

use std::sync::Once;

use metalcraft_agent::approval::{OperationKind, PermissionLevel};
use metalcraft_agent::persona::Persona;
use metalcraft_agent::{integration_packs, paths, seed};

const PACK_ID: &str = "vestaloop";
const PERSONA_SLUG: &str = "vestaloop-agent";

/// Every api tool the pack ships — kept in lockstep with
/// `seed/integration_packs/vestaloop/api_tools/`.
const EXPECTED_TOOLS: &[&str] = &[
    "vestaloop_whoami",
    "vestaloop_list_events",
    "vestaloop_get_event",
    "vestaloop_create_event",
    "vestaloop_update_event",
    "vestaloop_delete_event",
    "vestaloop_sync",
];

/// Reads + the idempotent Google refresh — should auto-approve.
const READ_TOOLS: &[&str] = &[
    "vestaloop_whoami",
    "vestaloop_list_events",
    "vestaloop_get_event",
    "vestaloop_sync",
];

/// Event mutations — require approval.
const WRITE_TOOLS: &[&str] = &[
    "vestaloop_create_event",
    "vestaloop_update_event",
    "vestaloop_delete_event",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-vestaloop-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable vestaloop pack");
    });
}

#[test]
fn vestaloop_pack_wires_up() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "vestaloop pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("vestaloop-agent persona should resolve from the enabled pack");
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the vestaloop pack via `packs`"
    );
    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "persona's resolved tools are missing expected tool `{tool}`"
        );
    }
    assert!(resolved.iter().any(|t| t == "load_skill"));
    assert!(persona.skills.iter().any(|s| s == "vestaloop-calendar"));

    // Every tool resolves to a parseable api-tool config that targets the portal's
    // /api/v1 API and authenticates with the API key.
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
                .is_some_and(|u| u.contains("https://vestaloop.com/api/v1")),
            "api tool `{tool}` should target the fixed https://vestaloop.com/api/v1 base"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$VESTALOOP_API_KEY")),
            "api tool `{tool}` should authenticate with $VESTALOOP_API_KEY"
        );
    }

    // create/update send the event as the JSON body via body_mapping=params.
    for tool in ["vestaloop_create_event", "vestaloop_update_event"] {
        let (p, _) = integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
            .expect("write tool resolves");
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(cfg["body_mapping"], "params", "`{tool}` should map params into the body");
    }

    // README + item slugs (the pack_read data path).
    let pack = integration_packs::find_installed(PACK_ID).expect("vestaloop pack installed");
    let readme = pack.readme().expect("vestaloop pack should ship a README");
    assert!(
        readme.contains("VESTALOOP_API_KEY") && readme.contains("vestaloop.com"),
        "README should explain the API key and the fixed vestaloop.com base"
    );
    assert_eq!(pack.item_slugs("api_tools", "json").len(), EXPECTED_TOOLS.len());
    assert!(pack.item_slugs("personas", "json").iter().any(|s| s == PERSONA_SLUG));
    assert!(pack.item_slugs("skills", "md").iter().any(|s| s == "vestaloop-calendar"));

    // The pack recommends its one env var (the API key) in the key-store UI.
    let recommended = integration_packs::recommended_env();
    assert!(
        recommended
            .iter()
            .any(|(v, packs)| v == "VESTALOOP_API_KEY" && packs.iter().any(|p| p == PACK_ID)),
        "vestaloop pack should recommend VESTALOOP_API_KEY"
    );
    assert!(
        !recommended.iter().any(|(v, _)| v == "VESTALOOP_BASE_URL"),
        "base URL is fixed, so VESTALOOP_BASE_URL should NOT be a required env var"
    );

    // Approval gating: reads/refresh auto-approve, event mutations require approval.
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
