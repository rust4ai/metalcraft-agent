//! Gateway channels — the agent's *internal* messaging gateway.
//!
//! A **channel type** is a declarative JSON manifest (modeled on integration
//! packs) describing a messaging platform: its display metadata, the API keys
//! it recommends (`requires_env` → the key-store "recommended keys" UI), the
//! native `adapter` that speaks the platform's protocol, and a per-instance
//! **settings schema** the workshop renders into a form. Types are seeded from
//! the binary into `<data>/gateway_channels/<id>/channel_type.json` (see
//! [`crate::seed`]). WhatsApp (Twilio) is the only built-in type today; Discord,
//! Slack, etc. follow the same template.
//!
//! A **channel instance** is a user-created, named, enable-able configuration of
//! a type (e.g. a specific WhatsApp number routed to a chosen persona). Multiple
//! instances of one type are allowed. Instances are persisted as a JSON array at
//! `<data>/gateway_channels.json`:
//! ```json
//! [ { "id": "uuid", "type_id": "pipestreamr", "name": "Support line",
//!     "enabled": true, "settings": { "from": "+1555…", "persona": "orchestrator-agent" } } ]
//! ```
//!
//! Account-level secrets (`TWILIO_ACCOUNT_SID`, `TWILIO_AUTH_TOKEN`) live in the
//! shared key store, never here — `settings` holds only non-secret per-instance
//! values. Inbound webhooks and outbound sends flow through the daemon's
//! workshop API (`/webhook/twilio`, the native `whatsapp_send_message` tool).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::paths;

/// Manifest for a channel type — what `channel_type.json` contains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelType {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    /// Native protocol adapter that handles inbound parsing + outbound sending
    /// for this type (e.g. `"twilio"`). Selects code, not config.
    pub adapter: String,
    /// API keys this type recommends — surfaced in the key-store UI once any
    /// instance of the type is enabled.
    #[serde(default)]
    pub requires_env: Vec<String>,
    /// Per-instance configuration fields the workshop renders into a form.
    #[serde(default)]
    pub settings: Vec<SettingField>,
    /// When set, this type is provisioned by a named "connect" flow rather than a
    /// manual settings form — the workshop renders that provider's Connect panel
    /// (e.g. `"metalcraft-gateway"` auto-syncs config from the gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioner: Option<String>,
}

/// One configurable field in a channel type's per-instance settings schema.
/// `input_type` is a hint for the workshop form (`text`, `tel`, `password`,
/// `number`, `persona`, `model`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    #[serde(default = "default_input_type")]
    pub input_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

fn default_input_type() -> String {
    "text".to_string()
}

/// A user-created configuration of a [`ChannelType`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInstance {
    pub id: String,
    pub type_id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub settings: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

impl ChannelInstance {
    /// Convenience accessor for a setting value, trimmed and non-empty.
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
    }
}

// ── Channel types (seeded manifests) ────────────────────────────────────

