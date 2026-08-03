//! Metalcraft ID bearer auth for the pod's `workshop_api`.
//!
//! Historically the workshop API accepted exactly one credential: a static
//! `WORKSHOP_API_KEY` compared byte-for-byte. That key never expires and is the
//! same for every caller. This module adds a second, expiring credential type:
//! a Metalcraft ID token (`mck_…` PAT) validated against the hub's `/verify`
//! endpoint. Both the connect broker (k3) and the Workshop can obtain such a
//! token, so the pod's surface stays simple — resolve a bearer to "the owner, or
//! a token audience-scoped to this pod, else reject".
//!
//! Acceptance rules for an `mck_` bearer:
//!   • the hub says it is `active`, and
//!   • if the token carries an `aud`, that list must contain `pod:{slug}` (a
//!     token minted specifically for this pod — e.g. a connection token), OR
//!   • if the token is unscoped (`aud` = null), its `sub` must equal this pod's
//!     owner (so a broad user PAT reaches only its own pod, never someone else's).
//!
//! Positive results are cached briefly to keep the hot path (the Workshop makes
//! many calls) from hitting the hub on every request. The static `WORKSHOP_API_KEY`
//! path is unchanged and still handled by the caller.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const DEFAULT_HUB: &str = "https://id.metalcraftai.com";
/// Metalcraft ID PATs are opaque `mck_…` bearers.
const PAT_PREFIX: &str = "mck_";
/// How long a positive `/verify` result is trusted before re-checking. Short, so
/// a revoked token stops working quickly; long enough to spare the hub on bursts.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// The Metalcraft ID base URL for server→server calls. Override with the
/// `METALCRAFT_ID_URL` key or `HUB_INTERNAL_URL` env; defaults to production.
fn hub_url() -> String {
    crate::key_store::lookup("METALCRAFT_ID_URL")
        .or_else(|| std::env::var("HUB_INTERNAL_URL").ok())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_HUB.to_string())
}

/// This pod's slug — the first label of `POD_PUBLIC_URL`'s host
/// (`https://{slug}.{domain}` ⇒ `{slug}`). `None` when unknown (dev/standalone),
/// which disables audience-scoped acceptance (only owner-matched tokens work).
fn pod_slug() -> Option<String> {
    let url = std::env::var("POD_PUBLIC_URL").ok()?;
    let host = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split('/').next().unwrap_or(host);
    let label = host.split('.').next().unwrap_or("");
    (!label.is_empty()).then(|| label.to_string())
}

#[derive(Deserialize)]
struct VerifyResp {
    active: bool,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    aud: Option<Vec<String>>,
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.trim().as_bytes()))
}

/// Positive-result cache: token hash → when the cached OK expires.
fn cache() -> &'static Mutex<HashMap<String, Instant>> {
    static C: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// This pod's owner `sub`, learned by verifying its own `METALCRAFT_TOKEN` once
/// and cached. `None` when the pod holds no token (unlinked) or the hub is
/// unreachable — in which case unscoped tokens are rejected (fail closed).
async fn owner_sub() -> Option<String> {
    static OWNER: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();
    let cell = OWNER.get_or_init(|| Mutex::new(None));
    if let Some(s) = cell.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(s);
    }
    let tok = crate::key_store::lookup("METALCRAFT_TOKEN").filter(|s| !s.is_empty())?;
    let v = verify_raw(&tok).await?;
    if !v.active {
        return None;
    }
    let sub = v.sub?;
    *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(sub.clone());
    Some(sub)
}

/// One `/verify` round-trip. `None` on any transport/parse error (fail closed).
async fn verify_raw(token: &str) -> Option<VerifyResp> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("metalcraft-agent (workshop-auth)")
        .build()
        .ok()?;
    let resp = client
        .post(format!("{}/verify", hub_url()))
        .json(&serde_json::json!({ "token": token }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<VerifyResp>().await.ok()
}

/// Validate an `mck_` bearer for the workshop API. Returns `true` iff it resolves
/// to this pod's owner or a token audience-scoped to this pod. Never panics.
pub async fn verify_pod_bearer(token: &str) -> bool {
    let token = token.trim();
    if !token.starts_with(PAT_PREFIX) {
        return false;
    }

    // Fast path: a recent positive result.
    let h = hash(token);
    {
        let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(exp) = c.get(&h) {
            if *exp > Instant::now() {
                return true;
            }
            c.remove(&h);
        }
    }

    let Some(v) = verify_raw(token).await else {
        return false;
    };
    if !v.active {
        return false;
    }

    let accepted = match v.aud {
        // Audience-scoped: must name this pod (e.g. a connection token).
        Some(list) if !list.is_empty() => {
            pod_slug().is_some_and(|slug| list.iter().any(|a| a == &format!("pod:{slug}")))
        }
        // Unscoped: only the pod's own owner may use it.
        _ => match (v.sub, owner_sub().await) {
            (Some(sub), Some(owner)) => sub == owner,
            _ => false,
        },
    };

    if accepted {
        cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(h, Instant::now() + CACHE_TTL);
    }
    accepted
}
