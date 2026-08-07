//! Thin client for the Metalcraft Packs registry (packs.metalcraftai.com).
//!
//! Today it only downloads a pack's ZIP so the agent can install it locally (see
//! [`crate::integration_packs::install_from_zip`]). The registry's
//! `GET /api/v1/packs/{slug}/download` endpoint is public, so no auth is needed.
use std::time::Duration;

/// Bytes we refuse to download for one pack — mirrors the extract-time cap in
/// [`crate::integration_packs`]. Packs are tiny (JSON + markdown).
const MAX_DOWNLOAD_BYTES: usize = 16 * 1024 * 1024;

/// The registry's base origin. Overridable via `PACKS_BASE_URL` for dev/self-host.
pub fn base_url() -> String {
    std::env::var("PACKS_BASE_URL").unwrap_or_else(|_| "https://packs.metalcraftai.com".to_string())
}

/// Download the ZIP for pack `slug` from the registry's public download endpoint.
pub async fn fetch_zip(slug: &str) -> Result<Vec<u8>, String> {
    let url = format!("{}/api/v1/packs/{}/download", base_url().trim_end_matches('/'), slug);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("registry request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("registry returned {status} for pack '{slug}'"));
    }
    // Reject an oversized body before buffering it all into memory.
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_DOWNLOAD_BYTES {
            return Err(format!("pack '{slug}' download is too large"));
        }
    }
    let bytes = resp.bytes().await.map_err(|e| format!("reading registry response: {e}"))?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(format!("pack '{slug}' download is too large"));
    }
    Ok(bytes.to_vec())
}