/// Read every `channel_type.json` under `<data>/gateway_channels/*/`. Malformed
/// manifests are logged and skipped. Sorted by id for deterministic output.
pub fn list_types() -> Vec<ChannelType> {
    let root = paths::gateway_channels_dir();
    let entries = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("channel_type.json");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        match serde_json::from_str::<ChannelType>(&content) {
            Ok(t) => out.push(t),
            Err(e) => log::warn!("invalid channel_type.json in {}: {e}", path.display()),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

pub fn find_type(id: &str) -> Option<ChannelType> {
    list_types().into_iter().find(|t| t.id == id)
}

// ── Channel instances (persisted state) ─────────────────────────────────

/// Read the on-disk instance array, defaulting to empty. A malformed file is
/// logged and treated as empty (never bricks the daemon).
pub fn load_instances() -> Vec<ChannelInstance> {
    let path = paths::gateway_channels_state_file();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    if content.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(&content).unwrap_or_else(|e| {
        log::warn!("gateway_channels.json is malformed, ignoring: {e}");
        Vec::new()
    })
}

fn save_instances(instances: &[ChannelInstance]) -> std::io::Result<()> {
    let path = paths::gateway_channels_state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(instances).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

pub fn get_instance(id: &str) -> Option<ChannelInstance> {
    load_instances().into_iter().find(|c| c.id == id)
}

/// Instances whose `enabled` flag is set.
pub fn enabled_instances() -> Vec<ChannelInstance> {
    load_instances().into_iter().filter(|c| c.enabled).collect()
}

/// Create a new instance of `type_id`. Validates the type exists and that all
/// `required` settings are present. Returns the stored instance (with a fresh
/// id + timestamp).
pub fn create_instance(
    type_id: &str,
    name: &str,
    settings: HashMap<String, String>,
) -> Result<ChannelInstance, String> {
    let ty = find_type(type_id).ok_or_else(|| format!("unknown channel type '{type_id}'"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err("channel name must not be empty".into());
    }
    validate_required(&ty, &settings)?;
    let instance = ChannelInstance {
        id: uuid::Uuid::new_v4().to_string(),
        type_id: type_id.to_string(),
        name: name.to_string(),
        enabled: false,
        settings,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    let mut all = load_instances();
    all.push(instance.clone());
    save_instances(&all).map_err(|e| format!("failed to write state: {e}"))?;
    Ok(instance)
}

/// Update an existing instance's name, enabled flag, and settings.
pub fn update_instance(
    id: &str,
    name: &str,
    enabled: bool,
    settings: HashMap<String, String>,
) -> Result<ChannelInstance, String> {
    let mut all = load_instances();
    let idx = all
        .iter()
        .position(|c| c.id == id)
        .ok_or_else(|| format!("channel '{id}' not found"))?;
    let ty = find_type(&all[idx].type_id)
        .ok_or_else(|| format!("unknown channel type '{}'", all[idx].type_id))?;
    let name = name.trim();
    if name.is_empty() {
        return Err("channel name must not be empty".into());
    }
    if enabled {
        validate_required(&ty, &settings)?;
    }
    all[idx].name = name.to_string();
    all[idx].enabled = enabled;
    all[idx].settings = settings;
    save_instances(&all).map_err(|e| format!("failed to write state: {e}"))?;
    Ok(all[idx].clone())
}

/// Flip the enabled flag for one instance.
pub fn set_enabled(id: &str, enabled: bool) -> Result<(), String> {
    let mut all = load_instances();
    let idx = all
        .iter()
        .position(|c| c.id == id)
        .ok_or_else(|| format!("channel '{id}' not found"))?;
    if enabled {
        let ty = find_type(&all[idx].type_id)
            .ok_or_else(|| format!("unknown channel type '{}'", all[idx].type_id))?;
        validate_required(&ty, &all[idx].settings)?;
    }
    all[idx].enabled = enabled;
    save_instances(&all).map_err(|e| format!("failed to write state: {e}"))
}

/// Delete an instance. Returns `true` if it existed.
pub fn delete_instance(id: &str) -> Result<bool, String> {
    let mut all = load_instances();
    let before = all.len();
    all.retain(|c| c.id != id);
    let removed = all.len() != before;
    if removed {
        save_instances(&all).map_err(|e| format!("failed to write state: {e}"))?;
    }
    Ok(removed)
}

fn validate_required(ty: &ChannelType, settings: &HashMap<String, String>) -> Result<(), String> {
    for field in &ty.settings {
        if field.required {
            let present = settings.get(&field.key).map(|v| !v.trim().is_empty()).unwrap_or(false);
            if !present {
                return Err(format!("missing required setting '{}'", field.label));
            }
        }
    }
    Ok(())
}

// ── Cross-cutting helpers ────────────────────────────────────────────────

/// Env keys recommended by the channel types that have at least one *enabled*
/// instance, each mapped to the sorted list of type names that declare it. The
/// workshop merges this with the integration-pack recommendations so a
/// channel's secrets (e.g. `TWILIO_*`) appear in the key-store UI as soon as one
/// of its instances is enabled. Returned in sorted key order.
pub fn recommended_env() -> Vec<(String, Vec<String>)> {
    let enabled_type_ids: std::collections::HashSet<String> =
        enabled_instances().into_iter().map(|c| c.type_id).collect();
    let mut map: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for ty in list_types() {
        if !enabled_type_ids.contains(&ty.id) {
            continue;
        }
        for key in &ty.requires_env {
            let sources = map.entry(key.clone()).or_default();
            if !sources.contains(&ty.name) {
                sources.push(ty.name.clone());
            }
        }
    }
    map.into_iter().collect()
}

/// Find the enabled instance configured with a given setting value (exact,
/// trimmed match). The stable way to route inbound messages: e.g. match a
/// PipeStreamr webhook's `source_id` against each channel's `integration_id`
/// setting. Unique per integration, unlike phone-number matching.
pub fn resolve_by_setting(key: &str, value: &str) -> Option<ChannelInstance> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    enabled_instances()
        .into_iter()
        .find(|c| c.setting(key) == Some(value))
}

/// Find the enabled instance whose `from` number matches an inbound message's
/// destination (`To`). Numbers are compared after stripping any `whatsapp:`
/// prefix and non-digit characters, so `whatsapp:+1 (555) 000` matches
/// `+15550000`. Used by the (dormant) Twilio webhook to route an incoming
/// message to the right channel (and thus persona/model).
pub fn resolve_by_inbound_to(to_number: &str) -> Option<ChannelInstance> {
    let target = normalize_number(to_number);
    enabled_instances()
        .into_iter()
        .find(|c| c.setting("from").map(normalize_number).as_deref() == Some(target.as_str()))
}

/// Reduce a phone number to bare digits (dropping `whatsapp:`, `+`, spaces,
/// punctuation) for tolerant matching.
pub fn normalize_number(raw: &str) -> String {
    raw.trim_start_matches("whatsapp:")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_prefix_and_punctuation() {
        assert_eq!(normalize_number("whatsapp:+1 (555) 000-1234"), "15550001234");
        assert_eq!(normalize_number("+15550001234"), "15550001234");
    }

    #[test]
    fn validate_required_reports_missing_label() {
        let ty = ChannelType {
            id: "x".into(),
            name: "X".into(),
            description: String::new(),
            version: "1.0.0".into(),
            adapter: "twilio".into(),
            requires_env: vec![],
            settings: vec![SettingField {
                key: "from".into(),
                label: "From number".into(),
                input_type: "tel".into(),
                required: true,
                placeholder: None,
                help: None,
            }],
        };
        let err = validate_required(&ty, &HashMap::new()).unwrap_err();
        assert!(err.contains("From number"), "got {err}");
        let mut ok = HashMap::new();
        ok.insert("from".to_string(), "+15550001234".to_string());
        assert!(validate_required(&ty, &ok).is_ok());
    }
}
