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
        // `packs.metalcraftai.com` was here too, and it does not serve agent packs:
        // it serves *integration* packs, at `/api/v1/packs/*`, which is a different
        // unit reached through [`crate::registry`]. Every agent-pack call to it 404s.
        // A configured host that cannot answer is worse than an absent one — it puts
        // a tab in front of someone that can only ever say "this host has nothing",
        // and it makes an id ambiguity check ask a host that has no opinion. Add it
        // back the day it implements §11.1.
        Self {
            default: "axoniac".to_string(),
            registries,
        }
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
        self.registries
            .values()
            .map(|r| r.url.trim_end_matches('/').to_string())
            .collect()
    }
}

fn config_file() -> std::path::PathBuf {
    crate::paths::data_dir().join("registries.json")
}

/// The `AGENT_PACK_REGISTRIES` override, if it is set to anything. Read in one place
/// because two questions depend on it: what [`load`] returns, and whether writing the
/// config file would mean anything (it would not — the override replaces it wholesale).
fn env_override() -> Option<String> {
    std::env::var("AGENT_PACK_REGISTRIES")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// The configured registries.
///
/// Precedence: `AGENT_PACK_REGISTRIES` (a bare comma-separated origin list, kept for
/// dev and for pointing a pod at a local registry), then `<data>/registries.json`,
/// then the built-in defaults. The env override wins because it is the thing someone
/// reaches for when the config file is exactly what they are trying to bypass.
pub fn load() -> Registries {
    if let Some(v) = env_override() {
        let mut registries = BTreeMap::new();
        for (i, url) in v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .enumerate()
        {
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
        return Registries {
            default,
            registries,
        };
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
            return Err(format!(
                "'{raw}' is not a valid reference; try 'axoniac:@amy_kitchen'"
            ));
        }
        return Ok(Reference::Qualified {
            registry: registry.to_string(),
            id: id.to_string(),
        });
    }
    Ok(Reference::Bare {
        id: raw.trim_start_matches('@').to_string(),
    })
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
async fn ask_version(name: &str, reg: &Registry, id: &str) -> Result<Option<Resolved>, String> {
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
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("{name}: unreadable response: {e}"))?;

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
        verified: body
            .get("verified")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
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
            let id = pack_id(&id).map_err(|e| e.to_string())?;
            let Some(reg) = cfg.get(&registry) else {
                return Err(format!(
                    "no registry named '{registry}' is configured. Configured: {}.",
                    cfg.registries
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            ask_version(&registry, reg, &id)
                .await?
                .ok_or_else(|| format!("'{id}' is not published on {registry}"))
        }
        Reference::Bare { id } => {
            let id = pack_id(&id).map_err(|e| e.to_string())?;
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
                    cfg.registries
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                0 => Err(format!("could not resolve '{id}': {}", errors.join("; "))),
                _ => {
                    let qualified: Vec<String> = hits
                        .iter()
                        .map(|h| format!("{}:@{id}", h.registry))
                        .collect();
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

    let resp = req
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;
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
    if resp
        .content_length()
        .is_some_and(|l| l as usize > MAX_DOWNLOAD_BYTES)
    {
        return Err("that agent pack is too large".to_string());
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("reading response: {e}"))?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err("that agent pack is too large".to_string());
    }
    Ok(bytes.to_vec())
}

// ── connection: who this pod is to a registry ────────────────────────────────
//
// The four protocol endpoints are anonymous for a public pack, so browsing and
// installing one need no credential at all. What a credential buys is the rest: a
// private pack, and a host able to answer "which account is this pod?". Every pod
// already holds exactly one such credential — the Metalcraft ID token the control
// plane injects — so connecting is *pointing a registry at it*. Nothing is minted,
// no secret moves, and there is no second account to keep in step.

/// The key-store entry holding this pod's Metalcraft ID token. Platform-managed
/// (`key_store::ENV_AUTHORITATIVE`), so the injected value always wins over a stale
/// one somebody pasted.
pub const POD_TOKEN_KEY: &str = "METALCRAFT_TOKEN";

/// How far a registry connection got, in the only terms a UI can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionState {
    /// The host resolved this pod's token to an account there.
    Connected,
    /// The token is good and no account on the host claims it yet. The one state a
    /// button can fix, which is why it is not folded into [`Self::Rejected`] —
    /// `link_url` is where that button goes.
    Unlinked,
    /// This pod sends no credential to this host. Public packs still install.
    NoToken,
    /// The host refused the token: expired, revoked, or from another ecosystem.
    Rejected,
    /// The host serves packs and has no identity endpoint. Nothing is wrong — §11.1
    /// is four endpoints and none of them is `whoami`; there is simply nothing here
    /// to connect.
    Unsupported,
    /// We could not ask.
    Unreachable,
}

/// What this pod is to one registry.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Connection {
    pub registry: String,
    pub url: String,
    pub trust: Trust,
    /// The key-store entry this registry draws its bearer from — the name, never the
    /// value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_key: Option<String>,
    pub state: ConnectionState,
    /// Where a human goes to finish the connection. Taken from the host's own answer
    /// rather than assembled here: a URL we guessed is a URL we would still be sending
    /// people to after the host moved it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    /// Whatever the host will say about the account — an email, usually.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The host's own words when something went wrong, for a UI to show verbatim
    /// instead of inventing an explanation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Whether `name` is a registry this pod knows about — the difference between a typo
/// and a host that is having a bad day.
pub fn configured(name: &str) -> bool {
    load().get(name).is_some()
}

/// Why a registry call produced no answer.
///
/// The distinctions here are the ones a caller acts on differently — a typo in a
/// registry name, a malformed reference, a host that does not have the pack, and a
/// host having a bad day are four different conversations. They were briefly one
/// `String`, and the HTTP layer had to sniff the message text to pick a status code,
/// which is a coupling that breaks the first time someone rewords an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// This pod has no registry by that name.
    Unknown(String),
    /// The caller's reference cannot be used.
    BadReference(String),
    /// The host does not publish it — or will not admit it does, which §11.1 requires
    /// it to make indistinguishable.
    NotFound(String),
    /// The host serves packs but not this part of the protocol.
    Unsupported(String),
    /// The host answered badly, or not at all.
    Host(String),
    /// The configuration is not ours to write.
    Locked(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::Unknown(m)
            | Self::BadReference(m)
            | Self::NotFound(m)
            | Self::Unsupported(m)
            | Self::Host(m)
            | Self::Locked(m) => m,
        };
        f.write_str(msg)
    }
}

fn unknown_registry(cfg: &Registries, name: &str) -> RegistryError {
    RegistryError::Unknown(format!(
        "no registry named '{name}' is configured. Configured: {}.",
        cfg.registries
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The bearer this registry is configured to send, if the entry names one and the key
/// store actually holds it.
fn token_for(reg: &Registry) -> Option<String> {
    reg.token_key
        .as_deref()
        .and_then(crate::key_store::lookup)
        .filter(|t| !t.trim().is_empty())
}

/// Ask a registry who this pod is.
///
/// Every failure is a `Connection`, not an `Err`: "the host is down" and "you are not
/// linked yet" are things a settings panel *renders*, and turning them into errors
/// would make the panel unable to tell them apart from a typo in the registry name —
/// which is the one case that really is an error.
pub async fn status(name: &str) -> Result<Connection, RegistryError> {
    let cfg = load();
    let reg = cfg.get(name).ok_or_else(|| unknown_registry(&cfg, name))?;
    let mut conn = Connection {
        registry: name.to_string(),
        url: reg.url.clone(),
        trust: reg.trust,
        token_key: reg.token_key.clone(),
        state: ConnectionState::NoToken,
        link_url: None,
        account: None,
        detail: None,
    };

    let Some(token) = token_for(reg) else {
        // Either no key is named or the store does not hold it. Both mean one thing to
        // a caller: this pod is anonymous here.
        return Ok(conn);
    };

    let url = format!("{}/api/v1/whoami", reg.url.trim_end_matches('/'));
    let http = client().await.map_err(RegistryError::Host)?;
    let resp = match http.get(&url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(e) => {
            conn.state = ConnectionState::Unreachable;
            conn.detail = Some(e.to_string());
            return Ok(conn);
        }
    };
    let code = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    conn.detail = body
        .get("error")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    conn.link_url = body
        .get("link_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    conn.account = body
        .get("email")
        .or_else(|| body.get("account"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    conn.state = match code {
        c if c.is_success() => ConnectionState::Connected,
        // The structured refusal: the host knows this token and has no account
        // attached to it, and says where to attach one.
        reqwest::StatusCode::FORBIDDEN if conn.link_url.is_some() => ConnectionState::Unlinked,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            ConnectionState::Rejected
        }
        reqwest::StatusCode::NOT_FOUND => ConnectionState::Unsupported,
        c => {
            conn.detail
                .get_or_insert_with(|| format!("{name} answered {c}"));
            ConnectionState::Unreachable
        }
    };
    Ok(conn)
}

/// Start presenting a credential this pod already holds to `name`, and report where
/// that got us. This is the whole of "initialize": one line of config, then the same
/// status probe the panel would have made anyway.
pub async fn connect(name: &str, token_key: &str) -> Result<Connection, RegistryError> {
    set_token_key(name, Some(token_key))?;
    status(name).await
}

/// Stop presenting one. Public packs keep working, which is why this is safe to offer
/// next to the button that turned it on.
pub async fn disconnect(name: &str) -> Result<Connection, RegistryError> {
    set_token_key(name, None)?;
    status(name).await
}

fn set_token_key(name: &str, token_key: Option<&str>) -> Result<(), RegistryError> {
    // `AGENT_PACK_REGISTRIES` replaces the file wholesale, so a write here would be a
    // write `load` never reads. Refuse rather than appear to work — a settings panel
    // that silently does nothing is worse than one that explains itself.
    if env_override().is_some() {
        return Err(RegistryError::Locked(
            "this pod's registries come from AGENT_PACK_REGISTRIES, which replaces the \
             config file. Unset it to manage registries from the workshop."
                .to_string(),
        ));
    }
    let mut cfg = load();
    let Some(reg) = cfg.registries.get_mut(name) else {
        return Err(unknown_registry(&cfg, name));
    };
    reg.token_key = token_key.map(str::to_string);
    save(&cfg).map_err(RegistryError::Host)
}

// ── browse ───────────────────────────────────────────────────────────────────

/// One result from a host's `/search`.
///
/// Everything past the name is optional because a fetch-only host is a real
/// deployment rather than a degenerate one (§11.1 makes `/search` optional in the
/// first place). A browse list renders what it got; it does not refuse what it
/// didn't.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SearchHit {
    /// What to install: `axoniac:@amy_kitchen`. **Qualified, always** — an
    /// unqualified reference is an error the moment two configured hosts publish the
    /// same id (§11.3), and a browse list is precisely where that collision surfaces.
    pub reference: String,
    /// The id on this host, without the `@`.
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Whether the host vouches for it — worth exactly as much as the host's `trust`
    /// makes it worth, and on a `verified-only` host it is what decides whether this
    /// pod will install the pack at all.
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_count: Option<i64>,
}

/// An id we are willing to put in a URL path — §3.1's identifier, exactly.
///
/// An allowlist rather than a list of characters to avoid: a `..` segment is
/// normalised away by any URL parser, turning "show me this pack's manifest" into a
/// call to whatever else the host serves at that path. The spec already says what an
/// id looks like (`^[a-z0-9][a-z0-9_-]{0,63}$`), so requiring it costs nothing real.
pub fn pack_id(id: &str) -> Result<String, RegistryError> {
    let id = id.trim().trim_start_matches('@');
    let shaped = id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !shaped {
        return Err(RegistryError::BadReference(format!(
            "'{id}' is not a valid agent pack id: lowercase letters, digits, '_' and '-', \
             starting with a letter or digit (AGENT_PACK_FORMAT.md §3.1)"
        )));
    }
    Ok(id.to_string())
}

async fn get_json(
    reg: &Registry,
    url: &str,
) -> Result<(reqwest::StatusCode, serde_json::Value), RegistryError> {
    let mut req = client().await.map_err(RegistryError::Host)?.get(url);
    if let Some(token) = token_for(reg) {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| RegistryError::Host(format!("registry request failed: {e}")))?;
    let code = resp.status();
    let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
    Ok((code, body))
}

/// Browse a host. An empty `q` asks for whatever it puts forward, which is a browse
/// list rather than an empty search.
pub async fn search(
    name: &str,
    q: Option<&str>,
    limit: u32,
) -> Result<Vec<SearchHit>, RegistryError> {
    let cfg = load();
    let reg = cfg.get(name).ok_or_else(|| unknown_registry(&cfg, name))?;
    let mut url = reqwest::Url::parse(&format!(
        "{}/api/v1/agent-packs/search",
        reg.url.trim_end_matches('/')
    ))
    .map_err(|e| RegistryError::Host(format!("registry '{name}' has an unusable url: {e}")))?;
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) {
            pairs.append_pair("q", q);
        }
        pairs.append_pair("limit", &limit.clamp(1, 100).to_string());
    }

    let (code, body) = get_json(reg, url.as_str()).await?;
    if code == reqwest::StatusCode::NOT_FOUND || code == reqwest::StatusCode::NOT_IMPLEMENTED {
        // Optional in the protocol, so this is a fact about the host rather than a
        // fault: say what still works instead of showing an error with no way forward.
        return Err(RegistryError::Unsupported(format!(
            "{name} does not offer search. Install by reference instead — '{name}:@handle'."
        )));
    }
    if !code.is_success() {
        return Err(RegistryError::Host(format!(
            "{name} answered {code} to a search"
        )));
    }
    let results = body
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| {
            RegistryError::Host(format!(
                "{name} returned a search body with no results array"
            ))
        })?;
    Ok(results.iter().filter_map(|v| hit_from(name, v)).collect())
}

/// Tell the host an install happened.
///
/// Fire and forget, by construction: the install already succeeded locally, and a
/// registry that is slow, down, or does not implement this must not turn that into a
/// failure the operator sees. Errors are logged at debug and dropped.
///
/// Sent only for a pack that came *from* a registry — an upload or a local path has no
/// host to tell, and an inspect is not an install.
pub async fn report_install(resolved: &Resolved) {
    let cfg = load();
    let Some(reg) = cfg.get(&resolved.registry) else {
        return;
    };
    let Ok(id) = pack_id(&resolved.id) else {
        return;
    };
    let url = format!(
        "{}/api/v1/agent-packs/{id}/installed",
        reg.url.trim_end_matches('/')
    );
    let Ok(client) = client().await else { return };
    let mut req = client.post(&url);
    if let Some(token) = token_for(reg) {
        req = req.bearer_auth(token);
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => log::debug!(
            "{} did not record the install: {}",
            resolved.registry,
            r.status()
        ),
        Err(e) => log::debug!(
            "could not tell {} about the install: {e}",
            resolved.registry
        ),
    }
}

/// What a host says about one pack, without downloading it (§11.1 `/manifest`).
pub async fn manifest(name: &str, id: &str) -> Result<serde_json::Value, RegistryError> {
    let cfg = load();
    let reg = cfg.get(name).ok_or_else(|| unknown_registry(&cfg, name))?;
    let id = pack_id(id)?;
    let url = format!(
        "{}/api/v1/agent-packs/{id}/manifest",
        reg.url.trim_end_matches('/')
    );
    let (code, body) = get_json(reg, &url).await?;
    match code {
        c if c.is_success() => Ok(body),
        // A host must 404 a pack the viewer cannot see rather than 403 it (§11.1), so
        // these are the same answer as far as anyone here is concerned.
        reqwest::StatusCode::NOT_FOUND => Err(RegistryError::NotFound(format!(
            "'{id}' is not published on {name}"
        ))),
        c => Err(RegistryError::Host(format!(
            "{name} answered {c} for '{id}'"
        ))),
    }
}

fn hit_from(registry: &str, v: &serde_json::Value) -> Option<SearchHit> {
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    // §11.1: the reference is the handle where the host has one, falling back to the
    // pack's own id. Taken in that order deliberately — a host whose `id` is a
    // database key has a handle, and one without handles is already naming packs in
    // `id`, so the first field that is a *name* wins.
    let id = field("handle")
        .or_else(|| field("slug"))
        .or_else(|| field("id"))?;
    // Checked here rather than at install time. The id is what the reference is built
    // from, and `pack_id` will refuse a malformed one when somebody presses Install —
    // so listing it would be offering a button that cannot work, with the explanation
    // arriving one click too late.
    let id = pack_id(&id).ok()?;
    let name = field("name").unwrap_or_else(|| id.clone());
    Some(SearchHit {
        reference: format!("{registry}:@{id}"),
        id,
        name,
        version: field("version"),
        tagline: field("tagline").or_else(|| field("description")),
        category: field("category"),
        tags: v
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        avatar_url: field("avatar_url"),
        verified: v.get("verified").and_then(|b| b.as_bool()).unwrap_or(false),
        install_count: v.get("install_count").and_then(|n| n.as_i64()),
    })
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
            assert!(is_allowed(
                "https://axoniac.example/api/v1/agent-packs/amy/download"
            ));
            assert!(
                is_allowed("https://AXONIAC.example/x"),
                "host match is case-insensitive"
            );
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
            assert!(
                !is_allowed("http://axoniac.example/x"),
                "plaintext is a different origin"
            );
        });
        // …and a local dev registry over http works when that is what was configured.
        with_registries("http://localhost:8080", || {
            assert!(is_allowed(
                "http://localhost:8080/api/v1/agent-packs/x/download"
            ));
            assert!(
                !is_allowed("http://localhost:9999/x"),
                "the port is part of the origin"
            );
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
    fn the_agent_pack_host_is_configured_by_default() {
        // A pod that has never been configured can still install an agent pack.
        // Shipping no default at all was why installing from axoniac needed an
        // environment variable.
        let origins = default_origins();
        assert!(
            origins.iter().any(|o| o.contains("axoniac.com")),
            "{origins:?}"
        );
        // `packs.metalcraftai.com` serves *integration* packs and 404s every
        // agent-pack path, so it is not one of these. Configuring a host that cannot
        // answer only produces a tab that says "this host has nothing".
        assert!(
            !origins.iter().any(|o| o.contains("packs.metalcraftai.com")),
            "the integration-pack host is not an agent-pack registry: {origins:?}"
        );
    }

    #[test]
    fn the_default_registry_is_the_social_one() {
        let cfg = Registries::default();
        assert_eq!(cfg.default, "axoniac");
        // `verified-only`, because anyone can publish there. That is the whole reason
        // the trust levels exist, and softening it for the default host would empty
        // them of meaning.
        assert_eq!(
            cfg.get("axoniac").map(|r| r.trust),
            Some(Trust::VerifiedOnly)
        );
    }

    /// A host can put anything in a search result. What it cannot do is make this pod
    /// offer an install button for something it will refuse to fetch.
    #[test]
    fn a_result_this_pod_could_never_install_is_not_listed() {
        assert!(hit_from("acme", &serde_json::json!({ "handle": "../secrets" })).is_none());
        assert!(hit_from("acme", &serde_json::json!({ "id": "Not A Handle" })).is_none());
        assert!(hit_from("acme", &serde_json::json!({ "handle": "helpdesk" })).is_some());
    }

    #[test]
    fn references_parse_into_their_three_shapes() {
        assert_eq!(
            parse_reference("@amy_kitchen").unwrap(),
            Reference::Bare {
                id: "amy_kitchen".into()
            }
        );
        assert_eq!(
            parse_reference("amy_kitchen").unwrap(),
            Reference::Bare {
                id: "amy_kitchen".into()
            },
            "the @ is decoration, not syntax"
        );
        assert_eq!(
            parse_reference("axoniac:@amy_kitchen").unwrap(),
            Reference::Qualified {
                registry: "axoniac".into(),
                id: "amy_kitchen".into()
            }
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

        let verified = Resolved {
            verified: true,
            ..unverified.clone()
        };
        assert!(trust_permits(&verified, false).is_ok());

        let first_party = Resolved {
            trust: Trust::FirstParty,
            ..unverified
        };
        assert!(
            trust_permits(&first_party, false).is_ok(),
            "a first-party host's packs are not gated on a verified flag it never sets"
        );
    }

    /// The id lands in a URL path, so the guard is an allowlist of what §3.1 says an
    /// id is — not a blocklist of what looks dangerous today.
    #[test]
    fn a_pack_id_that_could_leave_its_path_is_refused() {
        assert_eq!(pack_id("amy_kitchen").as_deref(), Ok("amy_kitchen"));
        assert_eq!(
            pack_id("@amy_kitchen").as_deref(),
            Ok("amy_kitchen"),
            "the @ is sugar"
        );
        assert_eq!(pack_id(" mitch-reviews ").as_deref(), Ok("mitch-reviews"));

        for hostile in [
            // Normalised away by any URL parser, landing on another endpoint entirely.
            "../version",
            "..",
            // A second path segment is a second endpoint.
            "amy/../../keys",
            "amy?x=1",
            "amy#frag",
            // Percent-encoding is how the above gets past a naive character check.
            "%2e%2e",
            "amy kitchen",
            "Amy_Kitchen",
            "",
        ] {
            assert!(
                matches!(pack_id(hostile), Err(RegistryError::BadReference(_))),
                "{hostile:?} must not reach a URL path — and it is the caller's mistake"
            );
        }
    }

    /// A browse result is only useful if it can be installed, and §11.3 makes an
    /// unqualified reference an error the moment two hosts publish the same id — so
    /// the reference a listing hands back is qualified, always.
    #[test]
    fn a_search_hit_is_installable_without_ambiguity() {
        let row = serde_json::json!({
            "id": "1f0b6a1e-0000-4000-8000-000000000000",
            "handle": "amy_kitchen",
            "slug": "amy-kitchen",
            "name": "Amy",
            "version": "1.2.0",
            "tagline": "cooks",
            "tags": ["food", "home"],
            "verified": true,
            "install_count": 42,
        });
        let hit = hit_from("axoniac", &row).expect("a row with a handle is a hit");
        assert_eq!(hit.reference, "axoniac:@amy_kitchen");
        assert_eq!(
            hit.id, "amy_kitchen",
            "the handle, not the host's database key"
        );
        assert_eq!(hit.version.as_deref(), Some("1.2.0"));
        assert!(hit.verified);
        assert_eq!(hit.tags, vec!["food", "home"]);

        // A fetch-only host that names packs in `id` and publishes nothing else still
        // renders: everything past the name is optional in the protocol.
        let sparse = serde_json::json!({ "id": "helpdesk", "name": "Helpdesk" });
        let hit = hit_from("acme", &sparse).expect("id alone is enough");
        assert_eq!(hit.reference, "acme:@helpdesk");
        assert_eq!(hit.version, None);
        assert!(
            !hit.verified,
            "a host that says nothing vouches for nothing"
        );

        assert!(
            hit_from("acme", &serde_json::json!({ "name": "no id" })).is_none(),
            "a result nothing can be installed from is not a result"
        );
    }
}
