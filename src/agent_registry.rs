//! Fetching an agent pack from a registry.
//!
//! Agent packs are hosted by **peer registries** — `axoniac` and
//! `packs.metalcraftai.com` both serve them, and neither is a dependency of the
//! other. So a pod does not resolve a slug against one canonical origin the way it
//! does for integration packs; it is handed a URL and decides whether it is willing
//! to fetch it.
//!
//! ## Why an allowlist
//!
//! "Fetch this URL" issued to a pod is a request made *from inside* whatever network
//! the pod runs in. Installing is already a deliberate, consent-gated act by the pod
//! owner, but the owner is approving *an agent*, not "any HTTP GET my pod can
//! reach" — and the two are not the same when the pod sits next to a metadata
//! service or an internal admin panel.
//!
//! So the origin must be on the allowlist. `AGENT_PACK_REGISTRIES` (comma-separated)
//! replaces the default for self-hosters and for pointing a dev pod at a local
//! registry. Nothing here makes the *contents* trustworthy — that is
//! [`crate::agent_packs::bundle`]'s job, and it re-derives everything it shows a
//! human from the bytes.
use std::time::Duration;

/// An agent pack carries seed memories and assets, so it is larger than an
/// integration pack — but still small. This is a bomb guard, matching the
/// extract-time cap in [`crate::agent_packs::bundle::MAX_BUNDLE_BYTES`].
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;

const DEFAULT_REGISTRIES: &[&str] = &["https://packs.metalcraftai.com"];

/// Origins this pod will fetch an agent pack from.
///
/// Returned rather than only checked so a UI can *say* what it will accept before
/// the user pastes a link and gets refused.
pub fn allowed_origins() -> Vec<String> {
    match std::env::var("AGENT_PACK_REGISTRIES") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => DEFAULT_REGISTRIES.iter().map(|s| s.to_string()).collect(),
    }
}

/// The scheme+host+port of a URL, lowercased, or `None` if it isn't one.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip userinfo: `https://evil@allowed.example/` is not the allowed origin, and
    // treating it as one is the classic way an allowlist gets walked past.
    let host_port = authority.rsplit('@').next()?;
    if host_port.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{}", host_port.to_ascii_lowercase()))
}

/// Whether this pod is willing to fetch `url`.
pub fn is_allowed(url: &str) -> bool {
    match origin_of(url) {
        Some(origin) => allowed_origins()
            .iter()
            .any(|a| origin_of(a).is_some_and(|allowed| allowed == origin)),
        None => false,
    }
}

/// Download an agent pack archive.
///
/// Returns the raw bytes; nothing is trusted about them here. Redirects are not
/// followed: a redirect is how an allowlisted origin would otherwise be used to
/// reach one that isn't.
pub async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if !is_allowed(url) {
        return Err(format!(
            "this pod will not download from that origin. Allowed: {}. \
             Set AGENT_PACK_REGISTRIES to change it, or upload the .agentpack directly.",
            allowed_origins().join(", ")
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let resp = client.get(url).send().await.map_err(|e| format!("download failed: {e}"))?;
    let status = resp.status();
    if status.is_redirection() {
        return Err(format!(
            "the registry redirected ({status}); this pod does not follow redirects when \
             downloading an agent pack. Use the final URL."
        ));
    }
    if !status.is_success() {
        return Err(format!("registry returned {status} for {url}"));
    }
    if resp.content_length().is_some_and(|l| l as usize > MAX_DOWNLOAD_BYTES) {
        return Err("that agent pack is too large".to_string());
    }
    let bytes = resp.bytes().await.map_err(|e| format!("reading response: {e}"))?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err("that agent pack is too large".to_string());
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allowlist is read from the environment, which is process-global, so these
    /// tests must not run concurrently with each other — cargo runs them on threads
    /// by default and they would clobber one another's variable.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_registries<T>(value: &str, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK, and the variable is restored before the
        // guard drops.
        unsafe { std::env::set_var("AGENT_PACK_REGISTRIES", value) };
        let out = f();
        unsafe { std::env::remove_var("AGENT_PACK_REGISTRIES") };
        out
    }

    /// Read the allowlist with no override set, under the same lock.
    fn default_origins() -> Vec<String> {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("AGENT_PACK_REGISTRIES") };
        allowed_origins()
    }

    #[test]
    fn an_allowlisted_origin_is_accepted_at_any_path() {
        with_registries("https://axoniac.example", || {
            assert!(is_allowed("https://axoniac.example/api/v1/agent-packs/amy/download"));
            assert!(is_allowed("https://AXONIAC.example/x"), "host match is case-insensitive");
        });
    }

    #[test]
    fn another_origin_is_refused() {
        with_registries("https://axoniac.example", || {
            assert!(!is_allowed("https://evil.example/pack.agentpack"));
            // A prefix that merely *starts* with the allowed host is a different host.
            assert!(!is_allowed("https://axoniac.example.evil.test/x"));
            // …and so is one that merely ends with it.
            assert!(!is_allowed("https://notaxoniac.example/x"));
        });
    }

    #[test]
    fn userinfo_cannot_disguise_the_origin() {
        // `https://allowed@evil.example/` has host `evil.example`. Reading the
        // authority naively is how an allowlist gets walked past.
        with_registries("https://axoniac.example", || {
            assert!(!is_allowed("https://axoniac.example@evil.example/pack"));
        });
    }

    #[test]
    fn the_scheme_must_match_too() {
        with_registries("https://axoniac.example", || {
            assert!(!is_allowed("http://axoniac.example/x"), "plaintext is a different origin");
        });
        // …and a local dev registry over http works when that is what was configured.
        with_registries("http://localhost:8080", || {
            assert!(is_allowed("http://localhost:8080/api/v1/agent-packs/x/download"));
            assert!(!is_allowed("http://localhost:9999/x"), "the port is part of the origin");
        });
    }

    #[test]
    fn non_http_schemes_are_refused() {
        with_registries("https://axoniac.example", || {
            assert!(!is_allowed("file:///etc/passwd"));
            assert!(!is_allowed("ftp://axoniac.example/x"));
            assert!(!is_allowed("not a url"));
            assert!(!is_allowed(""));
        });
    }

    #[test]
    fn several_registries_are_peers() {
        // Neither is canonical — that is the whole point of the peer model.
        with_registries("https://axoniac.example, https://packs.example/", || {
            assert!(is_allowed("https://axoniac.example/a"));
            assert!(is_allowed("https://packs.example/b"));
            assert_eq!(allowed_origins().len(), 2);
        });
    }

    #[test]
    fn the_default_is_the_metalcraft_registry() {
        // Explicit: an unconfigured pod still installs from somewhere sensible.
        assert_eq!(default_origins(), vec!["https://packs.metalcraftai.com"]);
    }
}
