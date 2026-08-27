use crate::paths;
use include_dir::{Dir, include_dir};
use std::fs;
use std::path::{Path, PathBuf};

/// Every seed file — default personas, skills, flows, flow templates, and
/// integrations — embedded into the binary at compile time from the
/// `seed/` directory, then written to the app data dir on startup by
/// [`ensure_defaults`]. The released binary is therefore self-contained: it
/// carries its seeds and needs no `seed/` folder shipped alongside it.
///
/// Adding a persona, skill, or whole integration is just dropping files
/// under `seed/` — no edit to this file is needed.
///
/// Layout (top-level subdirs map to data dirs; `integrations/<id>/` is a
/// pack tree):
/// ```text
/// seed/
///   personas/*.json          -> versioned upgrade (see write_versioned_seeds)
///   skills/*.md              -> write-if-missing
///   flows/*                  -> write-if-missing
///   api_tools/*              -> write-if-missing
///   flow_templates/*         -> write-if-missing
///   integrations/<id>/  -> pack-version-gated (see write_integrations)
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

/// Ensure default personas, skills, and integrations exist in the app data
/// directory. Creates the data dirs, then writes the embedded seed files
/// (personas upgrade on version bump; everything else is written only when
/// missing — packs gate on their own `integration.json` version).
pub fn ensure_defaults() {
    // Before anything reads a path: an upgraded pod still has its data under the
    // pre-0.30 names, and reading past them costs it every tool it had installed.
    paths::migrate_legacy_integration_paths();

    let dirs = [
        paths::personas_dir(),
        paths::agent_presets_dir(),
        paths::agent_instances_dir(),
        paths::skills_dir(),
        paths::flows_dir(),
        paths::sessions_dir(),
        paths::api_tools_dir(),
        paths::flow_templates_dir(),
        paths::chats_dir(),
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

    // Agent presets follow the persona rule rather than the write-if-missing rule:
    // adding a persona to the built-in roster is exactly the kind of change that has
    // to reach installs that already have the file.
    let presets = embedded_flat("agent_presets");
    write_versioned_seeds(&paths::agent_presets_dir(), &as_refs(&presets));

    for (subdir, target) in [
        ("skills", paths::skills_dir()),
        ("flows", paths::flows_dir()),
        ("api_tools", paths::api_tools_dir()),
        ("flow_templates", paths::flow_templates_dir()),
    ] {
        let seeds = embedded_flat(subdir);
        write_seeds(&target, &as_refs(&seeds));
    }

    install_seed_agent_packs();

    retire_obsolete_seeds();
}

/// Remove seeds that shipped in older versions but have since moved or been
/// replaced, so they don't linger as stale, enable-able items on upgraded
/// installs.
fn retire_obsolete_seeds() {
    // The `whatsapp` integration became the native, generic
    // `gateway_send_message` tool.
    retire_dir(
        paths::integrations_dir().join("whatsapp"),
        "'whatsapp' integration",
    );
    // The channel *type/instance* model was replaced by the simple channels
    // connection model (channels.json); drop the seeded manifest tree so old
    // channel types stop lingering on upgraded installs.
    retire_dir(
        paths::gateway_channels_dir(),
        "gateway channel type manifests",
    );
    // `<data>/integrations/` — the install layout that predates agent packs. Nothing
    // writes it any more: the first-party packs are installed from
    // `install_seed_agent_packs` through the normal installer, and the external ones
    // were withdrawn. What is left on an upgraded pod is a directory of packs that
    // resolve through no path anyone maintains.
    //
    // Deleting it wholesale is safe *because* it is only ever regenerated content —
    // every pack that was ever written there came from this binary or the pack
    // registry, and nothing a user authored has ever lived inside it. The personas
    // and skills the old ecosystem packs seeded next to it go too, by name: those
    // were loose files in the user-local dirs, where deleting by directory is not an
    // option.
    retire_dir(paths::integrations_dir(), "legacy integration packs");
    retire_file(
        paths::data_dir().join("integrations.json"),
        "legacy pack enable-state",
    );
    for slug in [
        "metalcraft-calendar-agent",
        "metalcraft-notes-agent",
        "metalcraft-contacts-agent",
        "metalcraft-drive-agent",
        "morning-briefer",
    ] {
        retire_file(
            paths::personas_dir().join(format!("{slug}.json")),
            &format!("'{slug}' persona"),
        );
    }
    for slug in [
        "metalcraft-calendar",
        "metalcraft-notes",
        "metalcraft-contacts",
        "metalcraft-drive",
    ] {
        retire_file(
            paths::skills_dir().join(format!("{slug}.md")),
            &format!("'{slug}' skill"),
        );
    }
    // The morning brief was a calendar flow — its persona reads `mcal_*` and its
    // prompt names them, so it could not outlive the pack it was written against.
    // It lives on in the Octaweave pack, against that workspace's calendar.
    retire_file(
        paths::flow_templates_dir().join("morning-brief.json"),
        "'morning-brief' flow template",
    );
    // `metalcraft-code` — the first *agent pack* to be retired rather than a loose
    // file, so it cannot go by `remove_dir_all`. An installed pack owns an entry in
    // `agent_packs.json`, refs into the content-addressed integration store, and a
    // line in the lockfile; unlinking its directory would leave all three pointing at
    // nothing. `agent_packs::uninstall` unwinds them in order.
    retire_agent_pack("metalcraft-code");
    // `metalcraft-assistant` — a seeded preset that duplicated the `metalcraft-packs`
    // agent pack. Its roster was the orchestrator plus `metalcraft-packs-agent`, and
    // that pack ships its own `metalcraft-packs` preset with the same specialist; the
    // packs agent also stays reachable from `general-agent`, which delegates to any
    // installed persona. Two presets for one capability is a choice the picker made
    // the user resolve for no reason.
    retire_seed_preset("metalcraft-assistant");
}

/// Delete a retired seed preset, unless an agent is still built on it.
///
/// Same reasoning as [`retire_agent_pack`]: a preset is not regenerated content the
/// way `<data>/integrations/` was. An instance names its preset (`AgentInstance::
/// agent_preset`) and resolves its persona roster through it, so deleting one out
/// from under a live agent would leave a conversation with memories and history
/// pointing at nothing. If any instance still uses it the file stays and the pod says
/// which agent is holding it; every other pod cleans itself on the next start.
fn retire_seed_preset(slug: &str) {
    let path = paths::agent_presets_dir().join(format!("{slug}.json"));
    if !path.is_file() {
        return;
    }
    let holders = preset_holders(slug, &crate::agent_instance::list());
    if !holders.is_empty() {
        eprintln!(
            "Warning: keeping retired '{slug}' preset — still used by: {}",
            holders.join(", ")
        );
        return;
    }
    retire_file(path, &format!("'{slug}' agent preset"));
}

/// The agents still built on `slug`, named so an operator can find them.
///
/// Split out from reading the data dir so the rule that decides whether a preset
/// may be deleted is testable without an ambient install — the case that matters
/// is the one that is hardest to reproduce on purpose: a pod that *does* have an
/// agent on the retired preset, where the wrong answer silently breaks a
/// conversation with real history behind it.
fn preset_holders(slug: &str, instances: &[crate::agent_instance::AgentInstance]) -> Vec<String> {
    instances
        .iter()
        .filter(|i| i.agent_preset == slug)
        .map(|i| format!("{} ({})", i.name, i.id))
        .collect()
}

/// Uninstall a retired agent pack, if this pod still has it.
///
/// Unforced, deliberately. A pack ships a preset, and an agent built on a preset is
/// not regenerated content the way `<data>/integrations/` was — it has memories and
/// conversations. If one still exists the uninstall refuses and names it, and the pod
/// boots with the pack intact so the operator decides its fate. Every pod without such
/// an agent — which is most of them — cleans itself on the next start.
fn retire_agent_pack(id: &str) {
    if crate::agent_packs::find(id).is_none() {
        return;
    }
    match crate::agent_packs::uninstall(id, false) {
        Ok(_) => log::info!("Retired obsolete '{id}' agent pack"),
        Err(e) => eprintln!("Warning: could not remove retired '{id}' agent pack: {e}"),
    }
}

fn retire_file(path: PathBuf, label: &str) {
    if path.is_file() {
        if let Err(e) = fs::remove_file(&path) {
            eprintln!(
                "Warning: could not remove retired {label} at {}: {e}",
                path.display()
            );
        } else {
            log::info!("Retired obsolete {label}");
        }
    }
}

fn retire_dir(dir: PathBuf, label: &str) {
    if dir.is_dir() {
        if let Err(e) = fs::remove_dir_all(&dir) {
            eprintln!(
                "Warning: could not remove retired {label} at {}: {e}",
                dir.display()
            );
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
/// (shared with integrations) is that this clobbers user edits to a
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
    fs::read_to_string(target)
        .ok()
        .and_then(|c| json_version(&c))
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

/// Write every embedded integration to `<data>/integrations/<id>/`.
/// Each pack is force-refreshed (all files overwritten) when its bundled
/// `integration.json` version exceeds the installed one; otherwise files are written
/// only when missing. Pack files are read-only in the UI, so overwriting is
/// safe and is the only way a manifest change (e.g. a shrunk `requires_env`)
/// reaches existing installs, which otherwise keep the first-seeded copy.
/// Install the first-party agent packs embedded in this binary — **through the
/// normal installer**, the same call an operator's upload makes.
///
/// This is what "one install door" means in practice. First-party content used to
/// take a private path: files copied straight into `<data>/integrations/`, with its
/// own version gate and its own idea of what a pack was. Nothing it produced had
/// been through `Bundle::validate`, so a seed could ship something the installer
/// would have refused — and the two layouts had to be resolved through forever
/// because of it.
///
/// Now the seeds are archives like any other. They are built in memory rather than
/// shipped as `.agentpack` blobs so the tree stays readable and diffable in the
/// repo, but from `install`'s side there is no difference at all.
fn install_seed_agent_packs() {
    let Some(root) = SEED.get_dir("agent_packs") else {
        return;
    };
    for pack in root.dirs() {
        let Some(id) = pack.path().file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let mut files: std::collections::BTreeMap<String, Vec<u8>> =
            std::collections::BTreeMap::new();
        let mut manifest_json: Option<Vec<u8>> = None;
        collect_pack_files(pack, pack.path(), &mut files, &mut manifest_json);

        let Some(manifest_json) = manifest_json else {
            eprintln!("Warning: seed agent pack '{id}' has no agent_pack.json");
            continue;
        };
        let manifest: crate::agent_packs::AgentPackManifest =
            match serde_json::from_slice(&manifest_json) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("Warning: seed agent pack '{id}' has an invalid manifest: {e}");
                    continue;
                }
            };

        // Already current? Then this boot has nothing to do. `install` would happily
        // rewrite the same bytes, but it also garbage-collects the content store on
        // the way through, and that is not work to repeat on every start.
        if let Some(installed) = crate::agent_packs::find(id)
            && installed.manifest.version == manifest.version
        {
            continue;
        }

        let bytes = match crate::agent_packs::bundle::write(manifest, files) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Warning: could not build seed agent pack '{id}': {e}");
                continue;
            }
        };
        match crate::agent_packs::install(&bytes, "seed") {
            Ok(report) => log::info!(
                "seeded agent pack '{id}' v{} ({} personas, {} skills)",
                report.version,
                report.personas.len(),
                report.skills.len()
            ),
            // A pod that cannot seed one pack still boots with the rest. Loudly,
            // though: this is first-party content failing its own validator.
            Err(e) => eprintln!("Warning: could not install seed agent pack '{id}': {e}"),
        }
    }
}

