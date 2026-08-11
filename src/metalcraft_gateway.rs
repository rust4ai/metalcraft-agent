//! Zero-copy "connect" to the **Metalcraft Gateway** (gateway.metalcraftai.com).
//!
//! The pod already holds the user's `METALCRAFT_TOKEN` (an `mck_…` Metalcraft ID PAT,
//! injected by the k3s control plane). Because the gateway authenticates that same
//! token, the agent can *fetch* everything it needs instead of the user pasting it:
//! `POST {gateway}/api/v1/agent/connect` returns the base URL, integration id, webhook
//! secret, and active number, and registers the agent's inbound webhook — then we
//! write the channel-scoped secrets (`BASE_URL`, `WEBHOOK_SECRET`) and enable a
//! built-in `metalcraft` channel. The API key is the adopted connection token
//! (or the pod's `METALCRAFT_TOKEN`), resolved at send time by
//! [`crate::channels`]; outbound and inbound both flow through the channel model.
use std::time::Duration;

use serde::{Deserialize, Serialize};

const DEFAULT_GATEWAY: &str = "https://gateway.metalcraftai.com";
const CHANNEL_TYPE: &str = "metalcraft-gateway";

/// Liveness for **Inbound Pull**: set by the long-poll client while it is connected and
/// draining. This is the honest "am I actually receiving?" signal surfaced on
/// [`GatewayStatus::streaming`], distinct from the on-disk `connected` config flag.
static STREAMING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set by the long-poll client on each successful/idle poll (true) and on error (false).
pub fn set_streaming(on: bool) {
    STREAMING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the Inbound Pull long-poll is currently connected.
pub fn is_streaming() -> bool {
    STREAMING.load(std::sync::atomic::Ordering::Relaxed)
}

/// The enabled `metalcraft-gateway` channel's pull target: `(gateway_base_url, bearer)`.
/// The bearer is the channel's adopted connection token (`API_KEY`) when present — else
/// the pod's broad `METALCRAFT_TOKEN`. `None` when no gateway channel is enabled (the
/// long-poll then stays idle). The base URL prefers the channel's stored `BASE_URL` (what
/// the gateway told us at connect) and falls back to the configured gateway origin.
pub fn pull_target() -> Option<(String, String)> {
    let ch = crate::channels::get_channel(crate::channels::DEFAULT_SLUG)?;
    if !ch.connected {
        return None;
    }
    let resolved = crate::channels::resolve_channel(Some(crate::channels::DEFAULT_SLUG)).ok()?;
    Some((resolved.url, resolved.secret))
}

/// One-time boot migration: if a legacy `metalcraft-gateway` channel *instance*
/// exists but the `metalcraft` channel isn't linked yet, mirror its link +
/// secrets into the channel model so the pull loop / status / inbound routing all
/// work from the channel before the first post-deploy resync runs.
pub fn migrate_instance_to_channel() {
    if crate::channels::get_channel(crate::channels::DEFAULT_SLUG).map(|c| c.connected).unwrap_or(false) {
        return; // already linked
    }
    // Read the legacy instance store directly (no dependency on the retiring
    // gateway_channels module). Its scoped secrets live under the instance id.
    #[derive(serde::Deserialize)]
    struct LegacyInstance {
        id: String,
        #[serde(default)]
        type_id: String,
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        settings: std::collections::HashMap<String, String>,
    }
    let raw = std::fs::read_to_string(crate::paths::gateway_channels_state_file()).unwrap_or_default();
    let insts: Vec<LegacyInstance> = serde_json::from_str(&raw).unwrap_or_default();
    let Some(inst) = insts.into_iter().find(|i| i.type_id == CHANNEL_TYPE && i.enabled) else {
        return;
    };
    if let Some(ws) = crate::key_store::lookup_scoped(Some(&inst.id), "WEBHOOK_SECRET") {
        let _ = crate::channels::set_webhook_secret(crate::channels::DEFAULT_SLUG, &ws);
    }
    if let Some(api) = crate::key_store::lookup_scoped(Some(&inst.id), "API_KEY") {
        let _ = crate::channels::set_secret(crate::channels::DEFAULT_SLUG, &api);
    }
    let get = |k: &str| inst.settings.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string);
    let _ = crate::channels::set_link(
        crate::channels::DEFAULT_SLUG,
        crate::channels::Link {
            integration_id: get("integration_id"),
            persona: get("persona"),
            model: get("model"),
            active_number: get("from"),
        },
    );

    // Clean up the migrated legacy instance so nothing stale lingers (e.g. the
    // old instance-UUID-scoped secrets showing as unnamed Keys entries): drop its
    // scoped secrets and remove the now-dead instance store. Best-effort.
    let kpath = crate::paths::keys_file();
    let mut kstore = crate::key_store::KeyStore::load(&kpath);
    if kstore.delete_channel(&inst.id) {
        if let Err(e) = kstore.save(&kpath) {
            log::warn!("migrate: mirrored channel but failed to prune legacy instance secrets: {e}");
        }
    }
    let _ = std::fs::remove_file(crate::paths::gateway_channels_state_file());

    log::info!("metalcraft-gateway: migrated legacy channel instance into the metalcraft channel model");
}

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

