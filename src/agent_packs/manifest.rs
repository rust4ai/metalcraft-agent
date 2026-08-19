//! The `agent_pack.json` manifest, and the consent summary derived from a pack's
//! contents.
//!
//! The summary is **derived, never author-supplied**. An author writes what their
//! agent *is*; the domains it can reach and the credentials it needs are computed
//! from the integration packs actually inside the archive. Anything a human is asked
//! to approve has to come from the bytes, not from a description of them.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Format version of `agent_pack.json` itself (not the pack's own version).
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Author {
    pub handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The author's Metalcraft ID subject, when the registry recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
}

/// A vendored integration pack, pinned by content.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PackRef {
    pub id: String,
    pub version: String,
    /// Integrity pin for the vendored copy. Verified at install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// Where the author obtained it, for provenance display. Never fetched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One credential, attributed to what needs it — because the question a human is
/// answering at install time is "which secrets do I have to paste in", not "which
/// pack wants what".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EnvRequirement {
    pub name: String,
    #[serde(default)]
    pub needed_by: Vec<String>,
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Provides {
    #[serde(default)]
    pub personas: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub integration_packs: Vec<PackRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Parent {
    pub id: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentPackManifest {
    #[serde(default = "one")]
    pub manifest_version: u32,
    pub id: String,
    /// The registry handle this pack is published under (`amy_kitchen`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,

    /// The agent presets this pack provides. Exactly one today — the registry
    /// enforces it — but an array so multi-preset "crews" stay additive later.
    #[serde(default)]
    pub presets: Vec<String>,
    #[serde(default)]
    pub provides: Provides,

    /// Derived at build time and **re-derived at install**; a manifest that
    /// disagrees with its own contents is rejected rather than trusted.
    #[serde(default)]
    pub requires_env: Vec<EnvRequirement>,
    #[serde(default)]
    pub domains: Vec<String>,

    /// Hash of every file in the archive **except `agent_pack.json`**, so the
    /// manifest can carry the hash of what it describes without hashing itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// Fork lineage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Parent>,
}

fn one() -> u32 {
    1
}

impl AgentPackManifest {
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            manifest_version: MANIFEST_VERSION,
            id: id.into(),
            handle: None,
            name: name.into(),
            description: String::new(),
            version: version.into(),
            license: None,
            author: None,
            category: None,
            tags: Vec::new(),
            presets: Vec::new(),
            provides: Provides::default(),
            requires_env: Vec::new(),
            domains: Vec::new(),
            content_sha256: None,
            parent: None,
        }
    }
}

/// What a human is shown before approving an install.
#[derive(Debug, Clone, Default, PartialEq, Serialize, utoipa::ToSchema)]
pub struct ConsentSummary {
    /// Every origin this agent's tools can reach.
    pub domains: Vec<String>,
    /// Every credential it wants, and what wants it.
    pub requires_env: Vec<EnvRequirement>,
    /// Tool names it gains, so "what can this thing actually do" is answerable.
    pub tools: Vec<String>,
    /// Tools classified as mutating. A read-only agent is a materially smaller
    /// commitment than one that can write, and the dialog should say which it is.
    pub mutating_tools: Vec<String>,
}

/// Derive the consent summary from the vendored integration packs.
///
/// `packs` maps `<pack id> -> (pack.json bytes, [(api_tool file name, bytes)])`.
pub fn derive_consent(
    packs: &BTreeMap<String, (Vec<u8>, Vec<(String, Vec<u8>)>)>,
) -> ConsentSummary {
    let mut domains: BTreeSet<String> = BTreeSet::new();
    let mut tools: BTreeSet<String> = BTreeSet::new();
    let mut mutating: BTreeSet<String> = BTreeSet::new();
    // name -> (needed_by, required)
    let mut env: BTreeMap<String, (BTreeSet<String>, bool)> = BTreeMap::new();

    for (pack_id, (manifest_bytes, api_tools)) in packs {
        if let Ok(manifest) =
            serde_json::from_slice::<metalcraft_packs::PackManifest>(manifest_bytes)
        {
            for key in &manifest.requires_env {
                let e = env.entry(key.clone()).or_insert_with(|| (BTreeSet::new(), true));
                e.0.insert(pack_id.clone());
            }
            for t in &manifest.native_tools {
                tools.insert(t.clone());
            }
        }

        for (file, bytes) in api_tools {
            let Ok(doc) = serde_json::from_slice::<serde_json::Value>(bytes) else {
                continue;
            };
            // The tool's name is authoritative from the file, falling back to the
            // filename — a tool that lies about its own name still can't hide.
            let name = doc
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| file.trim_end_matches(".json").to_string());
            tools.insert(name.clone());

            if let Some(url) = doc.get("url").and_then(|v| v.as_str())
                && let Some(host) = host_of(url)
            {
                domains.insert(host);
            }
            // Anything that isn't a GET can change something on the other end.
            let method = doc
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_ascii_uppercase();
            if method != "GET" {
                mutating.insert(name);
            }
            // `$SECRET` placeholders in headers are credentials too, and they are
            // easy to omit from a manifest's requires_env.
            if let Some(headers) = doc.get("headers").and_then(|v| v.as_object()) {
                for value in headers.values().filter_map(|v| v.as_str()) {
                    for key in env_refs(value) {
                        let e = env.entry(key).or_insert_with(|| (BTreeSet::new(), true));
                        e.0.insert(pack_id.clone());
                    }
                }
            }
        }
    }

    ConsentSummary {
        domains: domains.into_iter().collect(),
        requires_env: env
            .into_iter()
            .map(|(name, (needed_by, required))| EnvRequirement {
                name,
                needed_by: needed_by.into_iter().collect(),
                required,
            })
            .collect(),
        tools: tools.into_iter().collect(),
        mutating_tools: mutating.into_iter().collect(),
    }
}

