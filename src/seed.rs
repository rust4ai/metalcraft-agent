use crate::paths;
use std::fs;
use std::path::Path;

const SEED_PERSONAS: &[(&str, &str)] = &[
    ("coding-agent.json", include_str!("../seed/personas/coding-agent.json")),
    ("orchestrator-agent.json", include_str!("../seed/personas/orchestrator-agent.json")),
    ("devops-agent.json", include_str!("../seed/personas/devops-agent.json")),
    ("research-agent.json", include_str!("../seed/personas/research-agent.json")),
    ("video-script-agent.json", include_str!("../seed/personas/video-script-agent.json")),
];

const SEED_SKILLS: &[(&str, &str)] = &[
    ("ci-cd.md", include_str!("../seed/skills/ci-cd.md")),
    ("code-review.md", include_str!("../seed/skills/code-review.md")),
    ("commit-message.md", include_str!("../seed/skills/commit-message.md")),
    ("debugging.md", include_str!("../seed/skills/debugging.md")),
    ("dockerfile-best-practices.md", include_str!("../seed/skills/dockerfile-best-practices.md")),
    ("edit-workflow.md", include_str!("../seed/skills/edit-workflow.md")),
    ("explore-codebase.md", include_str!("../seed/skills/explore-codebase.md")),
    ("planning.md", include_str!("../seed/skills/planning.md")),
    ("research-methodology.md", include_str!("../seed/skills/research-methodology.md")),
    ("summarize.md", include_str!("../seed/skills/summarize.md")),
    ("video-scripting.md", include_str!("../seed/skills/video-scripting.md")),
];

const SEED_API_TOOLS: &[(&str, &str)] = &[];

/// Flows live in the user's project — we no longer seed any by default so
/// the workshop's "+ New Flow" picker (template vs blank) is the canonical
/// entry point.
const SEED_FLOWS: &[(&str, &str)] = &[];

/// Top-level flow templates that aren't tied to any specific integration
/// pack. Pack-bundled templates live in `seed/integration_packs/<id>/flow_templates/`.
const SEED_FLOW_TEMPLATES: &[(&str, &str)] = &[];

/// Discord integration pack — bundles personas, skills, HTTP-API tools, and
/// a flow template that all relate to Discord. Disabled by default; the
/// workshop's Packs section is where the user enables it.
const DISCORD_PACK: &[(&str, &str)] = &[
    (
        "pack.json",
        include_str!("../seed/integration_packs/discord/pack.json"),
    ),
    (
        "personas/discord-agent.json",
        include_str!("../seed/integration_packs/discord/personas/discord-agent.json"),
    ),
    (
        "personas/discord-reporter-agent.json",
        include_str!("../seed/integration_packs/discord/personas/discord-reporter-agent.json"),
    ),
    (
        "skills/discord-etiquette.md",
        include_str!("../seed/integration_packs/discord/skills/discord-etiquette.md"),
    ),
    (
        "skills/discord-formatting.md",
        include_str!("../seed/integration_packs/discord/skills/discord-formatting.md"),
    ),
    (
        "api_tools/discord_send_message.json",
        include_str!("../seed/integration_packs/discord/api_tools/discord_send_message.json"),
    ),
    (
        "api_tools/discord_edit_message.json",
        include_str!("../seed/integration_packs/discord/api_tools/discord_edit_message.json"),
    ),
    (
        "api_tools/discord_add_reaction.json",
        include_str!("../seed/integration_packs/discord/api_tools/discord_add_reaction.json"),
    ),
    (
        "api_tools/discord_get_messages.json",
        include_str!("../seed/integration_packs/discord/api_tools/discord_get_messages.json"),
    ),
    (
        "api_tools/discord_get_channel_info.json",
        include_str!("../seed/integration_packs/discord/api_tools/discord_get_channel_info.json"),
    ),
    (
        "flow_templates/daily-commit-summary.json",
        include_str!("../seed/integration_packs/discord/flow_templates/daily-commit-summary.json"),
    ),
];