/// The pod's public base URL for its inbound webhook. Prefer the infra-injected
/// `POD_PUBLIC_URL` (authoritative), falling back to an explicit override supplied by
/// the caller (e.g. the workshop passing the URL it already uses to reach the pod).
fn webhook_base(explicit: Option<String>) -> Option<String> {
    std::env::var("POD_PUBLIC_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| explicit.filter(|s| !s.trim().is_empty()))
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
    /// The gateway base URL. The metalcraft channel synthesizes its own url, so
    /// this is informational only.
    #[allow(dead_code)]
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
/// channel-scoped secrets, and create/enable the `metalcraft-gateway` channel.
/// Idempotent — safe to re-run to re-sync after a rotated secret or a reassigned
/// number.
///
/// When `connection_token` is supplied (the k3 broker's audience-scoped token),
/// it is used as the gateway bearer *and* adopted as the channel's outbound
/// `API_KEY`, so the pod stops sending with its broad `METALCRAFT_TOKEN`. Omitted
/// (the Workshop path) ⇒ the pod token is used and the send key keeps deriving
/// from `METALCRAFT_TOKEN`.
pub async fn connect(
    explicit_webhook_base: Option<String>,
    connection_token: Option<String>,
) -> Result<ConnectResult, String> {
    let gw = gateway_url();
    let tok = token()?;
    // Prefer the audience-scoped connection token for the gateway handshake.
    let connection_token = connection_token.filter(|s| !s.trim().is_empty());
    let auth = connection_token.as_deref().unwrap_or(tok.as_str());
    // A public URL is now OPTIONAL. In pull mode the pod has no POD_PUBLIC_URL and needs
    // none — inbound is drained via the long-poll, so there's no webhook to register. We
    // still send + persist one when we have it (push/dual mode) so a webhook consumer keeps
    // working during the transition.
    let base = webhook_base(explicit_webhook_base);
    let body = match &base {
        Some(b) => serde_json::json!({ "webhook_url": format!("{b}/webhook/gateway") }),
        None => serde_json::json!({}),
    };

    let resp = client()?
        .post(format!("{gw}/api/v1/agent/connect"))
        .bearer_auth(auth)
        .json(&body)
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

    // Write the link + secrets into the built-in `metalcraft` channel. The
    // channel is the single source of truth for the gateway connection.
    let path = crate::paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    // Adopt the audience-scoped connection token as the channel's outbound bearer
    // (else outbound falls back to the pod's broad METALCRAFT_TOKEN). Store the
    // signing secret for inbound HMAC, and the webhook base (when we have one) so
    // `resync` can re-register even after POD_PUBLIC_URL is lost. Drop any legacy
    // global PIPESTREAMR_* keys left by an older build.
    if let Some(ct) = connection_token.as_deref() {
        store.upsert_channel(crate::channels::DEFAULT_SLUG, "SECRET", ct);
    }
    store.upsert_channel(crate::channels::DEFAULT_SLUG, "WEBHOOK_SECRET", &cfg.signing_secret);
    if let Some(b) = &base {
        store.upsert_channel(crate::channels::DEFAULT_SLUG, "WEBHOOK_BASE", b);
    }
    for legacy in ["PIPESTREAMR_BASE_URL", "PIPESTREAMR_API_KEY", "PIPESTREAMR_WEBHOOK_SECRET"] {
        store.delete(legacy);
    }
    store.save(&path).map_err(|e| format!("failed to write channel secrets: {e}"))?;

    // Preserve any persona/model already pinned on the channel across reconnects.
    let existing = crate::channels::get_channel(crate::channels::DEFAULT_SLUG);
    if let Err(e) = crate::channels::set_link(
        crate::channels::DEFAULT_SLUG,
        crate::channels::Link {
            integration_id: Some(cfg.integration_id.clone()),
            persona: existing.as_ref().and_then(|c| c.persona.clone()),
            model: existing.as_ref().and_then(|c| c.model.clone()),
            active_number: Some(cfg.active_number.clone()),
        },
    ) {
        return Err(format!("connected but failed to persist the channel link: {e}"));
    }

    Ok(ConnectResult {
        connected: true,
        active_number: cfg.active_number,
        integration_id: cfg.integration_id,
        channel: cfg.channel,
    })
}

/// Tear down the gateway link: disable the `metalcraft-gateway` channel and drop
/// its channel-scoped secrets (`BASE_URL`, `WEBHOOK_SECRET`, adopted `API_KEY`).
/// Idempotent — a no-op `Ok` when nothing is connected. The heal loop then stays
/// quiet (no enabled channel), and inbound webhooks stop verifying.
pub async fn disconnect() -> Result<(), String> {
    // Clear the metalcraft channel's link + its scoped secrets. `clear_link`
    // already drops the WEBHOOK_SECRET; also drop the adopted outbound SECRET and
    // the stored WEBHOOK_BASE. Idempotent.
    crate::channels::clear_link(crate::channels::DEFAULT_SLUG)?;
    let path = crate::paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    let mut changed = false;
    for k in ["SECRET", "WEBHOOK_BASE"] {
        changed |= store.delete_channel_key(crate::channels::DEFAULT_SLUG, k);
    }
    if changed {
        store.save(&path).map_err(|e| format!("failed to clear channel secrets: {e}"))?;
    }
    log::info!("metalcraft-gateway: disconnected the metalcraft channel");
    Ok(())
}

/// URL of the k3 control plane (for connection-token refresh). Injected as
/// `METALCRAFT_K3_URL`; defaults to production.
fn k3_url() -> String {
    crate::key_store::lookup("METALCRAFT_K3_URL")
        .or_else(|| std::env::var("METALCRAFT_K3_URL").ok())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://pods.metalcraftai.com".to_string())
}

/// This pod's slug — first label of `POD_PUBLIC_URL`'s host. `None` when unknown.
fn pod_slug() -> Option<String> {
    let url = std::env::var("POD_PUBLIC_URL").ok()?;
    let host = url.trim().trim_start_matches("https://").trim_start_matches("http://");
    let label = host.split('/').next().unwrap_or(host).split('.').next().unwrap_or("");
    (!label.is_empty()).then(|| label.to_string())
}

/// Refresh the adopted connection token from k3 before it expires. No-op unless a
/// `metalcraft-gateway` channel is enabled *and* holds an adopted `API_KEY` (i.e.
/// it was connected via the broker, not the legacy METALCRAFT_TOKEN path). Best
/// effort: on failure the current token stands until its own expiry.
pub async fn refresh_connection_token() -> Result<(), String> {
    let connected = crate::channels::get_channel(crate::channels::DEFAULT_SLUG)
        .map(|c| c.connected)
        .unwrap_or(false);
    if !connected {
        return Ok(());
    }
    // Only broker-connected channels carry an adopted SECRET worth refreshing.
    if crate::key_store::lookup_scoped(Some(crate::channels::DEFAULT_SLUG), "SECRET").is_none() {
        return Ok(());
    }
    let slug = pod_slug().ok_or("no POD_PUBLIC_URL — cannot identify pod for refresh")?;
    let tok = token()?;

    #[derive(Deserialize)]
    struct RefreshResp {
        connection_token: String,
    }
    let resp = client()?
        .post(format!("{}/api/pods/{slug}/connection/refresh", k3_url()))
        .bearer_auth(&tok)
        .send()
        .await
        .map_err(|e| format!("refresh request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("refresh returned HTTP {}", resp.status().as_u16()));
    }
    let r: RefreshResp = resp.json().await.map_err(|e| format!("parse refresh: {e}"))?;
    if r.connection_token.trim().is_empty() {
        return Err("refresh returned an empty token".to_string());
    }

    let path = crate::paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    store.upsert_channel(crate::channels::DEFAULT_SLUG, "SECRET", r.connection_token.trim());
    store.save(&path).map_err(|e| format!("write refreshed token: {e}"))?;
    log::debug!("metalcraft-gateway: refreshed connection token");
    Ok(())
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
    /// The inbound webhook the gateway will POST to. Compared against this pod's own
    /// URL to detect the "connected but stale webhook" drift.
    #[serde(default)]
    consumer_webhook_url: Option<String>,
}

/// What the workshop Connect panel renders from.
#[derive(Serialize, Default, utoipa::ToSchema)]
pub struct GatewayStatus {
    /// The pod has a token (is linked to a Metalcraft ID account).
    pub configured: bool,
    /// The user has registered a personal number on the gateway.
    pub registered: bool,
    /// That number is verified — required before connecting.
    pub verified: bool,
    /// A local `metalcraft-gateway` channel is enabled and the webhook secret is set.
    pub connected: bool,
    /// The Inbound Pull long-poll is currently connected and draining. Unlike `connected`
    /// (config present on disk), this is *intrinsic* liveness — it cannot be true while
    /// inbound delivery is actually dead. `false` in push mode (no long-poll runs).
    pub streaming: bool,
    pub active_number: Option<String>,
    pub channel: Option<String>,
    /// Whether the pod knows its own public URL (POD_PUBLIC_URL) for the webhook.
    pub has_public_url: bool,
    /// The gateway's registered inbound webhook (`consumer_webhook_url`) no longer
    /// points at this pod's current URL, so inbound messages are being delivered into
    /// the void. `connected` can be true while this is true — the "green light, dead
    /// pipe" state. Detecting it kicks off a reactive re-register (see `status`).
    pub webhook_stale: bool,
    pub error: Option<String>,
}

/// Report registration/verification/connection state for the workshop.
pub async fn status() -> GatewayStatus {
    let connected = crate::channels::get_channel(crate::channels::DEFAULT_SLUG)
        .map(|c| c.connected)
        .unwrap_or(false)
        && crate::channels::webhook_secret(crate::channels::DEFAULT_SLUG).is_some();
    let my_webhook = webhook_base(None).map(|b| format!("{b}/webhook/gateway"));
    let has_public_url = my_webhook.is_some();
    let streaming = is_streaming();

    let tok = match token() {
        Ok(t) => t,
        Err(e) => {
            return GatewayStatus { connected, streaming, has_public_url, error: Some(e), ..Default::default() };
        }
    };

    let resp = match client() {
        Ok(c) => c.get(format!("{}/api/v1/phone", gateway_url())).bearer_auth(&tok).send().await,
        Err(e) => return GatewayStatus { configured: true, connected, streaming, has_public_url, error: Some(e), ..Default::default() },
    };
    match resp {
        Ok(r) if r.status().is_success() => match r.json::<PhoneResp>().await {
            Ok(p) => {
                // "Green light, dead pipe" detection: the gateway POSTs inbound to
                // `consumer_webhook_url`. If we're connected but that no longer equals
                // this pod's own URL, inbound is being dropped. Kick a reactive heal so
                // merely opening this panel (like the periodic heal loop) re-registers
                // the webhook. Only judged when we know our own URL.
                // Suppressed while `streaming`: a live long-poll proves inbound is being
                // drained, so a stored `consumer_webhook_url` is irrelevant (pull mode).
                let webhook_stale = connected
                    && !streaming
                    && my_webhook.is_some()
                    && p.consumer_webhook_url.as_deref() != my_webhook.as_deref();
                if webhook_stale {
                    log::warn!(
                        "metalcraft-gateway: registered webhook {:?} != this pod {:?}; re-registering",
                        p.consumer_webhook_url, my_webhook
                    );
                    maybe_reactive_resync();
                }
                GatewayStatus {
                    configured: true,
                    registered: p.personal_number.is_some(),
                    verified: p.verified,
                    connected,
                    streaming,
                    active_number: p.active_number,
                    channel: p.channel,
                    has_public_url,
                    webhook_stale,
                    error: None,
                }
            }
            Err(e) => GatewayStatus { configured: true, connected, streaming, has_public_url, error: Some(format!("parse /phone: {e}")), ..Default::default() },
        },
        Ok(r) => GatewayStatus { configured: true, connected, streaming, has_public_url, error: Some(format!("gateway /phone returned HTTP {}", r.status().as_u16())), ..Default::default() },
        Err(e) => GatewayStatus { configured: true, connected, streaming, has_public_url, error: Some(format!("gateway unreachable: {e}")), ..Default::default() },
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

/// One-time boot migration: move legacy **global** `PIPESTREAMR_*` secrets into
/// the built-in `metalcraft` channel's scope (`WEBHOOK_SECRET`) and delete the
/// globals. The old `PIPESTREAMR_API_KEY`/`BASE_URL` are dropped (the send key is
/// the pod token now and the url is synthesized). Idempotent.
pub fn migrate_legacy_keys() {
    let path = crate::paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);

    let secret = store.get("PIPESTREAMR_WEBHOOK_SECRET").map(str::to_string);
    let had_legacy = store.get("PIPESTREAMR_BASE_URL").is_some()
        || secret.is_some()
        || store.get("PIPESTREAMR_API_KEY").is_some();

    // Nothing to migrate — still persist the (idempotent) v2 schema upgrade so
    // pre-v2 files get rewritten once on boot.
    if !had_legacy {
        let _ = store.save(&path);
        return;
    }

    if let Some(v) = secret {
        store.upsert_channel(crate::channels::DEFAULT_SLUG, "WEBHOOK_SECRET", &v);
    }
    store.delete("PIPESTREAMR_BASE_URL");
    store.delete("PIPESTREAMR_API_KEY");
    store.delete("PIPESTREAMR_WEBHOOK_SECRET");
    if let Err(e) = store.save(&path) {
        log::warn!("metalcraft-gateway: failed to persist legacy key migration: {e}");
        return;
    }
    log::info!("metalcraft-gateway: migrated legacy PIPESTREAMR_* keys into the metalcraft channel scope");
}

// ── Self-heal (Phase 3) ──────────────────────────────────────────────────────

/// Re-sync the connection so a **rotated webhook secret** or a **reassigned number**
/// heals itself. No-op (`Ok`) unless an enabled `metalcraft-gateway` channel exists.
/// When `POD_PUBLIC_URL` is known it does a full [`connect`] (also re-registering the
/// webhook); otherwise it refreshes the secret + `integration_id`/`from` from
/// `GET /api/v1/phone` without touching the webhook.
pub async fn resync() -> Result<(), String> {
    let ch = crate::channels::get_channel(crate::channels::DEFAULT_SLUG);
    if !ch.as_ref().map(|c| c.connected).unwrap_or(false) {
        return Ok(()); // nothing connected — stay quiet
    }

    // Prefer a full re-register: it re-points the gateway's `consumer_webhook_url` at
    // our current URL, healing the "connected but stale webhook" drift. Use
    // POD_PUBLIC_URL if known, else the base captured at connect time
    // (`WEBHOOK_BASE`), so heal still works after POD_PUBLIC_URL is lost.
    let base = webhook_base(None)
        .or_else(|| crate::key_store::lookup_scoped(Some(crate::channels::DEFAULT_SLUG), "WEBHOOK_BASE"));
    if let Some(base) = base {
        return match connect(Some(base), None).await {
            Ok(_) => Ok(()),
            // A membership lapse mid-life shouldn't spam errors.
            Err(e) if e == VERIFY_REQUIRED => Ok(()),
            Err(e) => Err(e),
        };
    }

    // No URL anywhere (can't fix the webhook): refresh secret + routing from /phone
    // into the metalcraft channel.
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
        if crate::channels::webhook_secret(crate::channels::DEFAULT_SLUG).as_deref() != Some(secret.as_str()) {
            crate::channels::set_webhook_secret(crate::channels::DEFAULT_SLUG, &secret)?;
        }
    }
    if let (Some(iid), Some(from)) = (p.integration_id, p.active_number) {
        let cur = crate::channels::get_channel(crate::channels::DEFAULT_SLUG);
        let drifted = cur.as_ref().and_then(|c| c.integration_id.as_deref()) != Some(iid.as_str())
            || cur.as_ref().and_then(|c| c.active_number.as_deref()) != Some(from.as_str());
        if drifted {
            crate::channels::set_link(
                crate::channels::DEFAULT_SLUG,
                crate::channels::Link {
                    integration_id: Some(iid),
                    persona: cur.as_ref().and_then(|c| c.persona.clone()),
                    model: cur.as_ref().and_then(|c| c.model.clone()),
                    active_number: Some(from),
                },
            )?;
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
        // Refresh the audience-scoped connection token before it expires (no-op
        // unless broker-connected), then re-sync secret/number drift.
        if let Err(e) = refresh_connection_token().await {
            log::warn!("metalcraft-gateway token refresh: {e}");
        }
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
