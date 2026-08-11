use crate::paths;
use include_dir::{include_dir, Dir};
use std::fs;
use std::path::{Path, PathBuf};

/// Every seed file — default personas, skills, flows, flow templates, and
/// integration packs — embedded into the binary at compile time from the
/// `seed/` directory, then written to the app data dir on startup by
/// [`ensure_defaults`]. The released binary is therefore self-contained: it
/// carries its seeds and needs no `seed/` folder shipped alongside it.
///
/// Adding a persona, skill, or whole integration pack is just dropping files
/// under `seed/` — no edit to this file is needed.
///
/// Layout (top-level subdirs map to data dirs; `integration_packs/<id>/` is a
/// pack tree):
/// ```text
/// seed/
///   personas/*.json          -> versioned upgrade (see write_versioned_seeds)
///   skills/*.md              -> write-if-missing
///   flows/*                  -> write-if-missing
///   api_tools/*              -> write-if-missing
///   flow_templates/*         -> write-if-missing
///   integration_packs/<id>/  -> pack-version-gated (see write_integration_packs)
/// ```
///
/// Caveat: `include_dir` re-embeds when the *contents* of already-tracked files
/// change. If you ADD a brand-new file and a stale build doesn't pick it up,
/// force a rebuild (`touch src/seed.rs` or `cargo clean -p metalcraft-agent`).
static SEED: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/seed");

/// Immediate files of an embedded top-level subdir as `(file_name, contents)`
/// pairs. Used for the flat seed dirs (personas, skills, ...); returns empty if
/// the subdir isn't embedded (e.g. `flows/` doesn't exist).
fn embedded_flat(subdir: &str) -> Vec<(String, String)> {
    let Some(dir) = SEED.get_dir(subdir) else {
        return Vec::new();
    };
    dir.files()
        .filter_map(|f| {
            let name = f.path().file_name()?.to_str()?.to_string();
            let content = f.contents_utf8()?.to_string();
            Some((name, content))
        })
        .collect()
}

/// Borrow an owned `(name, content)` list as the `&[(&str, &str)]` shape the
/// seed writers take.
fn as_refs(v: &[(String, String)]) -> Vec<(&str, &str)> {
    v.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
}

/// Ensure default personas, skills, and integration packs exist in the app data
/// directory. Creates the data dirs, then writes the embedded seed files
/// (personas upgrade on version bump; everything else is written only when
/// missing — packs gate on their own `pack.json` version).
pub fn ensure_defaults() {
    let dirs = [
        paths::personas_dir(),
        paths::skills_dir(),
        paths::flows_dir(),
        paths::sessions_dir(),
        paths::api_tools_dir(),
        paths::flow_templates_dir(),
        paths::chats_dir(),
        paths::integration_packs_dir(),
        paths::upload_root(),
    ];

    for dir in &dirs {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("Warning: could not create {}: {e}", dir.display());
        }
    }

    // Personas re-seed on a version bump (how a built-in prompt change reaches
    // existing installs); everything else is write-if-missing.
    let personas = embedded_flat("personas");
    write_versioned_seeds(&paths::personas_dir(), &as_refs(&personas));

    for (subdir, target) in [
        ("skills", paths::skills_dir()),
        ("flows", paths::flows_dir()),
        ("api_tools", paths::api_tools_dir()),
        ("flow_templates", paths::flow_templates_dir()),
    ] {
        let seeds = embedded_flat(subdir);
        write_seeds(&target, &as_refs(&seeds));
    }

    write_integration_packs();

    retire_obsolete_seeds();
}

/// Remove seeds that shipped in older versions but have since moved or been
/// replaced, so they don't linger as stale, enable-able items on upgraded
/// installs.
fn retire_obsolete_seeds() {
    // The `whatsapp` integration pack became the native, generic
    // `gateway_send_message` tool.
    retire_dir(paths::integration_packs_dir().join("whatsapp"), "'whatsapp' integration pack");
    // The channel *type/instance* model was replaced by the simple channels
    // connection model (channels.json); drop the seeded manifest tree so old
    // channel types stop lingering on upgraded installs.
    retire_dir(paths::gateway_channels_dir(), "gateway channel type manifests");
}