/// Solarabase RAG integration pack — a persona, skill, and HTTP-API tools
/// for using a Solarabase knowledge base for retrieval-augmented generation.
/// Disabled by default; enabled from the workshop's Packs section. Reads its
/// `SOLARABASE_*` config from the key store (see [`crate::key_store`]).
const SOLARABASE_PACK: &[(&str, &str)] = &[
    (
        "pack.json",
        include_str!("../seed/integration_packs/solarabase/pack.json"),
    ),
    (
        "personas/knowledge-base-agent.json",
        include_str!("../seed/integration_packs/solarabase/personas/knowledge-base-agent.json"),
    ),
    (
        "skills/solarabase-rag.md",
        include_str!("../seed/integration_packs/solarabase/skills/solarabase-rag.md"),
    ),
    (
        "api_tools/solarabase_retrieve.json",
        include_str!("../seed/integration_packs/solarabase/api_tools/solarabase_retrieve.json"),
    ),
    (
        "api_tools/solarabase_query.json",
        include_str!("../seed/integration_packs/solarabase/api_tools/solarabase_query.json"),
    ),
    (
        "api_tools/solarabase_list_documents.json",
        include_str!("../seed/integration_packs/solarabase/api_tools/solarabase_list_documents.json"),
    ),
    (
        "api_tools/solarabase_get_document_pages.json",
        include_str!("../seed/integration_packs/solarabase/api_tools/solarabase_get_document_pages.json"),
    ),
    (
        "api_tools/solarabase_upload_document.json",
        include_str!("../seed/integration_packs/solarabase/api_tools/solarabase_upload_document.json"),
    ),
];

/// Starflask Media Studio integration pack — a persona, skill, and HTTP-API
/// tools for generating media (images, video, 3D, speech) with Starflask
/// (starflask.com). Disabled by default; enabled from the workshop's Packs
/// section. Reads its `STARFLASK_API_KEY` from the key store (see
/// [`crate::key_store`]).
const STARFLASK_PACK: &[(&str, &str)] = &[
    (
        "pack.json",
        include_str!("../seed/integration_packs/starflask/pack.json"),
    ),
    (
        "personas/media-studio-agent.json",
        include_str!("../seed/integration_packs/starflask/personas/media-studio-agent.json"),
    ),
    (
        "skills/starflask-media.md",
        include_str!("../seed/integration_packs/starflask/skills/starflask-media.md"),
    ),
    (
        "api_tools/starflask_generate_image.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_generate_image.json"),
    ),
    (
        "api_tools/starflask_generate_video.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_generate_video.json"),
    ),
    (
        "api_tools/starflask_generate_3d.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_generate_3d.json"),
    ),
    (
        "api_tools/starflask_generate_speech.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_generate_speech.json"),
    ),
    (
        "api_tools/starflask_create_job.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_create_job.json"),
    ),
    (
        "api_tools/starflask_get_job.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_get_job.json"),
    ),
    (
        "api_tools/starflask_list_models.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_list_models.json"),
    ),
    (
        "api_tools/starflask_list_styles.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_list_styles.json"),
    ),
    (
        "api_tools/starflask_upload_media.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_upload_media.json"),
    ),
    (
        "api_tools/starflask_get_media.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_get_media.json"),
    ),
    (
        "api_tools/starflask_account.json",
        include_str!("../seed/integration_packs/starflask/api_tools/starflask_account.json"),
    ),
];

