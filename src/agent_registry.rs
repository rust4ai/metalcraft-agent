//! Registries: where agent packs come from.
//!
//! **A registry is a protocol, not a host.** axoniac is the social discovery host,
//! `packs.metalcraftai.com` serves the first-party ecosystem packs, and a company can
//! self-host a private one. A pod treats them as interchangeable sources — the
//! crates.io alternative-registries model — and each one implements the four
//! endpoints in `specs/AGENT_PACK_FORMAT.md` §11.1.
//!
//! ## Two rules that are not conveniences
//!
//! **The origin must be configured.** "Fetch this URL" issued to a pod is a request
//! made *from inside* whatever network the pod runs in. Installing is already a
//! consent-gated act, but the owner is approving *an agent*, not "any HTTP GET my pod
//! can reach" — and those differ sharply when the pod sits next to a metadata service
//! or an internal admin panel. Redirects are refused and userinfo is stripped, both
//! being standard ways past an origin check.
//!
//! **An unqualified reference present on two registries is an error.** Not a
//! first-match, not a preference order. If `@amy_kitchen` exists on both axoniac and
//! an internal host, picking one silently is the supply-chain substitution attack
//! written as a feature. [`resolve`] asks every configured registry and refuses to
//! choose.
//!
//! Nothing here makes the *contents* trustworthy — that is
//! [`crate::agent_packs::bundle`]'s job, and it re-derives everything it shows a human
//! from the bytes.
use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// An agent pack carries seed memories and assets, so it is larger than an
/// integration — but still small. A bomb guard, matching the extract-time cap in
/// [`crate::agent_packs::bundle::MAX_BUNDLE_BYTES`].
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;

/// How much a host's word is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// Install with the ordinary approval prompt. For hosts the operator's own
    /// organisation runs.
    FirstParty,
    /// Refuse a pack the host has not marked verified, unless explicitly overridden.
    /// The right default for a host anyone can publish to.
    #[default]
    VerifiedOnly,
    /// The operator added this host by hand and is prompted on every install.
    Explicit,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Registry {
    pub url: String,
    #[serde(default)]
    pub trust: Trust,
    /// Key-store entry holding a bearer token for a private host. Never the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Registries {
    /// Which registry an unqualified `@handle` resolves against *when only one host
    /// has it*. Ambiguity is still an error (§11.3); this is not a tiebreak.
    pub default: String,
    pub registries: BTreeMap<String, Registry>,
}

impl Default for Registries {
    fn default() -> Self {
        let mut registries = BTreeMap::new();
        registries.insert(
            "axoniac".to_string(),
            Registry {
                url: "https://axoniac.com".to_string(),
                trust: Trust::VerifiedOnly,
                token_key: None,
            },
        );
        registries.insert(
            "metalcraft".to_string(),
            Registry {
                url: "https://packs.metalcraftai.com".to_string(),
                trust: Trust::FirstParty,
                token_key: None,
            },
        );
        Self { default: "axoniac".to_string(), registries }
    }
}

impl Registries {
    pub fn get(&self, name: &str) -> Option<&Registry> {
        self.registries.get(name)
    }

    /// The registry whose origin serves `url`, if any.
    pub fn owning(&self, url: &str) -> Option<(&str, &Registry)> {
        let origin = origin_of(url)?;
        self.registries
            .iter()
            .find(|(_, r)| origin_of(&r.url).is_some_and(|o| o == origin))
            .map(|(n, r)| (n.as_str(), r))
    }

    pub fn origins(&self) -> Vec<String> {
        self.registries.values().map(|r| r.url.trim_end_matches('/').to_string()).collect()
    }
}

fn config_file() -> std::path::PathBuf {
    crate::paths::data_dir().join("registries.json")
}

