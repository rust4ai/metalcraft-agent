use metalcraft_agent::tools::http_api::HttpApiToolConfig;
use std::path::Path;

// Discord api-tools moved into the discord integration pack (commit 063c07d).
const SEED_DIR: &str = "seed/integration_packs/discord/api_tools";

const DISCORD_TOOLS: &[&str] = &[
    "discord_send_message",
    "discord_edit_message",
    "discord_add_reaction",
    "discord_get_messages",
    "discord_get_channel_info",
];

#[test]
fn all_seed_discord_configs_parse() {
    let dir = Path::new(SEED_DIR);
    for name in DISCORD_TOOLS {
        let path = dir.join(format!("{name}.json"));
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
        let config: HttpApiToolConfig = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()));
        assert_eq!(config.name, *name, "name mismatch in {name}.json");
        assert!(!config.description.is_empty(), "empty description in {name}.json");
        assert!(!config.url.is_empty(), "empty url in {name}.json");
    }
}

#[test]
fn discord_send_message_config_details() {
    let content = std::fs::read_to_string(Path::new(SEED_DIR).join("discord_send_message.json")).unwrap();
    let config: HttpApiToolConfig = serde_json::from_str(&content).unwrap();
    assert_eq!(config.method, "POST");
    assert_eq!(config.body_mapping, "params");
    assert_eq!(config.body_defaults.get("platform").unwrap(), "discord");
    assert!(config.url.contains("/messages"));
    assert!(config.headers.contains_key("Authorization"));
    // parameters should require channel_id and content
    let required = config.parameters["required"].as_array().unwrap();
    let required_strs: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(required_strs.contains(&"channel_id"));
    assert!(required_strs.contains(&"content"));
}

#[test]
fn discord_get_messages_config_details() {
    let content = std::fs::read_to_string(Path::new(SEED_DIR).join("discord_get_messages.json")).unwrap();
    let config: HttpApiToolConfig = serde_json::from_str(&content).unwrap();
    assert_eq!(config.method, "GET");
    assert_eq!(config.body_mapping, "none");
    assert!(config.body_defaults.is_empty());
    assert!(config.url.contains("{channel_id}"));
    assert!(config.url.contains("platform=discord"));
    // limit is optional — only channel_id required
    let required = config.parameters["required"].as_array().unwrap();
    assert_eq!(required.len(), 1);
    assert_eq!(required[0].as_str().unwrap(), "channel_id");
}

#[test]
fn discord_edit_message_has_message_id_placeholder() {
    let content = std::fs::read_to_string(Path::new(SEED_DIR).join("discord_edit_message.json")).unwrap();
    let config: HttpApiToolConfig = serde_json::from_str(&content).unwrap();
    assert_eq!(config.method, "PATCH");
    assert!(config.url.contains("{message_id}"));
}

#[test]
fn discord_add_reaction_uses_put() {
    let content = std::fs::read_to_string(Path::new(SEED_DIR).join("discord_add_reaction.json")).unwrap();
    let config: HttpApiToolConfig = serde_json::from_str(&content).unwrap();
    assert_eq!(config.method, "PUT");
    assert!(config.url.contains("/reactions"));
}

#[test]
fn discord_get_channel_info_config() {
    let content = std::fs::read_to_string(Path::new(SEED_DIR).join("discord_get_channel_info.json")).unwrap();
    let config: HttpApiToolConfig = serde_json::from_str(&content).unwrap();
    assert_eq!(config.method, "GET");
    assert_eq!(config.body_mapping, "none");
    assert!(config.url.contains("{channel_id}"));
    assert!(config.url.contains("platform=discord"));
}
