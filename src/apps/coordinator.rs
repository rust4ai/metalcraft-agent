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

/// True if a coordinator is configured (invites require it — they're cross-tenant).
pub fn is_configured() -> bool {
    configured().is_some()
}

/// One guest's invite result from the coordinator.
pub struct InviteResult {
    pub email: String,
    pub token: String,
    pub rsvp: String,
}

/// Register calendar invites for an event's guests. Returns per-guest tokens, or
/// `None` if no coordinator is configured / the call fails.
#[allow(clippy::too_many_arguments)]
pub async fn register_invites(
    event_id: &str,
    organizer_email: Option<&str>,
    title: &str,
    starts_at: &str,
    ends_at: Option<&str>,
    location: Option<&str>,
    timezone: &str,
    guests: &[String],
) -> Option<Vec<InviteResult>> {
    let (url, secret) = configured()?;
    let body = json!({
        "event_id": event_id,
        "organizer_pod": pod_slug(),
        "organizer_email": organizer_email,
        "title": title,
        "starts_at": starts_at,
        "ends_at": ends_at,
        "location": location,
        "timezone": timezone,
        "guests": guests,
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/api/v1/invites"))
        .header("X-Metalcraft-Service-Secret", secret)
        .json(&body)
        .send()
        .await
        .map_err(|e| log::warn!("coordinator register_invites failed: {e}"))
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    let arr = v.get("invites")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|i| {
                Some(InviteResult {
                    email: i.get("email")?.as_str()?.to_string(),
                    token: i.get("token")?.as_str()?.to_string(),
                    rsvp: i.get("rsvp").and_then(|r| r.as_str()).unwrap_or("pending").to_string(),
                })
            })
            .collect(),
    )
}

/// List invites addressed to `email` (the guest mailbox). Returns the raw
/// `{ invites: [...] }` value, or `None` if no coordinator / the call fails.
pub async fn list_invites(email: &str) -> Option<serde_json::Value> {
    let (url, secret) = configured()?;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/api/v1/invites"))
        .query(&[("email", email)])
        .header("X-Metalcraft-Service-Secret", secret)
        .send()
        .await
        .ok()?;
    if resp.status().is_success() {
        resp.json().await.ok()
    } else {
        None
    }
}

/// Record the owner's RSVP to an invite (as a guest). Returns the coordinator's
/// `{ ok, rsvp }`, or `None` if not configured / failed.
pub async fn respond_invite(email: &str, event_id: &str, rsvp: &str) -> Option<serde_json::Value> {
    let (url, secret) = configured()?;
    let body = json!({ "email": email, "event_id": event_id, "rsvp": rsvp });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/api/v1/invites/rsvp"))
        .header("X-Metalcraft-Service-Secret", secret)
        .json(&body)
        .send()
        .await
        .ok()?;
    if resp.status().is_success() {
        resp.json().await.ok()
    } else {
        None
    }
}

/// The shared secret the coordinator presents on push-back webhooks.
pub fn service_secret() -> String {
    std::env::var("COORDINATOR_SECRET").unwrap_or_default()
}

/// Fetch current RSVP statuses for an event (best-effort). `(email, rsvp)` pairs.
pub async fn fetch_rsvps(event_id: &str) -> Option<Vec<(String, String)>> {
    let (url, secret) = configured()?;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/api/v1/events/{event_id}/rsvps"))
        .header("X-Metalcraft-Service-Secret", secret)
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    let arr = v.get("rsvps")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|r| {
                Some((r.get("email")?.as_str()?.to_string(), r.get("rsvp")?.as_str()?.to_string()))
            })
            .collect(),
    )
}