fn retire_dir(dir: PathBuf, label: &str) {
    if dir.is_dir() {
        if let Err(e) = fs::remove_dir_all(&dir) {
            eprintln!("Warning: could not remove retired {label} at {}: {e}", dir.display());
        } else {
            log::info!("Retired obsolete {label}");
        }
    }
}

fn write_seeds(dir: &Path, seeds: &[(&str, &str)]) {
    for (filename, content) in seeds {
        let target = dir.join(filename);
        if !target.exists() {
            if let Err(e) = fs::write(&target, content) {
                eprintln!("Warning: could not write {}: {e}", target.display());
            }
        }
    }
}

/// Write seed files, force-overwriting any whose bundled `version` is higher
/// than the installed copy's. A missing installed version counts as 0.0.0, so
/// a versioned seed reaches installs that predate the version field. A seed
/// with no bundled `version` is only written when missing (like [`write_seeds`]).
///
/// This is how a prompt change to a built-in persona reaches existing data
/// dirs — bump its `version` and it re-seeds on next start. The trade-off
/// (shared with integration packs) is that this clobbers user edits to a
/// built-in persona on a version bump; customizations should be saved under a
/// new slug, which is not a seeded persona and so is never touched.
fn write_versioned_seeds(dir: &Path, seeds: &[(&str, &str)]) {
    for (filename, content) in seeds {
        let target = dir.join(filename);
        let force_upgrade = match json_version(content) {
            Some(bundled) => bundled > installed_version(&target).unwrap_or((0, 0, 0)),
            None => false,
        };
        if force_upgrade || !target.exists() {
            if let Err(e) = fs::write(&target, content) {
                eprintln!("Warning: could not write {}: {e}", target.display());
            }
        }
    }
}

/// Read and parse the `version` of a seed file already on disk, if any.
fn installed_version(target: &Path) -> Option<(u64, u64, u64)> {
    fs::read_to_string(target).ok().and_then(|c| json_version(&c))
}

/// Parse a JSON document's `version` field into a comparable (major, minor,
/// patch) tuple. Returns `None` when the JSON is unparseable or the version is
/// missing/malformed — callers treat that as "don't force an upgrade". Used for
/// both pack manifests and seed personas.
fn json_version(doc: &str) -> Option<(u64, u64, u64)> {
    let v: serde_json::Value = serde_json::from_str(doc).ok()?;
    let s = v.get("version")?.as_str()?;
    let mut parts = s.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Write every embedded integration pack to `<data>/integration_packs/<id>/`.
/// Each pack is force-refreshed (all files overwritten) when its bundled
/// `pack.json` version exceeds the installed one; otherwise files are written
/// only when missing. Pack files are read-only in the UI, so overwriting is
/// safe and is the only way a manifest change (e.g. a shrunk `requires_env`)
/// reaches existing installs, which otherwise keep the first-seeded copy.
fn write_integration_packs() {
    write_seed_tree("integration_packs", &paths::integration_packs_dir(), "pack.json");
}

/// Materialize a single embedded integration pack into the data dir, writing
/// any of its files that are missing (which also repairs a partial install).
/// Returns `false` if no pack with `id` is embedded in the binary.
///
/// Called by [`crate::integration_packs::set_enabled`] so that *enabling* a
/// pack always guarantees its personas, skills, and api_tools are present on
/// disk — an enabled flag with no files behind it was a real failure mode.
/// Idempotent: existing files are left untouched (version upgrades still happen
/// at startup via [`write_integration_packs`]).
/// True when a pack with this id ships embedded in the binary (a first-party
/// seed). Registry installs refuse ids that collide with an embedded pack so the
/// version-gated boot seeder can never clobber a registry install.
pub fn is_embedded_pack(id: &str) -> bool {
    SEED.get_dir(format!("integration_packs/{id}")).is_some()
}

pub fn install_pack(id: &str) -> bool {
    let Some(pack_dir) = SEED.get_dir(format!("integration_packs/{id}")) else {
        return false;
    };
    let dest_root = paths::integration_packs_dir().join(id);
    let mut files: Vec<(PathBuf, &[u8])> = Vec::new();
    collect_files(pack_dir, pack_dir.path(), &mut files);
    for (rel_path, content) in files {
        let target = dest_root.join(&rel_path);
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                eprintln!("Warning: could not create {}: {e}", parent.display());
                continue;
            }
        }
        if let Err(e) = fs::write(&target, content) {
            eprintln!("Warning: could not write {}: {e}", target.display());
        }
    }
    true
}

