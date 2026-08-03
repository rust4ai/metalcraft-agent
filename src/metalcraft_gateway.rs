//! Zero-copy "connect" to the **Metalcraft Gateway** (gateway.metalcraftai.com).
//!
//! The pod already holds the user's `METALCRAFT_TOKEN` (an `mck_…` Metalcraft ID PAT,
//! injected by the k3s control plane). Because the gateway authenticates that same
//! token, the agent can *fetch* everything it needs instead of the user pasting it:
//! `POST {gateway}/api/v1/agent/connect` returns the base URL, integration id, webhook
//! secret, and active number, and registers the agent's inbound webhook — then we
//! write the `PIPESTREAMR_*` keys and enable a `metalcraft-gateway` channel instance.
//! The message path itself reuses the existing `pipestreamr` adapter unchanged.
use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const DEFAULT_GATEWAY: &str = "https://gateway.metalcraftai.com";
const CHANNEL_TYPE: &str = "metalcraft-gateway";

/// The gateway base URL — override with the `METALCRAFT_GATEWAY_URL` key.
fn gateway_url() -> String {
    crate::key_store::lookup("METALCRAFT_GATEWAY_URL")
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GATEWAY.to_string())
}

/// The pod's Metalcraft ID token (injected as `METALCRAFT_TOKEN`).
fn token() -> Result<String, String> {
    crate::key_store::lookup("METALCRAFT_TOKEN")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "METALCRAFT_TOKEN is not set — this pod isn't linked to a Metalcraft ID account".to_string())
}

/// The pod's public base URL for its inbound webhook: an explicit override, else the
/// injected `POD_PUBLIC_URL`.
fn webhook_base(explicit: Option<String>) -> Option<String> {
    explicit
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("POD_PUBLIC_URL").ok().filter(|s| !s.trim().is_empty()))
        .map(|s| s.trim().trim_end_matches('/').to_string())
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("metalcraft-agent (gateway-connect)")
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

#[derive(Deserialize)]
struct ConnectResp {
    base_url: String,
    integration_id: String,
    signing_secret: String,
    active_number: String,
    channel: String,
}

#[derive(Serialize)]
pub struct ConnectResult {
    pub connected: bool,
    pub active_number: String,
    pub integration_id: String,
    pub channel: String,
}

/// Sentinel error meaning the user hasn't verified their number yet — the caller maps
/// it to a "register + verify first" prompt.
pub const VERIFY_REQUIRED: &str = "verify_required";

/// Fetch config with the pod's token, register the agent's webhook, write the
/// `PIPESTREAMR_*` keys, and create/enable the `metalcraft-gateway` channel. Idempotent
/// — safe to re-run to re-sync after a rotated secret or a reassigned number.
pub async fn connect(explicit_webhook_base: Option<String>) -> Result<ConnectResult, String> {
    let gw = gateway_url();
    let tok = token()?;
    let base = webhook_base(explicit_webhook_base)
        .ok_or_else(|| "no POD_PUBLIC_URL set and no webhook_base provided".to_string())?;
    let webhook_url = format!("{base}/webhook/pipestreamr");

    let resp = client()?
        .post(format!("{gw}/api/v1/agent/connect"))
        .bearer_auth(&tok)
        .json(&serde_json::json!({ "webhook_url": webhook_url }))
        .send()
        .await
        .map_err(|e| format!("gateway connect request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::CONFLICT {
        return Err(VERIFY_REQUIRED.to_string());
    }
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("gateway connect failed (HTTP {}): {msg}", status.as_u16()));
    }
    let cfg: ConnectResp =
        serde_json::from_str(&text).map_err(|e| format!("failed to parse connect response: {e}"))?;

    // 1. Write the PipeStreamr keys (the API key is the same PAT).
    let path = crate::paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    store.upsert("PIPESTREAMR_BASE_URL", &cfg.base_url);
    store.upsert("PIPESTREAMR_API_KEY", &tok);
    store.upsert("PIPESTREAMR_WEBHOOK_SECRET", &cfg.signing_secret);
    store.save(&path).map_err(|e| format!("failed to write keys: {e}"))?;

    // 2. Create or update the channel instance (preserving persona/model), enabled.
    let existing = crate::gateway_channels::load_instances()
        .into_iter()
        .find(|i| i.type_id == CHANNEL_TYPE);
    match existing {
        Some(inst) => {
            let mut s = inst.settings.clone();
            s.insert("integration_id".into(), cfg.integration_id.clone());
            s.insert("from".into(), cfg.active_number.clone());
            crate::gateway_channels::update_instance(&inst.id, &inst.name, true, s)?;
        }
        None => {
            let mut s = HashMap::new();
            s.insert("integration_id".to_string(), cfg.integration_id.clone());
            s.insert("from".to_string(), cfg.active_number.clone());
            let inst = crate::gateway_channels::create_instance(CHANNEL_TYPE, "Metalcraft Gateway", s)?;
            crate::gateway_channels::update_instance(&inst.id, &inst.name, true, inst.settings)?;
        }
    }

    Ok(ConnectResult {
        connected: true,
        active_number: cfg.active_number,
        integration_id: cfg.integration_id,
        channel: cfg.channel,
    })
}

