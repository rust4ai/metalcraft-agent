//! Reading, verifying and writing `.agentpack` archives.
//!
//! ```text
//! amy-kitchen-agent-1.4.0.agentpack        (zip)
//!   agent_pack.json
//!   agent_presets/<slug>.json
//!   agent_presets/<slug>/memories.jsonl
//!   personas/<slug>.json
//!   skills/<slug>.md
//!   integration_packs/<id>/{pack.json, api_tools/*.json, README.md}
//! ```
//!
//! **Self-contained by construction.** Every persona a preset names, every skill
//! those personas load, and every integration pack they call is in the archive — so
//! installing needs no network at all, and there is no thin/fat variant to reason
//! about. A pack that does not carry its dependencies is not valid, and §`validate`
//! is where that stops being a promise.
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use super::manifest::{AgentPackManifest, ConsentSummary, derive_consent};

/// Assets and authored memories are bigger than a pack's JSON, but an archive is
/// still small. This is a zip-bomb guard, not a feature budget.
pub const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;

/// Seed memories per preset (`specs/AGENT_PACK_FORMAT.md` §6). A registry refuses
/// past this; a pod warns, because the operator already has the bytes and a noisier
/// agent beats a failed install.
pub const MAX_SEED_MEMORIES: usize = 5_000;

/// An archive read into memory and structurally checked, but not yet written
/// anywhere. Nothing touches disk until verification passes.
#[derive(Debug)]
pub struct Bundle {
    pub manifest: AgentPackManifest,
    /// Every file except `agent_pack.json`, keyed by its archive path.
    pub files: BTreeMap<String, Vec<u8>>,
    pub consent: ConsentSummary,
}