/// GitHub integration pack — a persona, skill, and HTTP-API tools for working
/// with GitHub over its REST API: read public/private repos, push commits,
/// manage branches, PRs, issues, and comments. Disabled by default; enabled
/// from the workshop's Packs section. Reads its `GITHUB_TOKEN` (a personal
/// access token) from the key store (see [`crate::key_store`]).
const GITHUB_PACK: &[(&str, &str)] = &[
    (
        "pack.json",
        include_str!("../seed/integration_packs/github/pack.json"),
    ),
    (
        "personas/github-agent.json",
        include_str!("../seed/integration_packs/github/personas/github-agent.json"),
    ),
    (
        "skills/github-ops.md",
        include_str!("../seed/integration_packs/github/skills/github-ops.md"),
    ),
    (
        "api_tools/github_get_authenticated_user.json",
        include_str!("../seed/integration_packs/github/api_tools/github_get_authenticated_user.json"),
    ),
    (
        "api_tools/github_list_repos.json",
        include_str!("../seed/integration_packs/github/api_tools/github_list_repos.json"),
    ),
    (
        "api_tools/github_get_repo.json",
        include_str!("../seed/integration_packs/github/api_tools/github_get_repo.json"),
    ),
    (
        "api_tools/github_get_file_contents.json",
        include_str!("../seed/integration_packs/github/api_tools/github_get_file_contents.json"),
    ),
    (
        "api_tools/github_list_branches.json",
        include_str!("../seed/integration_packs/github/api_tools/github_list_branches.json"),
    ),
    (
        "api_tools/github_get_ref.json",
        include_str!("../seed/integration_packs/github/api_tools/github_get_ref.json"),
    ),
    (
        "api_tools/github_create_branch.json",
        include_str!("../seed/integration_packs/github/api_tools/github_create_branch.json"),
    ),
    (
        "api_tools/github_create_or_update_file.json",
        include_str!("../seed/integration_packs/github/api_tools/github_create_or_update_file.json"),
    ),
    (
        "api_tools/github_list_pull_requests.json",
        include_str!("../seed/integration_packs/github/api_tools/github_list_pull_requests.json"),
    ),
    (
        "api_tools/github_create_pull_request.json",
        include_str!("../seed/integration_packs/github/api_tools/github_create_pull_request.json"),
    ),
    (
        "api_tools/github_list_issues.json",
        include_str!("../seed/integration_packs/github/api_tools/github_list_issues.json"),
    ),
    (
        "api_tools/github_create_issue.json",
        include_str!("../seed/integration_packs/github/api_tools/github_create_issue.json"),
    ),
    (
        "api_tools/github_create_issue_comment.json",
        include_str!("../seed/integration_packs/github/api_tools/github_create_issue_comment.json"),
    ),
];

/// Linear integration pack — a persona, skill, and HTTP-API tools for reading
/// and writing Linear issues (tasks) through the Linear GraphQL API. Disabled
/// by default; enabled from the workshop's Packs section. Reads its
/// `LINEAR_API_KEY` (a personal API key) from the key store (see
/// [`crate::key_store`]).
const LINEAR_PACK: &[(&str, &str)] = &[
    (
        "pack.json",
        include_str!("../seed/integration_packs/linear/pack.json"),
    ),
    (
        "personas/linear-agent.json",
        include_str!("../seed/integration_packs/linear/personas/linear-agent.json"),
    ),
    (
        "skills/linear-tasks.md",
        include_str!("../seed/integration_packs/linear/skills/linear-tasks.md"),
    ),
    (
        "api_tools/linear_viewer.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_viewer.json"),
    ),
    (
        "api_tools/linear_list_teams.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_list_teams.json"),
    ),
    (
        "api_tools/linear_list_projects.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_list_projects.json"),
    ),
    (
        "api_tools/linear_list_issues.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_list_issues.json"),
    ),
    (
        "api_tools/linear_get_issue.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_get_issue.json"),
    ),
    (
        "api_tools/linear_list_workflow_states.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_list_workflow_states.json"),
    ),
    (
        "api_tools/linear_create_issue.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_create_issue.json"),
    ),
    (
        "api_tools/linear_update_issue.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_update_issue.json"),
    ),
    (
        "api_tools/linear_create_comment.json",
        include_str!("../seed/integration_packs/linear/api_tools/linear_create_comment.json"),
    ),
];