#[derive(Deserialize)]
struct PhoneResp {
    #[serde(default)]
    personal_number: Option<String>,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    active_number: Option<String>,
    #[serde(default)]
    channel: Option<String>,
}

/// What the workshop Connect panel renders from.
#[derive(Serialize, Default)]
pub struct GatewayStatus {
    /// The pod has a token (is linked to a Metalcraft ID account).
    pub configured: bool,
    /// The user has registered a personal number on the gateway.
    pub registered: bool,
    /// That number is verified — required before connecting.
    pub verified: bool,
    /// A local `metalcraft-gateway` channel is enabled and the webhook secret is set.
    pub connected: bool,
    pub active_number: Option<String>,
    pub channel: Option<String>,
    /// Whether the pod knows its own public URL (POD_PUBLIC_URL) for the webhook.
    pub has_public_url: bool,
    pub error: Option<String>,
}

/// Report registration/verification/connection state for the workshop.
pub async fn status() -> GatewayStatus {
    let connected = crate::gateway_channels::load_instances()
        .iter()
        .any(|i| i.type_id == CHANNEL_TYPE && i.enabled)
        && crate::key_store::lookup("PIPESTREAMR_WEBHOOK_SECRET")
            .filter(|s| !s.is_empty())
            .is_some();
    let has_public_url = webhook_base(None).is_some();

    let tok = match token() {
        Ok(t) => t,
        Err(e) => {
            return GatewayStatus { connected, has_public_url, error: Some(e), ..Default::default() };
        }
    };

    let resp = match client() {
        Ok(c) => c.get(format!("{}/api/v1/phone", gateway_url())).bearer_auth(&tok).send().await,
        Err(e) => return GatewayStatus { configured: true, connected, has_public_url, error: Some(e), ..Default::default() },
    };
    match resp {
        Ok(r) if r.status().is_success() => match r.json::<PhoneResp>().await {
            Ok(p) => GatewayStatus {
                configured: true,
                registered: p.personal_number.is_some(),
                verified: p.verified,
                connected,
                active_number: p.active_number,
                channel: p.channel,
                has_public_url,
                error: None,
            },
            Err(e) => GatewayStatus { configured: true, connected, has_public_url, error: Some(format!("parse /phone: {e}")), ..Default::default() },
        },
        Ok(r) => GatewayStatus { configured: true, connected, has_public_url, error: Some(format!("gateway /phone returned HTTP {}", r.status().as_u16())), ..Default::default() },
        Err(e) => GatewayStatus { configured: true, connected, has_public_url, error: Some(format!("gateway unreachable: {e}")), ..Default::default() },
    }
}

