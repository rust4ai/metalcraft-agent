//! Thin client for the ecosystem **coordinator** (cross-tenant relay). When a
//! pod shares content, it registers `{token → this pod, ref}` so a neutral
//! public URL (`{coordinator}/p/{token}`) can route to it; unshare deregisters.
//!
//! All calls are **best-effort**: the pod-local share works regardless (the pod
//! serves its own `/p/{token}`), so a missing/unreachable coordinator only means
//! the link isn't on the neutral domain. Configured via env:
//! `COORDINATOR_URL`, `COORDINATOR_SECRET`, and `POD_PUBLIC_URL` (for the slug).

use serde_json::json;

/// `(base_url, secret)` if a coordinator is configured.
fn configured() -> Option<(String, String)> {
    let url = std::env::var("COORDINATOR_URL").ok().filter(|s| !s.is_empty())?;
    let secret = std::env::var("COORDINATOR_SECRET").unwrap_or_default();
    Some((url.trim_end_matches('/').to_string(), secret))
}

/// This pod's slug, from the first label of `POD_PUBLIC_URL`'s host.
pub fn pod_slug() -> String {
    std::env::var("POD_PUBLIC_URL")
        .ok()
        .and_then(|u| u.split("://").nth(1).map(str::to_string))
        .and_then(|host| host.split('.').next().map(str::to_string))
        .unwrap_or_default()
}

/// The shareable URL for `token`: the coordinator's neutral URL when configured,
/// else a pod-local URL from `POD_PUBLIC_URL`.
pub fn share_url(mount: &str, token: &str) -> String {
    if let Some((url, _)) = configured() {
        format!("{url}/p/{token}")
    } else {
        let base = std::env::var("POD_PUBLIC_URL").unwrap_or_default();
        format!("{base}/apps/{mount}/p/{token}")
    }
}

pub async fn register_share(token: &str, kind: &str, reference: &str) {
    let Some((url, secret)) = configured() else { return };
    let body = json!({ "token": token, "pod_slug": pod_slug(), "kind": kind, "ref": reference });
    let client = reqwest::Client::new();
    if let Err(e) = client
        .post(format!("{url}/api/v1/shares"))
        .header("X-Metalcraft-Service-Secret", secret)
        .json(&body)
        .send()
        .await
    {
        log::warn!("coordinator register_share failed: {e}");
    }
}

pub async fn unregister_share(token: &str) {
    let Some((url, secret)) = configured() else { return };
    let client = reqwest::Client::new();
    if let Err(e) = client
        .delete(format!("{url}/api/v1/shares/{token}"))
        .header("X-Metalcraft-Service-Secret", secret)
        .send()
        .await
    {
        log::warn!("coordinator unregister_share failed: {e}");
    }
}
