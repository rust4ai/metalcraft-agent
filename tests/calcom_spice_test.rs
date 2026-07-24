//! Wire-up test for the **calcom** integration pack.
//!
//! No network. Seeds the bundled packs into an isolated data dir, enables
//! `calcom`, loads the `calcom-agent` persona, and asserts every tool it
//! resolves is a parseable api-tool config that targets cal.com's v2 REST API
//! and authenticates with `$CALCOM_API_KEY`. Proves the manifest, persona,
//! skill, README, and api tools all agree — without any live call.
//!
//! Run: `cargo test --test calcom_spice_test`

use std::sync::Once;

use metalcraft_agent::approval::OperationKind;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::{integration_packs, paths, seed};

const PACK_ID: &str = "calcom";
const PERSONA_SLUG: &str = "calcom-agent";

/// Every api tool the pack ships — kept in lockstep with
/// `seed/integration_packs/calcom/api_tools/`.
const EXPECTED_TOOLS: &[&str] = &[
    "calcom_get_me",
    "calcom_list_event_types",
    "calcom_get_available_slots",
    "calcom_create_booking",
    "calcom_list_bookings",
    "calcom_get_booking",
    "calcom_cancel_booking",
    "calcom_reschedule_booking",
];

/// Read-only tools that should auto-approve.
const READ_TOOLS: &[&str] = &[
    "calcom_get_me",
    "calcom_list_event_types",
    "calcom_get_available_slots",
    "calcom_list_bookings",
    "calcom_get_booking",
];

/// Booking mutations that require approval.
const WRITE_TOOLS: &[&str] = &[
    "calcom_create_booking",
    "calcom_cancel_booking",
    "calcom_reschedule_booking",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-calcom-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable calcom pack");
    });
}

#[test]
fn calcom_pack_wires_up() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "calcom pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("calcom-agent persona should resolve from the enabled pack");
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the calcom pack via `packs`"
    );
    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "persona's resolved tools are missing expected tool `{tool}`"
        );
    }
    assert!(resolved.iter().any(|t| t == "load_skill"));
    assert!(persona.skills.iter().any(|s| s == "calcom-scheduling"));

    // Every tool resolves to a parseable api-tool config that targets the cal.com
    // v2 API and authenticates with the API key.
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
            cfg["url"].as_str().is_some_and(|u| u.contains("https://api.cal.com/v2")),
            "api tool `{tool}` should target the cal.com v2 API"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$CALCOM_API_KEY")),
            "api tool `{tool}` should authenticate with $CALCOM_API_KEY"
        );
        assert!(
            cfg["headers"]["cal-api-version"].as_str().is_some_and(|v| !v.is_empty()),
            "api tool `{tool}` should pin a cal-api-version"
        );
    }

    // create_booking nests the attendee via params_nested + param_paths so cal.com's
    // required `attendee` object is produced from flat scalar params.
    let (create_path, _) =
        integration_packs::resolve_file(&api_tools_dir, "api_tools", "calcom_create_booking.json")
            .expect("create_booking resolves");
    let create: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&create_path).unwrap()).unwrap();
    assert_eq!(create["body_mapping"], "params_nested");
    assert_eq!(create["param_paths"]["name"], "attendee.name");
    assert_eq!(create["param_paths"]["timeZone"], "attendee.timeZone");

    // README + item slugs (the pack_read data path).
    let pack = integration_packs::find_installed(PACK_ID).expect("calcom pack installed");
    let readme = pack.readme().expect("calcom pack should ship a README");
    assert!(
        readme.contains("CALCOM_API_KEY") && readme.to_lowercase().contains("google calendar"),
        "README should explain the API key and the Google Calendar sync"
    );
    assert_eq!(pack.item_slugs("api_tools", "json").len(), EXPECTED_TOOLS.len());
    assert!(pack.item_slugs("personas", "json").iter().any(|s| s == PERSONA_SLUG));
    assert!(pack.item_slugs("skills", "md").iter().any(|s| s == "calcom-scheduling"));

    // The pack recommends the API key in the key-store UI once enabled.
    let recommended = integration_packs::recommended_env();
    assert!(
        recommended
            .iter()
            .any(|(var, packs)| var == "CALCOM_API_KEY" && packs.iter().any(|p| p == PACK_ID)),
        "calcom pack should recommend CALCOM_API_KEY"
    );

    // Approval gating: reads auto-approve, booking mutations require approval.
    let args = serde_json::json!({});
    for tool in READ_TOOLS {
        assert_eq!(
            OperationKind::classify(tool, &args),
            OperationKind::ReadFile,
            "read tool `{tool}` should auto-approve"
        );
    }
    for tool in WRITE_TOOLS {
        assert_eq!(
            OperationKind::classify(tool, &args).default_permission(),
            metalcraft_agent::approval::PermissionLevel::RequiresApproval,
            "mutating tool `{tool}` should require approval"
        );
    }
}
