//! Channels — a **channel is a named connection to a messaging gateway**:
//! `{ slug, name, url, secret }`. Outbound sends resolve a channel by slug and
//! POST to `{url}/api/v1/messages/send` with the channel's secret as the bearer.
//!
//! There is always a default channel, slug **`metalcraft`**, pointing at the
//! first-party gateway (`gateway.metalcraftai.com`, overridable via
//! `METALCRAFT_GATEWAY_URL`) and authenticated with the pod's `METALCRAFT_TOKEN`.
//! Because the token is injected by the control plane, this channel needs no
//! setup — any linked pod can send. Its secret is a *live reference* to
//! `METALCRAFT_TOKEN` (never copied), so it survives token rotation; the channel
//! is `managed` and cannot be edited or deleted.
//!
//! Users may add **custom channels** — their own gateway URL + secret, selected
//! by passing `channel: "<slug>"` to `gateway_send_message`. A custom channel's
//! secret lives in the scoped key store (`lookup_scoped(slug, "SECRET")`), never
//! in `channels.json`, which holds only `{ slug, name, url, enabled }`.
//!
//! This is the outbound connection concept only. It deliberately carries none of
//! the old channel-*type* / adapter / provisioner machinery: a channel is just a
//! URL and a secret. Delivery *kind* (push vs text) is a separate axis chosen at
//! send time, not a property of the channel.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::paths;

/// Slug of the always-present, managed first-party channel.
pub const DEFAULT_SLUG: &str = "metalcraft";
const DEFAULT_GATEWAY_URL: &str = "https://gateway.metalcraftai.com";
const SECRET_KEY: &str = "SECRET";
const WEBHOOK_SECRET_KEY: &str = "WEBHOOK_SECRET";
const SEND_TIMEOUT_SECS: u64 = 30;

/// A channel as surfaced to callers/UI: a named connection. `managed` marks the
/// built-in `metalcraft` channel (secret is the pod token; not user-editable).
/// The secret value itself is never included here.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Channel {
    pub slug: String,
    pub name: String,
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// True only for the built-in `metalcraft` channel.
    #[serde(default)]
    pub managed: bool,
    // ── Link fields — set when the channel is connected to a gateway
    // integration (used for inbound routing + replies). Absent until connected.
    /// The gateway integration UUID this channel is bound to. Inbound messages
    /// carrying this `source_id` route here; also the send-time sender selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
    /// Persona that answers inbound on this channel (default: orchestrator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Model override for inbound runs on this channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The gateway number bound to this channel, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_number: Option<String>,
    /// Whether the channel has completed a gateway connect (link fields present).
    #[serde(default)]
    pub connected: bool,
}

/// On-disk record. Custom channels store the full record; the managed
/// `metalcraft` channel is synthesized but may still have a stored entry that
/// carries only its *link* fields (written on connect). Secrets are kept out of
/// this file (scoped key store).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredChannel {
    slug: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    integration_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    persona: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_number: Option<String>,
    #[serde(default)]
    connected: bool,
}

fn default_true() -> bool {
    true
}

/// serde default for an update request's `enabled` flag (keeps a channel enabled
/// when the field is omitted).
pub fn default_enabled() -> bool {
    true
}

/// A channel resolved to what a send actually needs: where to POST and the
/// bearer secret to use.
pub struct ResolvedChannel {
    pub slug: String,
    pub url: String,
    pub secret: String,
}

// ── The managed default ──────────────────────────────────────────────────

