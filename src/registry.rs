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

/// The flows registry's base origin (flows.metalcraftai.com). Overridable via
/// `FLOWS_BASE_URL` for dev/self-host, mirroring `PACKS_BASE_URL`.
pub fn flows_base_url() -> String {
    std::env::var("FLOWS_BASE_URL").unwrap_or_else(|_| "https://flows.metalcraftai.com".to_string())
}

/// Download and parse the `SavedFlow` for `slug` from the flows registry's public
/// download endpoint. A flow is a single self-contained JSON document, so unlike a
/// pack there's no ZIP to extract — this returns the parsed flow ready to save.
pub async fn fetch_flow(slug: &str) -> Result<metalcraft_flows::SavedFlow, String> {
    let url = format!("{}/api/v1/flows/{}/download", flows_base_url().trim_end_matches('/'), slug);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("flows registry request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("flows registry returned {status} for flow '{slug}'"));
    }
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_DOWNLOAD_BYTES {
            return Err(format!("flow '{slug}' download is too large"));
        }
    }
    let bytes = resp.bytes().await.map_err(|e| format!("reading registry response: {e}"))?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(format!("flow '{slug}' download is too large"));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid flow document from registry: {e}"))
}

/// Ask the registry to resolve a semver range to the highest published version of
/// `slug`, returning `(version, content_sha256)`. `range = None` resolves to the
/// latest version. This is the first hop of a requirement-driven install:
/// resolve → [`fetch_zip`] that version → verify the hash on install.
pub async fn resolve_pack_version(
    slug: &str,
    range: Option<&str>,
) -> Result<(String, String), String> {
    let url = format!("{}/api/v1/packs/{}/resolve", base_url().trim_end_matches('/'), slug);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut req = client.get(&url);
    if let Some(r) = range {
        req = req.query(&[("range", r)]);
    }
    let resp = req.send().await.map_err(|e| format!("registry request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!(
            "registry could not resolve pack '{slug}'{}: {status}",
            range.map(|r| format!(" for range {r}")).unwrap_or_default()
        ));
    }
    let body: serde_json::Value =
        resp.json().await.map_err(|e| format!("reading resolve response: {e}"))?;
    let version = body
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("resolve response missing version")?
        .to_string();
    let content_sha256 = body
        .get("content_sha256")
        .and_then(|v| v.as_str())
        .ok_or("resolve response missing content_sha256")?
        .to_string();
    Ok((version, content_sha256))
}

/// The flows registry's latest `{ version, content_sha256 }` for `slug`
/// (`GET /flows/{slug}/version`). `content_sha256` may be absent on legacy rows that
/// predate version hashing — returned as `None` then.
pub async fn flow_version(slug: &str) -> Result<(String, Option<String>), String> {
    let url = format!("{}/api/v1/flows/{}/version", flows_base_url().trim_end_matches('/'), slug);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client.get(&url).send().await.map_err(|e| format!("flows registry request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("flows registry returned {status} for flow '{slug}'"));
    }
    let body: serde_json::Value =
        resp.json().await.map_err(|e| format!("reading version response: {e}"))?;
    let version = body
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("version response missing version")?
        .to_string();
    let content_sha256 = body.get("content_sha256").and_then(|v| v.as_str()).map(str::to_string);
    Ok((version, content_sha256))
}

/// Download a flow's exact bytes from the flows registry. When `version` is `Some`,
/// requests that pinned version (`?version=`) so the bytes — and their hash — match
/// what was locked. Returns the raw document bytes (not parsed), so the caller can
/// verify the content hash before trusting them.
pub async fn fetch_flow_bytes(slug: &str, version: Option<&str>) -> Result<Vec<u8>, String> {
    let mut url = format!("{}/api/v1/flows/{}/download", flows_base_url().trim_end_matches('/'), slug);
    if let Some(v) = version {
        url.push_str(&format!("?version={}", v.replace('+', "%2B")));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client.get(&url).send().await.map_err(|e| format!("flows registry request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("flows registry returned {status} for flow '{slug}'"));
    }
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_DOWNLOAD_BYTES {
            return Err(format!("flow '{slug}' download is too large"));
        }
    }
    let bytes = resp.bytes().await.map_err(|e| format!("reading registry response: {e}"))?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(format!("flow '{slug}' download is too large"));
    }
    Ok(bytes.to_vec())
}

/// A pack that provides a tool, from the registry's tool → pack index.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ToolProvider {
    /// Providing pack slug.
    pub slug: String,
    /// The pack's latest version.
    #[serde(default)]
    pub version: String,
    /// `"http"` (declarative api_tool) or `"native"`.
    #[serde(default)]
    pub kind: String,
    /// Whether the pack is first-party / verified.
    #[serde(default)]
    pub verified: bool,
}

/// Resolve tool names to the packs that provide them, via the registry's bulk
/// tool → pack index (`GET /api/v1/tools/resolve?names=a,b`). Returns a map from
/// each requested name to its providers (unknown names map to an empty list).
/// Used to enrich a flow's pack requirements from the bare `tool_name`s it binds.
pub async fn resolve_tools(
    names: &[String],
) -> Result<std::collections::HashMap<String, Vec<ToolProvider>>, String> {
    if names.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let url = format!("{}/api/v1/tools/resolve", base_url().trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(&url)
        .query(&[("names", names.join(","))])
        .send()
        .await
        .map_err(|e| format!("registry request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("registry tool resolve failed: {status}"));
    }
    resp.json().await.map_err(|e| format!("reading resolve response: {e}"))
}

/// Download the ZIP for pack `slug` from the registry's public download endpoint.
///
/// When `version` is `Some`, requests that specific published version
/// (`?version=`) instead of the latest — used to satisfy a flow's pinned/ranged
/// pack requirement.
pub async fn fetch_zip(slug: &str, version: Option<&str>) -> Result<Vec<u8>, String> {
    let mut url = format!("{}/api/v1/packs/{}/download", base_url().trim_end_matches('/'), slug);
    if let Some(v) = version {
        // Concrete semver only reaches here; `+` (build metadata) is the sole
        // char needing escaping in a query value.
        url.push_str(&format!("?version={}", v.replace('+', "%2B")));
    }
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