/// Flatten an embedded pack directory into archive-relative paths, splitting out
/// `agent_pack.json` — [`crate::agent_packs::bundle::write`] takes the manifest as a
/// value and rejects a file map that also carries it.
fn collect_pack_files(
    dir: &include_dir::Dir<'_>,
    root: &Path,
    files: &mut std::collections::BTreeMap<String, Vec<u8>>,
    manifest: &mut Option<Vec<u8>>,
) {
    for f in dir.files() {
        let Ok(rel) = f.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel == "agent_pack.json" {
            *manifest = Some(f.contents().to_vec());
        } else {
            files.insert(rel, f.contents().to_vec());
        }
    }
    for sub in dir.dirs() {
        collect_pack_files(sub, root, files, manifest);
    }
}

/// Materialize a single embedded integration into the data dir, writing
/// any of its files that are missing (which also repairs a partial install).
/// Returns `false` if no pack with `id` is embedded in the binary.
///
/// Called by [`crate::integrations::set_enabled`] so that *enabling* a
/// pack always guarantees its personas, skills, and api_tools are present on
/// disk — an enabled flag with no files behind it was a real failure mode.
/// Idempotent: existing files are left untouched (version upgrades still happen
/// at startup via [`write_integrations`]).
/// True when a pack with this id ships embedded in the binary (a first-party
/// seed). Registry installs refuse ids that collide with an embedded pack so the
/// version-gated boot seeder can never clobber a registry install.
pub fn is_embedded_integration(id: &str) -> bool {
    SEED.get_dir(format!("integrations/{id}")).is_some()
}