/// The first-party gateway URL: `METALCRAFT_GATEWAY_URL` if set, else the hosted
/// default. Trailing slash trimmed.
fn metalcraft_url() -> String {
    std::env::var("METALCRAFT_GATEWAY_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string())
}

/// The synthesized `metalcraft` channel record (always present), before any
/// stored link fields are overlaid.
fn metalcraft_channel() -> Channel {
    Channel {
        slug: DEFAULT_SLUG.to_string(),
        name: "Metalcraft Gateway".to_string(),
        url: metalcraft_url(),
        enabled: true,
        managed: true,
        integration_id: None,
        persona: None,
        model: None,
        active_number: None,
        connected: false,
    }
}

// ── Persistence (custom channels) ────────────────────────────────────────

fn load_stored() -> Vec<StoredChannel> {
    let path = paths::channels_state_file();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    if content.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(&content).unwrap_or_else(|e| {
        log::warn!("channels.json is malformed, ignoring: {e}");
        Vec::new()
    })
}

fn save_stored(channels: &[StoredChannel]) -> std::io::Result<()> {
    let path = paths::channels_state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(channels).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

// ── Listing / lookup ─────────────────────────────────────────────────────

/// All channels: the managed `metalcraft` default first (with any stored link
/// fields overlaid), then custom channels in stored order.
pub fn list_channels() -> Vec<Channel> {
    let stored = load_stored();
    let mut mc = metalcraft_channel();
    if let Some(s) = stored.iter().find(|s| s.slug == DEFAULT_SLUG) {
        mc.integration_id = s.integration_id.clone();
        mc.persona = s.persona.clone();
        mc.model = s.model.clone();
        mc.active_number = s.active_number.clone();
        mc.connected = s.connected;
    }
    let mut out = vec![mc];
    out.extend(stored.into_iter().filter(|s| s.slug != DEFAULT_SLUG).map(|s| Channel {
        slug: s.slug,
        name: s.name,
        url: s.url,
        enabled: s.enabled,
        managed: false,
        integration_id: s.integration_id,
        persona: s.persona,
        model: s.model,
        active_number: s.active_number,
        connected: s.connected,
    }));
    out
}

/// Find the channel bound to a gateway integration id — the inbound routing key
/// (an inbound message's `source_id` is matched against each channel's
/// `integration_id`).
pub fn resolve_by_integration(integration_id: &str) -> Option<Channel> {
    let id = integration_id.trim();
    if id.is_empty() {
        return None;
    }
    list_channels().into_iter().find(|c| c.integration_id.as_deref() == Some(id))
}

/// The inbound HMAC secret for a channel (scoped `WEBHOOK_SECRET`), or `None`.
pub fn webhook_secret(slug: &str) -> Option<String> {
    crate::key_store::lookup_scoped(Some(slug), WEBHOOK_SECRET_KEY).filter(|s| !s.is_empty())
}

/// Store a channel's inbound HMAC secret (scoped).
pub fn set_webhook_secret(slug: &str, secret: &str) -> Result<(), String> {
    let path = paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    store.upsert_channel(slug, WEBHOOK_SECRET_KEY, secret);
    store.save(&path).map_err(|e| format!("failed to store webhook secret: {e}"))
}

/// Gateway link fields written on connect.
#[derive(Debug, Clone, Default)]
pub struct Link {
    pub integration_id: Option<String>,
    pub persona: Option<String>,
    pub model: Option<String>,
    pub active_number: Option<String>,
}

/// Bind a channel to a gateway integration (upsert its link fields), marking it
/// connected. Creates the stored entry if absent — e.g. the synthesized
/// `metalcraft` channel's first connect writes a link-only record.
pub fn set_link(slug: &str, link: Link) -> Result<(), String> {
    let mut stored = load_stored();
    match stored.iter_mut().find(|s| s.slug == slug) {
        Some(s) => {
            s.integration_id = link.integration_id;
            s.persona = link.persona;
            s.model = link.model;
            s.active_number = link.active_number;
            s.connected = true;
        }
        None => stored.push(StoredChannel {
            slug: slug.to_string(),
            enabled: true,
            integration_id: link.integration_id,
            persona: link.persona,
            model: link.model,
            active_number: link.active_number,
            connected: true,
            ..Default::default()
        }),
    }
    save_stored(&stored).map_err(|e| format!("failed to write channels: {e}"))
}

/// Clear a channel's gateway link (disconnect) and drop its inbound secret. A
/// managed link-only entry is removed; a custom channel keeps its record.
pub fn clear_link(slug: &str) -> Result<(), String> {
    let mut stored = load_stored();
    if slug == DEFAULT_SLUG {
        stored.retain(|s| s.slug != DEFAULT_SLUG);
    } else if let Some(s) = stored.iter_mut().find(|s| s.slug == slug) {
        s.integration_id = None;
        s.persona = None;
        s.model = None;
        s.active_number = None;
        s.connected = false;
    }
    save_stored(&stored).map_err(|e| format!("failed to write channels: {e}"))?;
    let path = paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    if store.delete_channel_key(slug, WEBHOOK_SECRET_KEY) {
        let _ = store.save(&path);
    }
    Ok(())
}

/// A channel's public record by slug, or `None`.
pub fn get_channel(slug: &str) -> Option<Channel> {
    list_channels().into_iter().find(|c| c.slug == slug)
}

/// Reduce a name to a URL-safe slug: lowercase, non-alphanumerics → `-`, trimmed.
pub fn slugify(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in raw.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

// ── Resolution ───────────────────────────────────────────────────────────

/// Resolve a channel (by slug; `None`/empty → the default `metalcraft`) to its
/// send target: `{ url, secret }`. Errors clearly when a slug is unknown, a
/// custom channel is disabled or missing its secret, or the pod isn't linked
/// (no `METALCRAFT_TOKEN`) for the default channel.
pub fn resolve_channel(slug: Option<&str>) -> Result<ResolvedChannel, String> {
    let slug = slug.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_SLUG);

    if slug == DEFAULT_SLUG {
        // Prefer an adopted audience-scoped token (written at connect), falling
        // back to the pod's broad METALCRAFT_TOKEN.
        let secret = crate::key_store::lookup_scoped(Some(DEFAULT_SLUG), SECRET_KEY)
            .filter(|s| !s.is_empty())
            .or_else(|| crate::key_store::lookup("METALCRAFT_TOKEN").filter(|s| !s.is_empty()))
            .ok_or("METALCRAFT_TOKEN is not set — this pod isn't linked to a Metalcraft ID account")?;
        return Ok(ResolvedChannel { slug: DEFAULT_SLUG.to_string(), url: metalcraft_url(), secret });
    }

    let ch = get_channel(slug).ok_or_else(|| format!("no channel with slug '{slug}'"))?;
    if !ch.enabled {
        return Err(format!("channel '{slug}' is disabled"));
    }
    let secret = crate::key_store::lookup_scoped(Some(slug), SECRET_KEY)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("channel '{slug}' has no secret configured"))?;
    Ok(ResolvedChannel { slug: ch.slug, url: ch.url, secret })
}

/// Resolve a legacy gateway *channel instance* (the inbound registration model
/// in [`crate::gateway_channels`]) to a send target. Used by the inbound-reply
/// sink so a reply goes back out through the same gateway the message arrived
/// on. A `metalcraft-gateway`-provisioned instance uses the pod's
/// `METALCRAFT_TOKEN` (or its adopted scoped key); a custom instance uses its
/// scoped `API_KEY`. The URL is the instance's stored `BASE_URL`, defaulting to
/// the first-party gateway.
pub fn resolve_instance(
    inst: &crate::gateway_channels::ChannelInstance,
) -> Result<ResolvedChannel, String> {
    let provisioner =
        crate::gateway_channels::find_type(&inst.type_id).and_then(|t| t.provisioner);
    let secret = if provisioner.as_deref() == Some("metalcraft-gateway") {
        crate::key_store::lookup_scoped(Some(&inst.id), "API_KEY")
            .filter(|s| !s.is_empty())
            .or_else(|| crate::key_store::lookup("METALCRAFT_TOKEN").filter(|s| !s.is_empty()))
            .ok_or("METALCRAFT_TOKEN is not set — this pod isn't linked to a Metalcraft ID account")?
    } else {
        crate::key_store::lookup_scoped(Some(&inst.id), "API_KEY")
            .filter(|s| !s.is_empty())
            .ok_or("no API key configured for this channel")?
    };
    let url = crate::key_store::lookup_scoped(Some(&inst.id), "BASE_URL")
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(metalcraft_url);
    Ok(ResolvedChannel { slug: inst.id.clone(), url, secret })
}

// ── CRUD for custom channels ─────────────────────────────────────────────

/// Add a custom channel and store its secret in the scoped key store. The slug
/// is derived from `name` (or an explicit `slug`), must be unique, and cannot be
/// the reserved `metalcraft`.
pub fn create_channel(
    name: &str,
    url: &str,
    secret: &str,
    slug: Option<&str>,
) -> Result<Channel, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("channel name must not be empty".into());
    }
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err("channel url must not be empty".into());
    }
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("channel secret must not be empty".into());
    }
    let slug = slug.map(slugify).filter(|s| !s.is_empty()).unwrap_or_else(|| slugify(name));
    if slug.is_empty() {
        return Err("could not derive a slug from the channel name".into());
    }
    if slug == DEFAULT_SLUG {
        return Err(format!("'{DEFAULT_SLUG}' is reserved for the built-in channel"));
    }
    let mut stored = load_stored();
    if stored.iter().any(|c| c.slug == slug) {
        return Err(format!("a channel with slug '{slug}' already exists"));
    }
    stored.push(StoredChannel {
        slug: slug.clone(),
        name: name.to_string(),
        url: url.to_string(),
        enabled: true,
        ..Default::default()
    });
    save_stored(&stored).map_err(|e| format!("failed to write channels: {e}"))?;
    set_secret(&slug, secret)?;
    Ok(Channel {
        slug,
        name: name.to_string(),
        url: url.to_string(),
        enabled: true,
        managed: false,
        integration_id: None,
        persona: None,
        model: None,
        active_number: None,
        connected: false,
    })
}