/// Host of a URL, ignoring `{placeholder}` path segments.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?']).next()?;
    let host = host.split('@').next_back()?;
    let host = host.split(':').next()?;
    if host.is_empty() || host.contains('{') {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// `$NAME` references inside a header value.
fn env_refs(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
            {
                end += 1;
            }
            if end > start {
                out.push(value[start..end].to_string());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(id: &str, requires: &[&str], tools: &[(&str, &str, &str)]) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
        let manifest = serde_json::json!({
            "id": id, "name": id, "description": "t", "version": "1.0.0",
            "requires_env": requires,
        });
        let files = tools
            .iter()
            .map(|(name, method, url)| {
                let doc = serde_json::json!({
                    "name": name, "method": method, "url": url,
                    "headers": { "Authorization": "Bearer $ACME_TOKEN" },
                });
                (format!("{name}.json"), serde_json::to_vec(&doc).unwrap())
            })
            .collect();
        (serde_json::to_vec(&manifest).unwrap(), files)
    }

    #[test]
    fn consent_is_derived_from_the_tools_themselves() {
        let mut packs = BTreeMap::new();
        packs.insert(
            "calendar".to_string(),
            pack(
                "calendar",
                &["METALCRAFT_TOKEN"],
                &[
                    ("mcal_list", "GET", "https://calendar.metalcraftai.com/api/v1/calendars"),
                    ("mcal_create", "POST", "https://calendar.metalcraftai.com/api/v1/events"),
                ],
            ),
        );
        packs.insert(
            "instacart".to_string(),
            pack("instacart", &[], &[("ic_order", "POST", "https://api.instacart.com/v2/orders")]),
        );

        let c = derive_consent(&packs);
        assert_eq!(c.domains, vec!["api.instacart.com", "calendar.metalcraftai.com"]);
        assert_eq!(c.tools, vec!["ic_order", "mcal_create", "mcal_list"]);
        assert_eq!(
            c.mutating_tools,
            vec!["ic_order", "mcal_create"],
            "a GET is not a commitment; a POST is"
        );

        // The manifest's declared key AND the `$ACME_TOKEN` hiding in a header.
        let names: Vec<&str> = c.requires_env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["ACME_TOKEN", "METALCRAFT_TOKEN"]);
        let acme = c.requires_env.iter().find(|e| e.name == "ACME_TOKEN").unwrap();
        assert_eq!(
            acme.needed_by,
            vec!["calendar", "instacart"],
            "a credential is attributed to every pack that reaches for it"
        );
    }

    #[test]
    fn hosts_are_extracted_and_placeholders_ignored() {
        assert_eq!(host_of("https://api.github.com/repos/x"), Some("api.github.com".into()));
        assert_eq!(host_of("http://user:pw@example.com:8080/x"), Some("example.com".into()));
        assert_eq!(host_of("https://{region}.example.com/x"), None, "a templated host is not a promise");
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn env_refs_finds_every_placeholder() {
        assert_eq!(env_refs("Bearer $GITHUB_TOKEN"), vec!["GITHUB_TOKEN"]);
        assert_eq!(env_refs("$A and $B_2"), vec!["A", "B_2"]);
        assert!(env_refs("no placeholders").is_empty());
        assert!(env_refs("$").is_empty());
    }

    #[test]
    fn a_manifest_round_trips() {
        let m = AgentPackManifest::new("amy-kitchen-agent", "Amy's Kitchen Agent", "1.4.0");
        let json = serde_json::to_string(&m).unwrap();
        let back: AgentPackManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "amy-kitchen-agent");
        assert_eq!(back.manifest_version, MANIFEST_VERSION);
    }
}