/// The configured registries.
///
/// Precedence: `AGENT_PACK_REGISTRIES` (a bare comma-separated origin list, kept for
/// dev and for pointing a pod at a local registry), then `<data>/registries.json`,
/// then the built-in defaults. The env override wins because it is the thing someone
/// reaches for when the config file is exactly what they are trying to bypass.
pub fn load() -> Registries {
    if let Ok(v) = std::env::var("AGENT_PACK_REGISTRIES")
        && !v.trim().is_empty()
    {
        let mut registries = BTreeMap::new();
        for (i, url) in v.split(',').map(str::trim).filter(|s| !s.is_empty()).enumerate() {
            // An origin list carries no trust information, so the only honest reading
            // is "the operator named this deliberately" — which is `explicit`.
            registries.insert(
                format!("env{}", i + 1),
                Registry {
                    url: url.trim_end_matches('/').to_string(),
                    trust: Trust::Explicit,
                    token_key: None,
                },
            );
        }
        let default = registries.keys().next().cloned().unwrap_or_default();
        return Registries { default, registries };
    }

    match std::fs::read_to_string(config_file()) {
        Ok(s) => match serde_json::from_str::<Registries>(&s) {
            Ok(r) if !r.registries.is_empty() => r,
            Ok(_) => Registries::default(),
            Err(e) => {
                // A malformed config must not silently widen or narrow what this pod
                // will fetch from — say so and use the defaults.
                log::warn!("registries.json is malformed, using defaults: {e}");
                Registries::default()
            }
        },
        Err(_) => Registries::default(),
    }
}

/// Persist the registry configuration.
pub fn save(r: &Registries) -> Result<(), String> {
    let path = config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(r).map_err(|e| format!("serializing: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("finalizing {}: {e}", path.display()))
}

/// Origins this pod is willing to fetch an agent pack from.
///
/// Returned rather than only checked so a UI can *say* what it will accept, before
/// somebody pastes a link and gets refused.
pub fn allowed_origins() -> Vec<String> {
    load().origins()
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
    load().owning(url).is_some()
}

// ── reference resolution ─────────────────────────────────────────────────────

/// A reference the operator typed, parsed but not yet looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// `https://host/…` — a direct link.
    Url(String),
    /// `axoniac:@amy_kitchen` — qualified, so no ambiguity is possible.
    Qualified { registry: String, id: String },
    /// `@amy_kitchen` — every configured registry is asked (§11.3).
    Bare { id: String },
}

/// Parse `@handle`, `registry:@handle`, or a URL.
pub fn parse_reference(raw: &str) -> Result<Reference, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("an agent pack reference is required".to_string());
    }
    if raw.contains("://") {
        return Ok(Reference::Url(raw.to_string()));
    }
    // `registry:@handle` — split on the *first* colon so a handle may not contain one.
    if let Some((registry, rest)) = raw.split_once(':') {
        let id = rest.trim_start_matches('@');
        if registry.is_empty() || id.is_empty() {
            return Err(format!("'{raw}' is not a valid reference; try 'axoniac:@amy_kitchen'"));
        }
        return Ok(Reference::Qualified {
            registry: registry.to_string(),
            id: id.to_string(),
        });
    }
    Ok(Reference::Bare { id: raw.trim_start_matches('@').to_string() })
}

/// What a registry says about a pack, and where to get it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Resolved {
    /// The registry that answered.
    pub registry: String,
    pub id: String,
    pub version: String,
    pub content_sha256: String,
    pub download_url: String,
    /// Whether the host vouches for this pack. Meaningful only as far as the host's
    /// `trust` level makes it meaningful.
    pub verified: bool,
    pub trust: Trust,
}

async fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// Ask one registry for a pack's current version. `Ok(None)` means "this host does
/// not have it", which is different from "this host is broken".
async fn ask_version(
    name: &str,
    reg: &Registry,
    id: &str,
) -> Result<Option<Resolved>, String> {
    let base = reg.url.trim_end_matches('/');
    let url = format!("{base}/api/v1/agent-packs/{id}/version");
    let mut req = client().await?.get(&url);
    if let Some(key) = &reg.token_key
        && let Some(token) = crate::key_store::lookup(key)
    {
        req = req.bearer_auth(token);
    }

    let resp = req.send().await.map_err(|e| format!("{name}: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("{name} returned {} for {id}", resp.status()));
    }
    let body: serde_json::Value =
        resp.json().await.map_err(|e| format!("{name}: unreadable response: {e}"))?;

    let version = body
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{name}: response has no version"))?;
    Ok(Some(Resolved {
        registry: name.to_string(),
        id: id.to_string(),
        version: version.to_string(),
        content_sha256: body
            .get("content_sha256")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        download_url: format!("{base}/api/v1/agent-packs/{id}/download"),
        verified: body.get("verified").and_then(|v| v.as_bool()).unwrap_or(false),
        trust: reg.trust,
    }))
}

/// Resolve a reference to exactly one registry's answer.
///
/// A bare reference asks **every** configured registry. Two hosts claiming the same
/// id is an error naming both qualified forms — never a silent first match, because
/// that is precisely how a private host gets shadowed by a public one, or the
/// reverse.
pub async fn resolve(raw: &str) -> Result<Resolved, String> {
    let cfg = load();
    match parse_reference(raw)? {
        Reference::Url(url) => {
            let Some((name, reg)) = cfg.owning(&url) else {
                return Err(refuse_origin(&cfg));
            };
            Ok(Resolved {
                registry: name.to_string(),
                id: url.clone(),
                version: String::new(),
                content_sha256: String::new(),
                download_url: url,
                // A direct link carries no claim, so it gets none.
                verified: false,
                trust: reg.trust,
            })
        }
        Reference::Qualified { registry, id } => {
            let Some(reg) = cfg.get(&registry) else {
                return Err(format!(
                    "no registry named '{registry}' is configured. Configured: {}.",
                    cfg.registries.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            };
            ask_version(&registry, reg, &id)
                .await?
                .ok_or_else(|| format!("'{id}' is not published on {registry}"))
        }
        Reference::Bare { id } => {
            let mut hits: Vec<Resolved> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            for (name, reg) in &cfg.registries {
                match ask_version(name, reg, &id).await {
                    Ok(Some(r)) => hits.push(r),
                    Ok(None) => {}
                    // One unreachable host must not stop the others answering — but
                    // it also must not be swallowed, because "not found" and "could
                    // not ask" are very different answers to "is this ambiguous".
                    Err(e) => errors.push(e),
                }
            }
            match hits.len() {
                1 => Ok(hits.remove(0)),
                0 if errors.is_empty() => Err(format!(
                    "'{id}' is not published on any configured registry ({})",
                    cfg.registries.keys().cloned().collect::<Vec<_>>().join(", ")
                )),
                0 => Err(format!("could not resolve '{id}': {}", errors.join("; "))),
                _ => {
                    let qualified: Vec<String> =
                        hits.iter().map(|h| format!("{}:@{id}", h.registry)).collect();
                    Err(format!(
                        "'{id}' is published on more than one configured registry ({}). \
                         Say which one you mean — this is not a preference, it is how a \
                         pack gets substituted for another with the same name.",
                        qualified.join(" or ")
                    ))
                }
            }
        }
    }
}

/// Whether the host's trust level permits installing this without a further say-so.
///
/// `verified-only` is the interesting one: a host anyone can publish to is exactly
/// where "the host vouches for this" has to mean something before an agent's prompts
/// start running on the operator's credentials.
pub fn trust_permits(r: &Resolved, allow_unverified: bool) -> Result<(), String> {
    match r.trust {
        Trust::FirstParty => Ok(()),
        Trust::Explicit => Ok(()),
        Trust::VerifiedOnly if r.verified || allow_unverified => Ok(()),
        Trust::VerifiedOnly => Err(format!(
            "'{}' is not verified on {}, which this pod is configured to require. \
             Install it explicitly if you know who wrote it.",
            r.id, r.registry
        )),
    }
}

fn refuse_origin(cfg: &Registries) -> String {
    format!(
        "this pod will not download from that origin. Configured registries: {}. \
         Edit <data>/registries.json or set AGENT_PACK_REGISTRIES, or upload the \
         .agentpack directly.",
        cfg.origins().join(", ")
    )
}

/// Download an agent pack archive.
///
/// Returns the raw bytes; nothing is trusted about them here. Redirects are not
/// followed: a redirect is how a configured origin would otherwise be used to reach
/// one that isn't.
pub async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let cfg = load();
    let Some((_, reg)) = cfg.owning(url) else {
        return Err(refuse_origin(&cfg));
    };

    let mut req = client().await?.get(url);
    if let Some(key) = &reg.token_key
        && let Some(token) = crate::key_store::lookup(key)
    {
        req = req.bearer_auth(token);
    }

    let resp = req.send().await.map_err(|e| format!("download failed: {e}"))?;
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

    /// The configuration is read from the environment and the data dir, both
    /// process-global, so these tests must not run concurrently — cargo threads them
    /// by default and they would clobber one another.
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

    /// Read the configuration with no override set, under the same lock.
    fn default_origins() -> Vec<String> {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("AGENT_PACK_REGISTRIES") };
        Registries::default().origins()
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
    fn both_first_party_hosts_are_configured_by_default() {
        // A pod that has never been configured can still install from the social
        // host *and* from the ecosystem host. Shipping only one of them was why
        // installing from axoniac needed an environment variable.
        let origins = default_origins();
        assert!(origins.iter().any(|o| o.contains("axoniac.com")), "{origins:?}");
        assert!(origins.iter().any(|o| o.contains("packs.metalcraftai.com")), "{origins:?}");
    }

    #[test]
    fn the_default_registry_is_the_social_one() {
        let cfg = Registries::default();
        assert_eq!(cfg.default, "axoniac");
        assert_eq!(cfg.get("axoniac").map(|r| r.trust), Some(Trust::VerifiedOnly));
        assert_eq!(cfg.get("metalcraft").map(|r| r.trust), Some(Trust::FirstParty));
    }

    #[test]
    fn references_parse_into_their_three_shapes() {
        assert_eq!(
            parse_reference("@amy_kitchen").unwrap(),
            Reference::Bare { id: "amy_kitchen".into() }
        );
        assert_eq!(
            parse_reference("amy_kitchen").unwrap(),
            Reference::Bare { id: "amy_kitchen".into() },
            "the @ is decoration, not syntax"
        );
        assert_eq!(
            parse_reference("axoniac:@amy_kitchen").unwrap(),
            Reference::Qualified { registry: "axoniac".into(), id: "amy_kitchen".into() }
        );
        assert_eq!(
            parse_reference("https://axoniac.com/x").unwrap(),
            Reference::Url("https://axoniac.com/x".into()),
            "a URL is a URL even though it contains a colon"
        );
        assert!(parse_reference("").is_err());
        assert!(parse_reference("axoniac:").is_err());
    }

    #[test]
    fn a_verified_only_host_refuses_an_unverified_pack() {
        let unverified = Resolved {
            registry: "axoniac".into(),
            id: "amy_kitchen".into(),
            version: "1.0.0".into(),
            content_sha256: String::new(),
            download_url: String::new(),
            verified: false,
            trust: Trust::VerifiedOnly,
        };
        assert!(trust_permits(&unverified, false).is_err());
        assert!(
            trust_permits(&unverified, true).is_ok(),
            "an operator who says so anyway is allowed to; they are the one being asked"
        );

        let verified = Resolved { verified: true, ..unverified.clone() };
        assert!(trust_permits(&verified, false).is_ok());

        let first_party = Resolved { trust: Trust::FirstParty, ..unverified };
        assert!(
            trust_permits(&first_party, false).is_ok(),
            "a first-party host's packs are not gated on a verified flag it never sets"
        );
    }
}