impl Bundle {
    /// Parse and verify an archive.
    ///
    /// Order matters: unpack with guards, hash-check, *then* validate structure. A
    /// tampered archive must be rejected before its contents are interpreted at all.
    pub fn read(bytes: &[u8]) -> Result<Self, String> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("not a valid .agentpack (zip): {e}"))?;

        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut manifest_bytes: Option<Vec<u8>> = None;
        let mut total: u64 = 0;

        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| format!("reading zip entry: {e}"))?;
            if entry.is_dir() {
                continue;
            }
            let raw = entry.name().replace('\\', "/");
            if !is_safe_path(&raw) {
                return Err(format!("unsafe path in archive: {}", entry.name()));
            }
            // `entry.size()` is the size the *archive declares*, which whoever built
            // it controls independently of the actual stream — the decompressor is
            // bounded by `compressed_size`, not by this. So the budget is spent
            // against bytes actually read, and the declared size only avoids a
            // pointless reallocation. Trusting the declaration let a small archive
            // inflate without limit: `?path=` and `?url=` both reach here having
            // bypassed any HTTP body cap.
            let remaining = MAX_BUNDLE_BYTES.saturating_sub(total);
            let mut buf =
                Vec::with_capacity(entry.size().min(remaining).min(1 << 20) as usize);
            // One byte past the budget, so an over-long entry is detected rather than
            // silently truncated into something that then fails its hash check.
            std::io::Read::take(&mut entry, remaining + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("reading {raw}: {e}"))?;
            if buf.len() as u64 > remaining {
                return Err("archive exceeds the maximum allowed size".to_string());
            }
            total = total.saturating_add(buf.len() as u64);
            if raw == "agent_pack.json" {
                manifest_bytes = Some(buf);
            } else {
                files.insert(raw, buf);
            }
        }

        let manifest_bytes =
            manifest_bytes.ok_or_else(|| "archive has no top-level agent_pack.json".to_string())?;
        let manifest: AgentPackManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("invalid agent_pack.json: {e}"))?;

        if manifest.manifest_version != super::manifest::MANIFEST_VERSION {
            return Err(format!(
                "agent_pack.json is manifest_version {}; this agent understands {}",
                manifest.manifest_version,
                super::manifest::MANIFEST_VERSION
            ));
        }
        if !valid_id(&manifest.id) {
            return Err(format!("invalid agent pack id '{}'", manifest.id));
        }

        // Integrity: the manifest carries the hash of everything but itself.
        if let Some(expected) = &manifest.content_sha256 {
            let actual = content_hash(&files);
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "content hash {actual} does not match the {expected} declared in agent_pack.json"
                ));
            }
        }

        let consent = derive_consent(&collect_packs(&files));
        let bundle = Bundle { manifest, files, consent };
        bundle.validate()?;
        Ok(bundle)
    }

    /// Everything the archive claims must actually be in it, and nothing may reach
    /// outside what its preset declared.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems: Vec<String> = Vec::new();

        if self.manifest.presets.is_empty() {
            problems.push("declares no agent presets".into());
        }
        if self.manifest.presets.len() > 1 {
            // One preset per agent pack: the page, the pack and the agent are the
            // same thing. Multi-preset "crews" would be an additive change later.
            problems.push(format!(
                "declares {} presets; exactly one is supported",
                self.manifest.presets.len()
            ));
        }

        let pack_ids: Vec<String> = collect_packs(&self.files).keys().cloned().collect();

        for slug in &self.manifest.presets {
            let path = format!("agent_presets/{slug}.json");
            let Some(raw) = self.files.get(&path) else {
                problems.push(format!("preset '{slug}' is declared but missing from the archive"));
                continue;
            };
            let preset: crate::agent_preset::AgentPreset = match serde_json::from_slice(raw) {
                Ok(p) => p,
                Err(e) => {
                    problems.push(format!("preset '{slug}' is unreadable: {e}"));
                    continue;
                }
            };
            if let Err(e) = preset.validate() {
                problems.push(e);
            }

            for p in preset.callable_personas() {
                let key = format!("personas/{p}.json");
                if !self.files.contains_key(&key) {
                    problems.push(format!(
                        "preset '{slug}' names persona '{p}', which the archive does not carry"
                    ));
                    continue;
                }
                // Containment: a persona may only reach packs its preset declared.
                // Cheap to check, and it is what makes the consent summary complete.
                if let Ok(persona) =
                    serde_json::from_slice::<crate::persona::Persona>(&self.files[&key])
                {
                    for pack in &persona.packs {
                        if !preset.integration_packs.contains(pack) {
                            problems.push(format!(
                                "persona '{p}' uses integration pack '{pack}', which preset '{slug}' does not declare"
                            ));
                        }
                    }
                }
            }

            // Every skill the preset declares, *and* every skill its personas
            // actually load — `load_skill`'s enum comes from the persona, so a
            // missing one is a runtime failure on the installing pod.
            let mut wanted: Vec<String> = preset.skills.clone();
            for p in preset.callable_personas() {
                if let Some(raw) = self.files.get(&format!("personas/{p}.json"))
                    && let Ok(persona) = serde_json::from_slice::<crate::persona::Persona>(raw)
                {
                    for s in persona.skills {
                        if !wanted.contains(&s) {
                            wanted.push(s);
                        }
                    }
                }
            }
            for s in &wanted {
                if !self.files.contains_key(&format!("skills/{s}.md")) {
                    problems.push(format!(
                        "preset '{slug}' needs skill '{s}', which the archive does not carry"
                    ));
                }
            }
            for pack in &preset.integration_packs {
                if !pack_ids.contains(pack) {
                    problems.push(format!(
                        "preset '{slug}' requires integration pack '{pack}', which the archive does not vendor"
                    ));
                }
            }

            // A shipped flow is background work, so the containment rule that scopes
            // `sub_agent` has to reach it too: a flow may only name personas from the
            // preset that owns it. Without this the consent summary shown before
            // arming could not be complete, because the graph could reach anywhere.
            let roster = preset.callable_personas();
            for (path, raw) in self.files.iter().filter(|(p, _)| p.starts_with("flows/")) {
                let Ok(flow) = serde_json::from_slice::<serde_json::Value>(raw) else {
                    problems.push(format!("flow '{path}' is unreadable"));
                    continue;
                };
                for p in flow_personas(&flow) {
                    if !roster.contains(&p) {
                        problems.push(format!(
                            "flow '{path}' names persona '{p}', which preset '{slug}' does not \
                             include (roster: {})",
                            roster.join(", ")
                        ));
                    }
                }
            }

            // Seed memories are capped so a corpus stays reviewable and its base
            // stays cheap to build. A pod warns rather than refusing: the operator
            // already has the bytes, and a smaller agent beats a failed install.
            if let Some(raw) = self.files.get(&format!("agent_presets/{slug}/memories.jsonl")) {
                let count = String::from_utf8_lossy(raw).lines().filter(|l| !l.trim().is_empty()).count();
                if count > MAX_SEED_MEMORIES {
                    log::warn!(
                        "agent pack '{}': preset '{slug}' ships {count} seed memories, above the \
                         {MAX_SEED_MEMORIES} limit — the excess will be indexed but the registry \
                         that served this should have refused it",
                        self.manifest.id
                    );
                }
            }
        }

        // Vendored pack hashes are pins, not decoration.
        let packs = collect_pack_files(&self.files);
        for r in &self.manifest.provides.integration_packs {
            let Some(files) = packs.get(&r.id) else {
                problems.push(format!("manifest lists integration pack '{}', which is absent", r.id));
                continue;
            };
            if let Some(expected) = &r.content_sha256 {
                let actual = metalcraft_packs::canonical_sha256(
                    files.iter().map(|(p, c)| (p.as_str(), c.as_slice())),
                );
                if !actual.eq_ignore_ascii_case(expected) {
                    problems.push(format!(
                        "vendored pack '{}' hashes to {actual}, not the declared {expected}",
                        r.id
                    ));
                }
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!("invalid agent pack:\n  - {}", problems.join("\n  - ")))
        }
    }

    pub fn preset_slug(&self) -> Option<&str> {
        self.manifest.presets.first().map(String::as_str)
    }
}