/// Update a custom channel's name/url/enabled, and its secret when a non-empty
/// one is provided. The managed `metalcraft` channel cannot be edited.
pub fn update_channel(
    slug: &str,
    name: &str,
    url: &str,
    enabled: bool,
    secret: Option<&str>,
) -> Result<Channel, String> {
    if slug == DEFAULT_SLUG {
        return Err("the built-in 'metalcraft' channel cannot be edited".into());
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("channel name must not be empty".into());
    }
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err("channel url must not be empty".into());
    }
    let mut stored = load_stored();
    let idx = stored
        .iter()
        .position(|c| c.slug == slug)
        .ok_or_else(|| format!("no channel with slug '{slug}'"))?;
    stored[idx].name = name.to_string();
    stored[idx].url = url.to_string();
    stored[idx].enabled = enabled;
    let updated = stored[idx].clone();
    save_stored(&stored).map_err(|e| format!("failed to write channels: {e}"))?;
    if let Some(s) = secret.map(str::trim).filter(|s| !s.is_empty()) {
        set_secret(slug, s)?;
    }
    Ok(Channel {
        slug: slug.to_string(),
        name: name.to_string(),
        url: url.to_string(),
        enabled,
        managed: false,
        integration_id: updated.integration_id,
        persona: updated.persona,
        model: updated.model,
        active_number: updated.active_number,
        connected: updated.connected,
    })
}

