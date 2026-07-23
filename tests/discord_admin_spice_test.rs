//! Wire-up test for the **discord_admin** integration pack.
//!
//! No network. Seeds the bundled packs into an isolated data dir, enables
//! `discord_admin`, loads the `discord-admin-agent` persona, and asserts every
//! tool it resolves is a parseable api-tool config that targets Discord's REST
//! API and authenticates with `$DISCORD_BOT_TOKEN`. Proves the pack is
//! internally consistent — the manifest, persona, skill, and ~30 api tools all
//! agree — without any live call.
//!
//! Run: `cargo test --test discord_admin_spice_test`

use std::sync::Once;

use metalcraft_agent::approval::OperationKind;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::{integration_packs, paths, seed};

const PACK_ID: &str = "discord_admin";
const PERSONA_SLUG: &str = "discord-admin-agent";

/// Every api tool the pack ships — kept in lockstep with
/// `seed/integration_packs/discord_admin/api_tools/`.
const EXPECTED_TOOLS: &[&str] = &[
    "discord_get_guild",
    "discord_modify_guild",
    "discord_list_guild_channels",
    "discord_create_channel",
    "discord_modify_channel",
    "discord_delete_channel",
    "discord_edit_channel_permissions",
    "discord_list_roles",
    "discord_create_role",
    "discord_modify_role",
    "discord_delete_role",
    "discord_add_member_role",
    "discord_remove_member_role",
    "discord_list_members",
    "discord_get_member",
    "discord_search_members",
    "discord_modify_member",
    "discord_kick_member",
    "discord_list_bans",
    "discord_create_ban",
    "discord_remove_ban",
    "discord_delete_message",
    "discord_bulk_delete_messages",
    "discord_pin_message",
    "discord_list_guild_invites",
    "discord_delete_invite",
    "discord_get_audit_log",
    "discord_list_channel_webhooks",
    "discord_create_webhook",
    "discord_delete_webhook",
];

/// Read-only tools that should auto-approve.
const READ_TOOLS: &[&str] = &[
    "discord_get_guild",
    "discord_list_guild_channels",
    "discord_list_roles",
    "discord_list_members",
    "discord_get_member",
    "discord_search_members",
    "discord_list_bans",
    "discord_list_guild_invites",
    "discord_get_audit_log",
    "discord_list_channel_webhooks",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-discord-admin-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable discord_admin pack");
    });
}

#[test]
fn discord_admin_pack_wires_up() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "discord_admin pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("discord-admin-agent persona should resolve from the enabled pack");

    // The persona is scoped to the pack rather than listing each tool.
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the discord_admin pack via `packs`"
    );
    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "persona's resolved tools are missing expected tool `{tool}`"
        );
    }
    assert!(
        resolved.iter().any(|t| t == "load_skill"),
        "persona should include the native load_skill tool"
    );
    assert!(
        persona.skills.iter().any(|s| s == "discord-server-admin"),
        "persona should reference the discord-server-admin skill"
    );

    // Every tool resolves to a parseable api-tool config that targets the
    // Discord REST API and authenticates with the bot token.
    let api_tools_dir = paths::api_tools_dir();
    for tool in EXPECTED_TOOLS {
        let (path, _origin) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .unwrap_or_else(|| panic!("api tool `{tool}` should resolve from the pack"));
        let raw = std::fs::read_to_string(&path).expect("read api tool config");
        let cfg: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("api tool `{tool}` is not valid JSON: {e}"));
        assert_eq!(
            cfg["name"], *tool,
            "api tool `{tool}` config `name` should match its filename"
        );
        assert!(
            cfg["url"]
                .as_str()
                .is_some_and(|u| u.contains("https://discord.com/api/v10")),
            "api tool `{tool}` should target the Discord REST API"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$DISCORD_BOT_TOKEN")),
            "api tool `{tool}` should authenticate with $DISCORD_BOT_TOKEN"
        );
    }

    // pack_read's data path: the pack ships a README setup guide and reports the
    // items it provides. This is what the agent surfaces to walk a user through
    // obtaining the bot token and inviting the bot.
    let pack = integration_packs::find_installed(PACK_ID).expect("discord_admin pack installed");
    let readme = pack.readme().expect("discord_admin pack should ship a README");
    assert!(
        readme.contains("DISCORD_BOT_TOKEN") && readme.contains("bot token"),
        "README should explain the bot token credential and setup"
    );
    let tool_slugs = pack.item_slugs("api_tools", "json");
    assert_eq!(tool_slugs.len(), EXPECTED_TOOLS.len(), "item_slugs should list every api tool");
    assert!(pack.item_slugs("personas", "json").iter().any(|s| s == PERSONA_SLUG));
    assert!(pack.item_slugs("skills", "md").iter().any(|s| s == "discord-server-admin"));

    // The pack recommends the bot token in the key-store UI once enabled.
    let recommended = integration_packs::recommended_env();
    assert!(
        recommended
            .iter()
            .any(|(var, packs)| var == "DISCORD_BOT_TOKEN" && packs.iter().any(|p| p == PACK_ID)),
        "discord_admin pack should recommend DISCORD_BOT_TOKEN"
    );

    // Approval gating: reads auto-approve, mutations require approval.
    let args = serde_json::json!({});
    for tool in READ_TOOLS {
        assert_eq!(
            OperationKind::classify(tool, &args),
            OperationKind::ReadFile,
            "read tool `{tool}` should classify as ReadFile (auto-approve)"
        );
    }
    for tool in EXPECTED_TOOLS.iter().filter(|t| !READ_TOOLS.contains(t)) {
        assert_eq!(
            OperationKind::classify(tool, &args),
            OperationKind::DiscordAction,
            "mutating tool `{tool}` should classify as DiscordAction (requires approval)"
        );
    }
}