/// Every persona a flow document names: the flow-level default, each node's
/// `data.persona`, and each schedule's `persona`.
///
/// Read from JSON rather than from `SavedFlow` on purpose — this runs during
/// validation, before we have committed to the document being a flow we can parse,
/// and an unknown node shape should contribute nothing rather than fail the install.
/// The rule is "nothing outside the roster"; anything this misses, `arm()` catches.
pub fn flow_personas(flow: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: Option<&Value>| {
        if let Some(s) = v.and_then(Value::as_str)
            && !s.is_empty()
            && !out.iter().any(|p| p == s)
        {
            out.push(s.to_string());
        }
    };

    push(flow.get("persona"));
    for s in flow.get("schedules").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        push(s.get("persona"));
    }
    let nodes = flow
        .get("flow")
        .and_then(|f| f.get("nodes"))
        .or_else(|| flow.get("nodes"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for n in nodes {
        push(n.get("data").and_then(|d| d.get("persona")));
    }
    out
}

/// Hash of every file except the manifest — what `content_sha256` pins.
pub fn content_hash(files: &BTreeMap<String, Vec<u8>>) -> String {
    metalcraft_packs::canonical_sha256(files.iter().map(|(p, c)| (p.as_str(), c.as_slice())))
}

/// `<pack id> -> (pack.json, [(api tool file, bytes)])`, for consent derivation.
fn collect_packs(
    files: &BTreeMap<String, Vec<u8>>,
) -> BTreeMap<String, (Vec<u8>, Vec<(String, Vec<u8>)>)> {
    let mut out: BTreeMap<String, (Vec<u8>, Vec<(String, Vec<u8>)>)> = BTreeMap::new();
    for (path, bytes) in files {
        let Some(rest) = path.strip_prefix("integration_packs/") else { continue };
        let Some((id, tail)) = rest.split_once('/') else { continue };
        let entry = out.entry(id.to_string()).or_default();
        if tail == "pack.json" {
            entry.0 = bytes.clone();
        } else if let Some(file) = tail.strip_prefix("api_tools/") {
            entry.1.push((file.to_string(), bytes.clone()));
        }
    }
    out
}

/// `<pack id> -> {relative path -> bytes}`, for hashing and storing.
pub fn collect_pack_files(
    files: &BTreeMap<String, Vec<u8>>,
) -> BTreeMap<String, BTreeMap<String, Vec<u8>>> {
    let mut out: BTreeMap<String, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
    for (path, bytes) in files {
        let Some(rest) = path.strip_prefix("integration_packs/") else { continue };
        let Some((id, tail)) = rest.split_once('/') else { continue };
        out.entry(id.to_string()).or_default().insert(tail.to_string(), bytes.clone());
    }
    out
}

/// Build an archive from a file map, computing and embedding the content hash.
pub fn write(
    mut manifest: AgentPackManifest,
    files: BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>, String> {
    manifest.content_sha256 = Some(content_hash(&files));
    manifest.manifest_version = super::manifest::MANIFEST_VERSION;

    // Regenerate the consent summary rather than trusting whatever was passed in:
    // a manifest that disagrees with its own contents would be rejected at install,
    // so it must not be possible to build one here.
    let consent = derive_consent(&collect_packs(&files));
    manifest.domains = consent.domains.clone();
    manifest.requires_env = consent.requires_env.clone();

    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        let manifest_json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| format!("serializing agent_pack.json: {e}"))?;
        zip.start_file("agent_pack.json", opts)
            .map_err(|e| format!("writing agent_pack.json: {e}"))?;
        zip.write_all(&manifest_json).map_err(|e| format!("writing agent_pack.json: {e}"))?;

        for (path, bytes) in &files {
            zip.start_file(path.as_str(), opts).map_err(|e| format!("writing {path}: {e}"))?;
            zip.write_all(bytes).map_err(|e| format!("writing {path}: {e}"))?;
        }
        zip.finish().map_err(|e| format!("finalizing archive: {e}"))?;
    }
    Ok(buf)
}

fn is_safe_path(raw: &str) -> bool {
    use std::path::{Component, PathBuf};
    if raw.starts_with('/') || raw.is_empty() {
        return false;
    }
    !PathBuf::from(raw)
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_traversal_is_refused() {
        assert!(is_safe_path("personas/amy.json"));
        assert!(!is_safe_path("../escape.json"));
        assert!(!is_safe_path("/etc/passwd"));
        assert!(!is_safe_path("a/../../b"));
        assert!(!is_safe_path(""));
    }

    #[test]
    fn ids_are_constrained() {
        assert!(valid_id("amy-kitchen-agent"));
        assert!(valid_id("pack_1"));
        assert!(!valid_id("Amy"), "uppercase would collide on case-insensitive filesystems");
        assert!(!valid_id("../x"));
        assert!(!valid_id(""));
    }

    #[test]
    fn the_manifest_is_excluded_from_its_own_hash() {
        let mut files = BTreeMap::new();
        files.insert("personas/amy.json".to_string(), b"{}".to_vec());
        let a = content_hash(&files);
        // Adding the manifest to the map would change the hash — which is exactly
        // why `read` keeps it out of `files`.
        files.insert("agent_pack.json".to_string(), b"{}".to_vec());
        assert_ne!(a, content_hash(&files));
    }
}