/// Delete a custom channel and its scoped secret. Returns `true` if it existed.
/// The managed `metalcraft` channel cannot be deleted.
pub fn delete_channel(slug: &str) -> Result<bool, String> {
    if slug == DEFAULT_SLUG {
        return Err("the built-in 'metalcraft' channel cannot be deleted".into());
    }
    let mut stored = load_stored();
    let before = stored.len();
    stored.retain(|c| c.slug != slug);
    let removed = stored.len() != before;
    if removed {
        save_stored(&stored).map_err(|e| format!("failed to write channels: {e}"))?;
        let path = paths::keys_file();
        let mut store = crate::key_store::KeyStore::load(&path);
        if store.delete_channel(slug) {
            if let Err(e) = store.save(&path) {
                log::warn!("deleted channel '{slug}' but failed to prune its secret: {e}");
            }
        }
    }
    Ok(removed)
}

/// Store a channel's outbound bearer secret (scoped). Used by custom-channel
/// CRUD and by the gateway connect to adopt an audience-scoped token.
pub fn set_secret(slug: &str, secret: &str) -> Result<(), String> {
    let path = paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    store.upsert_channel(slug, SECRET_KEY, secret);
    store.save(&path).map_err(|e| format!("failed to store channel secret: {e}"))
}

// ── Send ─────────────────────────────────────────────────────────────────

/// POST an outbound message to `channel`'s gateway
/// (`{url}/api/v1/messages/send`). `to` is the recipient (a phone number for
/// text; ignored for push, which fans out over the owner's devices). `kind`,
/// when set (e.g. `"apns"`), tells the gateway which *delivery kind* to route
/// through; `integration_id`, when a UUID, pins a specific sender integration.
/// Returns a small JSON receipt or an error string.
pub async fn send(
    channel: &ResolvedChannel,
    to: &str,
    content: &str,
    kind: Option<&str>,
    integration_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let url = format!("{}/api/v1/messages/send", channel.url);
    let mut body = serde_json::json!({ "to": to, "body": content });
    if let Some(iid) = integration_id.map(str::trim).filter(|s| looks_like_uuid(s)) {
        body["integration_id"] = serde_json::Value::String(iid.to_string());
    }
    if let Some(k) = kind.map(str::trim).filter(|s| !s.is_empty()) {
        body["platform"] = serde_json::Value::String(k.to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(SEND_TIMEOUT_SECS))
        .user_agent("metalcraft-agent (channels)")
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client
        .post(&url)
        .bearer_auth(&channel.secret)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request to gateway failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = extract_error(&text)
            .unwrap_or_else(|| crate::tools::truncate_output(text.trim(), 500));
        return Err(format!("gateway returned HTTP {} — {detail}", status.as_u16()));
    }

    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    Ok(serde_json::json!({
        "to": to,
        "sent": true,
        "id": parsed.get("id"),
        "status": parsed.get("status"),
    }))
}

/// Pull a human-readable message out of a gateway JSON error body.
fn extract_error(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("error")
        .or_else(|| v.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// Loose UUID shape check (8-4-4-4-12 hex) — a non-UUID `integration_id` is
/// dropped rather than forwarded (the gateway expects a UUID and would 400).
fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("My Gateway"), "my-gateway");
        assert_eq!(slugify("  Weird__Name!! "), "weird-name");
        assert_eq!(slugify("metalcraft"), "metalcraft");
    }

    #[test]
    fn looks_like_uuid_accepts_only_uuid_shape() {
        assert!(looks_like_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!looks_like_uuid("me"));
        assert!(!looks_like_uuid("+14155238886"));
    }
}