pub fn install_pack(id: &str) -> bool {
    let Some(pack_dir) = SEED.get_dir(format!("integrations/{id}")) else {
        return false;
    };
    let dest_root = paths::integrations_dir().join(id);
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

    fn instance_on(preset_slug: &str, name: &str) -> crate::agent_instance::AgentInstance {
        let preset: crate::agent_preset::AgentPreset = serde_json::from_str(&format!(
            r#"{{"slug":"{preset_slug}","name":"{name}","default_persona":"orchestrator-agent",
                 "personas":[{{"slug":"orchestrator-agent","role":"default"}}]}}"#
        ))
        .unwrap();
        crate::agent_instance::AgentInstance::new(
            &preset,
            crate::agent_instance::InstanceOrigin::Workshop,
        )
    }

    #[test]
    fn a_retired_preset_with_no_agents_on_it_is_free_to_go() {
        let others = [instance_on("general-agent", "General Agent")];
        assert!(preset_holders("metalcraft-assistant", &others).is_empty());
        assert!(preset_holders("metalcraft-assistant", &[]).is_empty());
    }

    /// The case that must never delete: an agent is still built on it, and its
    /// conversations resolve their persona roster through that preset.
    #[test]
    fn an_agent_still_on_the_preset_holds_it() {
        let live = [
            instance_on("metalcraft-assistant", "Ecosystem"),
            instance_on("general-agent", "General Agent"),
        ];
        let holders = preset_holders("metalcraft-assistant", &live);
        assert_eq!(holders.len(), 1, "only the agent on that preset counts");
        assert!(
            holders[0].starts_with("Ecosystem (inst_"),
            "an operator has to be able to find it: {}",
            holders[0]
        );
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
        assert!(
            got.contains("\"new\""),
            "versioned seed should overwrite versionless install"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn versioned_seed_skips_equal_or_newer_install() {
        let dir = tmp_dir("skip");
        fs::write(
            dir.join("p.json"),
            r#"{"name":"installed","version":"1.1.0"}"#,
        )
        .unwrap();
        // Same version -> no overwrite (preserves any user edit at this version).
        write_versioned_seeds(
            &dir,
            &[("p.json", r#"{"name":"bundled","version":"1.1.0"}"#)],
        );
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
        assert!(
            got.contains("installed"),
            "unversioned seed must not overwrite existing"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The embedded `seed/` tree resolves and contains the expected top-level
    /// dirs and at least the first-party packs we ship — guards against an
    /// empty/mis-rooted `include_dir!`. (The external-service packs now live in
    /// the `metalcraft-agent-external-packs` repo and are no longer embedded;
    /// the two email packs live in `unbundled_packs/` — see its README — so a
    /// pod does not arrive holding the keys to somebody's mailbox.)
    #[test]
    fn embedded_seed_tree_has_expected_contents() {
        assert!(
            !embedded_flat("personas").is_empty(),
            "personas should be embedded"
        );
        assert!(
            !embedded_flat("skills").is_empty(),
            "skills should be embedded"
        );
        let packs = SEED.get_dir("agent_packs").expect("agent packs embedded");
        let ids: Vec<&str> = packs
            .dirs()
            .filter_map(|d| d.path().file_name().and_then(|s| s.to_str()))
            .collect();
        assert!(
            ids.contains(&"metalcraft-packs"),
            "pack 'metalcraft-packs' should be embedded, got {ids:?}"
        );
        for unbundled in ["email", "metalcraft-email"] {
            assert!(
                !ids.contains(&unbundled),
                "pack '{unbundled}' lives in unbundled_packs/ and must not be seeded — \
                 a fresh pod is not supposed to arrive with mailbox access"
            );
        }
        // A seeded agent pack is a real archive: a manifest, exactly one preset, and
        // the persona, skill and vendored integration that preset needs. Anything
        // missing here is caught by `Bundle::validate` at boot instead — on the pod,
        // where the operator can do nothing about it.
        for id in ids {
            let dir = SEED.get_dir(format!("agent_packs/{id}")).expect("pack dir");
            let mut files: Vec<(PathBuf, &[u8])> = Vec::new();
            collect_files(dir, dir.path(), &mut files);
            let names: Vec<String> = files
                .iter()
                .map(|(p, _)| p.to_string_lossy().into_owned())
                .collect();
            for required in [
                "agent_pack.json".to_string(),
                format!("agent_presets/{id}.json"),
                format!("integrations/{id}/integration.json"),
            ] {
                assert!(
                    names.contains(&required),
                    "{id} is missing {required}: {names:?}"
                );
            }
            assert!(
                names.iter().any(|n| n.starts_with("personas/")),
                "{id} ships no persona: {names:?}"
            );
            assert!(
                names.iter().any(|n| n.starts_with("skills/")),
                "{id} ships no skill: {names:?}"
            );
        }
    }

    /// Every first-party `metalcraft-*` pack must carry the ecosystem tag —
    /// `ecosystem_pack_ids` is what marks a pack as ours rather than a stranger's.
    /// This guards a new subapp pack shipped without the tag.
    ///
    /// Reads the repo rather than the embedded tree, so it covers the packs in
    /// `unbundled_packs/` too: not shipping in the binary does not make a pack
    /// less ours, and the tag is what says so once it arrives from a registry.
    #[test]
    fn metalcraft_packs_are_tagged_ecosystem() {
        use crate::integrations::{IntegrationManifest, is_ecosystem};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        for root in [repo.join("seed/agent_packs"), repo.join("unbundled_packs")] {
            for entry in fs::read_dir(&root).expect("pack root must exist") {
                let pack = entry.expect("readable entry").path();
                let id = pack
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if !id.starts_with("metalcraft-") {
                    continue;
                }
                let manifest_path = pack.join(format!("integrations/{id}/integration.json"));
                let manifest_json = fs::read_to_string(&manifest_path)
                    .unwrap_or_else(|e| panic!("{id} missing integration.json: {e}"));
                let manifest: IntegrationManifest = serde_json::from_str(&manifest_json)
                    .unwrap_or_else(|e| panic!("{id} integration.json invalid: {e}"));
                assert!(
                    is_ecosystem(&manifest),
                    "pack '{id}' must carry the metalcraft-ecosystem tag"
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 2,
            "expected the metalcraft-* packs, checked {checked}"
        );
    }
}