/// Write every embedded `<seed_subdir>/<id>/` tree to `<dest_root>/<id>/`. Each
/// item is force-refreshed (all files overwritten) when its bundled `manifest`
/// version exceeds the installed one; otherwise files are written only when
/// missing. Shared by integration packs (`pack.json`) and gateway channel types
/// (`channel_type.json`) — both ship read-only directory trees gated on a
/// versioned manifest, so a manifest change reaches existing installs.
fn write_seed_tree(seed_subdir: &str, dest_root: &Path, manifest: &str) {
    let Some(group) = SEED.get_dir(seed_subdir) else {
        return;
    };
    for item in group.dirs() {
        let Some(id) = item.path().file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let item_dir = dest_root.join(id);

        // Every file in the item, relative to its root (recursing into subdirs).
        let mut files: Vec<(PathBuf, &[u8])> = Vec::new();
        collect_files(item, item.path(), &mut files);

        let bundled_ver = files
            .iter()
            .find(|(rel, _)| rel.to_str() == Some(manifest))
            .and_then(|(_, content)| json_version(&String::from_utf8_lossy(content)));
        let installed_ver = fs::read_to_string(item_dir.join(manifest))
            .ok()
            .and_then(|content| json_version(&content));
        let force_upgrade = matches!((bundled_ver, installed_ver), (Some(b), Some(i)) if b > i);

        for (rel_path, content) in files {
            let target = item_dir.join(&rel_path);
            if force_upgrade || !target.exists() {
                if let Some(parent) = target.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        eprintln!("Warning: could not create {}: {e}", parent.display());
                        continue;
                    }
                }
                if let Err(e) = fs::write(&target, content) {
                    eprintln!("Warning: could not write {}: {e}", target.display());
                }
            }
        }
    }
}