/// Cloudflare DNS integration pack — a persona, skill, and HTTP-API tools for
/// managing Cloudflare DNS (list zones, read/create/update/patch/delete DNS
/// records) through the Cloudflare API. Disabled by default; enabled from the
/// workshop's Packs section. Reads its `CLOUDFLARE_API_TOKEN` (a scoped API
/// token with Zone:Read + DNS:Edit) from the key store (see
/// [`crate::key_store`]).
const CLOUDFLARE_PACK: &[(&str, &str)] = &[
    (
        "pack.json",
        include_str!("../seed/integration_packs/cloudflare/pack.json"),
    ),
    (
        "personas/cloudflare-agent.json",
        include_str!("../seed/integration_packs/cloudflare/personas/cloudflare-agent.json"),
    ),
    (
        "skills/cloudflare-dns.md",
        include_str!("../seed/integration_packs/cloudflare/skills/cloudflare-dns.md"),
    ),
    (
        "api_tools/cloudflare_verify_token.json",
        include_str!("../seed/integration_packs/cloudflare/api_tools/cloudflare_verify_token.json"),
    ),
    (
        "api_tools/cloudflare_list_zones.json",
        include_str!("../seed/integration_packs/cloudflare/api_tools/cloudflare_list_zones.json"),
    ),
    (
        "api_tools/cloudflare_list_dns_records.json",
        include_str!(
            "../seed/integration_packs/cloudflare/api_tools/cloudflare_list_dns_records.json"
        ),
    ),
    (
        "api_tools/cloudflare_get_dns_record.json",
        include_str!(
            "../seed/integration_packs/cloudflare/api_tools/cloudflare_get_dns_record.json"
        ),
    ),
    (
        "api_tools/cloudflare_create_dns_record.json",
        include_str!(
            "../seed/integration_packs/cloudflare/api_tools/cloudflare_create_dns_record.json"
        ),
    ),
    (
        "api_tools/cloudflare_update_dns_record.json",
        include_str!(
            "../seed/integration_packs/cloudflare/api_tools/cloudflare_update_dns_record.json"
        ),
    ),
    (
        "api_tools/cloudflare_patch_dns_record.json",
        include_str!(
            "../seed/integration_packs/cloudflare/api_tools/cloudflare_patch_dns_record.json"
        ),
    ),
    (
        "api_tools/cloudflare_delete_dns_record.json",
        include_str!(
            "../seed/integration_packs/cloudflare/api_tools/cloudflare_delete_dns_record.json"
        ),
    ),
];

const SEED_INTEGRATION_PACKS: &[(&str, &[(&str, &str)])] = &[
    ("discord", DISCORD_PACK),
    ("solarabase", SOLARABASE_PACK),
    ("starflask", STARFLASK_PACK),
    ("github", GITHUB_PACK),
    ("linear", LINEAR_PACK),
    ("cloudflare", CLOUDFLARE_PACK),
];

/// Ensure default personas and skills exist in the app data directory.
/// Creates directories and writes seed files only if they don't already exist.
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

    write_versioned_seeds(&paths::personas_dir(), SEED_PERSONAS);
    write_seeds(&paths::skills_dir(), SEED_SKILLS);
    write_seeds(&paths::flows_dir(), SEED_FLOWS);
    write_seeds(&paths::api_tools_dir(), SEED_API_TOOLS);
    write_seeds(&paths::flow_templates_dir(), SEED_FLOW_TEMPLATES);
    write_integration_packs();
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
/// new slug, which is never in `SEED_PERSONAS` and so is never touched.
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

fn write_integration_packs() {
    let root = paths::integration_packs_dir();
    for (pack_id, files) in SEED_INTEGRATION_PACKS {
        let pack_dir = root.join(pack_id);

        // If the bundled pack.json is a newer version than what's installed,
        // force-refresh every file in the pack. Pack files are read-only in the
        // UI, so users never hand-edit them — overwriting is safe and is the
        // only way a manifest change (e.g. a shrunk `requires_env`) reaches
        // existing installs, which otherwise keep the first-seeded copy forever.
        let bundled_ver = files
            .iter()
            .find(|(rel, _)| *rel == "pack.json")
            .and_then(|(_, content)| json_version(content));
        let installed_ver = fs::read_to_string(pack_dir.join("pack.json"))
            .ok()
            .and_then(|content| json_version(&content));
        let force_upgrade = matches!((bundled_ver, installed_ver), (Some(b), Some(i)) if b > i);

        for (rel_path, content) in *files {
            let target = pack_dir.join(rel_path);
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
}