/// Proxy a phone registration to the gateway with the pod's token (for the workshop's
/// inline register → verify flow). Returns the gateway's JSON (incl. `verify_code`).
pub async fn register(phone_number: &str) -> Result<serde_json::Value, String> {
    let gw = gateway_url();
    let tok = token()?;
    let resp = client()?
        .post(format!("{gw}/api/v1/phone/register"))
        .bearer_auth(&tok)
        .json(&serde_json::json!({ "phone_number": phone_number }))
        .send()
        .await
        .map_err(|e| format!("gateway register request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("register failed (HTTP {}): {msg}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|e| format!("failed to parse register response: {e}"))
}

// ── Self-heal (Phase 3) ──────────────────────────────────────────────────────

/// Re-sync the connection so a **rotated webhook secret** or a **reassigned number**
/// heals itself. No-op (`Ok`) unless an enabled `metalcraft-gateway` channel exists.
/// When `POD_PUBLIC_URL` is known it does a full [`connect`] (also re-registering the
/// webhook); otherwise it refreshes the secret + `integration_id`/`from` from
/// `GET /api/v1/phone` without touching the webhook.
pub async fn resync() -> Result<(), String> {
    let Some(inst) = crate::gateway_channels::load_instances()
        .into_iter()
        .find(|i| i.type_id == CHANNEL_TYPE && i.enabled)
    else {
        return Ok(()); // nothing connected — stay quiet
    };

    if webhook_base(None).is_some() {
        return match connect(None).await {
            Ok(_) => Ok(()),
            // A membership lapse mid-life shouldn't spam errors.
            Err(e) if e == VERIFY_REQUIRED => Ok(()),
            Err(e) => Err(e),
        };
    }

    // No public URL: refresh secret + routing from /phone.
    #[derive(Deserialize)]
    struct P {
        #[serde(default)]
        verified: bool,
        #[serde(default)]
        active_number: Option<String>,
        #[serde(default)]
        integration_id: Option<String>,
        #[serde(default)]
        signing_secret: Option<String>,
    }
    let gw = gateway_url();
    let tok = token()?;
    let resp = client()?
        .get(format!("{gw}/api/v1/phone"))
        .bearer_auth(&tok)
        .send()
        .await
        .map_err(|e| format!("phone fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("gateway /phone returned HTTP {}", resp.status().as_u16()));
    }
    let p: P = resp.json().await.map_err(|e| format!("parse /phone: {e}"))?;
    if !p.verified {
        return Ok(()); // no longer verified — leave config as-is
    }
    if let Some(secret) = p.signing_secret.filter(|s| !s.is_empty()) {
        let path = crate::paths::keys_file();
        let mut store = crate::key_store::KeyStore::load(&path);
        if store.get("PIPESTREAMR_WEBHOOK_SECRET") != Some(secret.as_str()) {
            store.upsert("PIPESTREAMR_WEBHOOK_SECRET", &secret);
            store.save(&path).map_err(|e| format!("write keys: {e}"))?;
        }
    }
    if let (Some(iid), Some(from)) = (p.integration_id, p.active_number) {
        let drifted =
            inst.settings.get("integration_id") != Some(&iid) || inst.settings.get("from") != Some(&from);
        if drifted {
            let mut s = inst.settings.clone();
            s.insert("integration_id".into(), iid);
            s.insert("from".into(), from);
            crate::gateway_channels::update_instance(&inst.id, &inst.name, true, s)?;
        }
    }
    Ok(())
}

/// Periodic self-heal: re-sync every `METALCRAFT_GATEWAY_HEAL_SECS` (default 600).
/// Spawned by the daemon; no-op while nothing is connected.
pub async fn heal_loop() {
    let secs = std::env::var("METALCRAFT_GATEWAY_HEAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(600);
    let interval = std::time::Duration::from_secs(secs);
    loop {
        tokio::time::sleep(interval).await;
        if let Err(e) = resync().await {
            log::warn!("metalcraft-gateway heal: {e}");
        }
    }
}

fn reactive_gate() -> &'static std::sync::Mutex<Option<std::time::Instant>> {
    static G: std::sync::OnceLock<std::sync::Mutex<Option<std::time::Instant>>> =
        std::sync::OnceLock::new();
    G.get_or_init(|| std::sync::Mutex::new(None))
}

/// Fire-and-forget reactive heal: call this when an inbound webhook signature is
/// rejected (a rotated secret is the usual cause). Rate-limited to once / 30s.
pub fn maybe_reactive_resync() {
    const MIN_GAP: std::time::Duration = std::time::Duration::from_secs(30);
    {
        let mut last = reactive_gate().lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        if let Some(t) = *last
            && now.duration_since(t) < MIN_GAP
        {
            return;
        }
        *last = Some(now);
    }
    tokio::spawn(async move {
        match resync().await {
            Ok(()) => log::info!("metalcraft-gateway: reactive re-sync after signature rejection"),
            Err(e) => log::warn!("metalcraft-gateway reactive re-sync failed: {e}"),
        }
    });
}