/// Recursively collect every embedded file under `dir` as
/// `(path_relative_to_base, contents)`, descending into subdirectories.
fn collect_files<'a>(dir: &Dir<'a>, base: &Path, out: &mut Vec<(PathBuf, &'a [u8])>) {
    for f in dir.files() {
        if let Ok(rel) = f.path().strip_prefix(base) {
            out.push((rel.to_path_buf(), f.contents()));
        }
    }
    for sub in dir.dirs() {
        collect_files(sub, base, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mc-seed-test-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn json_version_parses_and_tolerates_missing() {
        assert_eq!(json_version(r#"{"version":"1.2.3"}"#), Some((1, 2, 3)));
        assert_eq!(json_version(r#"{"version":"2"}"#), Some((2, 0, 0)));
        assert_eq!(json_version(r#"{"name":"x"}"#), None);
        assert_eq!(json_version("not json"), None);
    }

    #[test]
    fn versioned_seed_upgrades_versionless_install() {
        let dir = tmp_dir("upgrade");
        // Pre-existing install with NO version field (predates the version field).
        fs::write(dir.join("p.json"), r#"{"name":"old"}"#).unwrap();
        write_versioned_seeds(&dir, &[("p.json", r#"{"name":"new","version":"1.1.0"}"#)]);
        let got = fs::read_to_string(dir.join("p.json")).unwrap();
        assert!(got.contains("\"new\""), "versioned seed should overwrite versionless install");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn versioned_seed_skips_equal_or_newer_install() {
        let dir = tmp_dir("skip");
        fs::write(dir.join("p.json"), r#"{"name":"installed","version":"1.1.0"}"#).unwrap();
        // Same version -> no overwrite (preserves any user edit at this version).
        write_versioned_seeds(&dir, &[("p.json", r#"{"name":"bundled","version":"1.1.0"}"#)]);
        let got = fs::read_to_string(dir.join("p.json")).unwrap();
        assert!(got.contains("installed"), "equal version must not clobber");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unversioned_seed_only_writes_when_missing() {
        let dir = tmp_dir("missing");
        // Bundled seed has no version -> behaves like write-if-missing.
        fs::write(dir.join("p.json"), r#"{"name":"installed"}"#).unwrap();
        write_versioned_seeds(&dir, &[("p.json", r#"{"name":"bundled"}"#)]);
        let got = fs::read_to_string(dir.join("p.json")).unwrap();
        assert!(got.contains("installed"), "unversioned seed must not overwrite existing");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The embedded `seed/` tree resolves and contains the expected top-level
    /// dirs and at least the first-party packs we ship — guards against an
    /// empty/mis-rooted `include_dir!`. (The external-service packs now live in
    /// the `metalcraft-agent-external-packs` repo and are no longer embedded.)
    #[test]
    fn embedded_seed_tree_has_expected_contents() {
        assert!(!embedded_flat("personas").is_empty(), "personas should be embedded");
        assert!(!embedded_flat("skills").is_empty(), "skills should be embedded");
        let packs = SEED.get_dir("integration_packs").expect("integration_packs embedded");
        let ids: Vec<&str> = packs
            .dirs()
            .filter_map(|d| d.path().file_name().and_then(|s| s.to_str()))
            .collect();
        for expected in ["email", "metalcraft-notes", "metalcraft-calendar", "metalcraft-drive"] {
            assert!(ids.contains(&expected), "pack '{expected}' should be embedded, got {ids:?}");
        }
        // The email pack ships a manifest + persona + skill but no api_tools/
        // (its tools are native Rust, compiled into the agent).
        let email = SEED.get_dir("integration_packs/email").expect("email pack");
        let mut files: Vec<(PathBuf, &[u8])> = Vec::new();
        collect_files(email, email.path(), &mut files);
        let names: Vec<String> = files.iter().map(|(p, _)| p.to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n == "pack.json"), "got {names:?}");
        assert!(names.iter().any(|n| n.starts_with("personas/")), "got {names:?}");
    }

    /// Every first-party `metalcraft-*` pack must carry the ecosystem tag, or the
    /// daemon's first-boot auto-enable (`ENABLE_METALCRAFT_PACKS`) silently skips
    /// it. This guards a new subapp pack shipped without the tag.
    #[test]
    fn metalcraft_packs_are_tagged_ecosystem() {
        use crate::integration_packs::{is_ecosystem, PackManifest};
        let packs = SEED.get_dir("integration_packs").expect("integration_packs embedded");
        let mut checked = 0;
        for item in packs.dirs() {
            let id = item.path().file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !id.starts_with("metalcraft-") {
                continue;
            }
            let manifest_json = item
                .get_file(item.path().join("pack.json"))
                .and_then(|f| f.contents_utf8())
                .unwrap_or_else(|| panic!("{id} missing pack.json"));
            let manifest: PackManifest = serde_json::from_str(manifest_json)
                .unwrap_or_else(|e| panic!("{id} pack.json invalid: {e}"));
            assert!(
                is_ecosystem(&manifest),
                "pack '{id}' must carry the metalcraft-ecosystem tag"
            );
            checked += 1;
        }
        assert!(checked >= 4, "expected the 4 metalcraft-* packs, checked {checked}");
    }
}
