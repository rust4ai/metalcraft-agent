//! Every `api_tools/*.json` this binary ships must parse and be coherent.
//!
//! This used to assert on the Discord pack specifically, field by field. That pack
//! is published to `packs.metalcraftai.com` rather than seeded, so the whole file
//! had been failing against a directory that no longer exists — six red tests
//! guarding nothing.
//!
//! What is worth guarding is the invariant that actually breaks silently: a
//! first-party api-tool config that doesn't parse, or that parses but is malformed
//! in a way the runtime only discovers when the model calls it. So sweep the seed
//! tree — and `unbundled_packs/` with it, since a pack that ships in this repo but
//! not in the binary is still a pack we publish, and nobody would notice it rotting.

use metalcraft_agent::tools::http_api::HttpApiToolConfig;
use std::path::{Path, PathBuf};

/// The directory each first-party agent pack vendors its integration in:
/// `<root>/<id>/integrations/<id>/`, across both pack roots — `seed/agent_packs`
/// (embedded in the binary) and `unbundled_packs` (shipped by registry only).
fn seeded_integration_dirs() -> Vec<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    [repo.join("seed/agent_packs"), repo.join("unbundled_packs")]
        .iter()
        .flat_map(|root| {
            std::fs::read_dir(root).unwrap_or_else(|e| panic!("reading {}: {e}", root.display()))
        })
        .filter_map(|e| e.ok())
        .filter_map(|pack| {
            let id = pack.file_name().to_str()?.to_string();
            let dir = pack.path().join("integrations").join(&id);
            dir.is_dir().then_some(dir)
        })
        .collect()
}

/// Every api tool a seeded agent pack vendors, as `(path, config)`.
fn seeded_api_tools() -> Vec<(PathBuf, HttpApiToolConfig)> {
    let mut out = Vec::new();
    for pack in seeded_integration_dirs() {
        let dir = pack.join("api_tools");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let config: HttpApiToolConfig = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
            out.push((path, config));
        }
    }
    out
}

#[test]
fn every_seeded_api_tool_parses_and_is_coherent() {
    let tools = seeded_api_tools();
    assert!(
        !tools.is_empty(),
        "no seeded api tools found — did seed/ move?"
    );

    let mut problems: Vec<String> = Vec::new();
    for (path, c) in &tools {
        let file = path.file_stem().unwrap().to_str().unwrap();
        let mut bad = |msg: String| problems.push(format!("{}: {msg}", path.display()));

        // The filename IS the tool name at resolution time, so a mismatch means the
        // registry registers one name while the persona references another.
        if c.name != file {
            bad(format!(
                "declares name '{}' but the file is '{file}.json'",
                c.name
            ));
        }
        if c.description.trim().is_empty() {
            bad("empty description — the model has nothing to choose it by".into());
        }
        if c.url.trim().is_empty() {
            bad("empty url".into());
        }
        let method = c.method.to_ascii_uppercase();
        if !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"].contains(&method.as_str()) {
            bad(format!("unknown HTTP method '{}'", c.method));
        }
        if c.method != method {
            bad(format!("method '{}' should be upper-case", c.method));
        }

        // A GET with a body mapping would build a body nothing sends.
        if method == "GET" && c.body_mapping != "none" {
            bad(format!("GET with body_mapping '{}'", c.body_mapping));
        }
        if !["none", "params", "params_nested", "template", "multipart"]
            .contains(&c.body_mapping.as_str())
        {
            bad(format!("unknown body_mapping '{}'", c.body_mapping));
        }
        if c.body_mapping == "template" && c.body_template.is_none() {
            bad("body_mapping 'template' without a body_template".into());
        }
        if c.body_mapping == "multipart" && c.multipart.is_none() {
            bad("body_mapping 'multipart' without a multipart config".into());
        }
        if c.body_mapping != "params_nested" && !c.param_paths.is_empty() {
            bad(format!(
                "param_paths set but body_mapping is '{}'",
                c.body_mapping
            ));
        }

        // Parameters must be a JSON-Schema object, or the provider rejects the
        // function definition outright.
        if c.parameters.get("type").and_then(|v| v.as_str()) != Some("object") {
            bad("parameters is not a JSON-Schema object".into());
        }
        let props = c.parameters.get("properties").and_then(|v| v.as_object());
        if props.is_none() {
            bad("parameters has no `properties`".into());
        }

        // Every `required` entry must exist in `properties` — otherwise the model
        // is told to supply an argument the schema does not define.
        if let (Some(props), Some(required)) = (
            props,
            c.parameters.get("required").and_then(|v| v.as_array()),
        ) {
            for r in required.iter().filter_map(|v| v.as_str()) {
                if !props.contains_key(r) {
                    bad(format!("requires '{r}', which is not in properties"));
                }
            }
        }

        // Every `{placeholder}` in the URL must be a declared parameter, or the
        // request goes out with a literal brace in the path.
        if let Some(props) = props {
            for placeholder in url_placeholders(&c.url) {
                if !props.contains_key(&placeholder) {
                    bad(format!(
                        "url references {{{placeholder}}}, which is not a parameter"
                    ));
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "malformed seeded api tools:\n  - {}",
        problems.join("\n  - ")
    );
}

#[test]
fn seeded_api_tool_names_are_unique_across_packs() {
    // Tool names are a flat namespace in the registry: two packs shipping the same
    // name means whichever registers last silently wins.
    let mut seen: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    let mut clashes = Vec::new();
    for (path, c) in seeded_api_tools() {
        if let Some(first) = seen.insert(c.name.clone(), path.clone()) {
            clashes.push(format!(
                "'{}' in both {} and {}",
                c.name,
                first.display(),
                path.display()
            ));
        }
    }
    assert!(
        clashes.is_empty(),
        "duplicate api tool names:\n  - {}",
        clashes.join("\n  - ")
    );
}

#[test]
fn seeded_api_tools_reference_only_declared_env_keys() {
    // A `$NAME` in a header that the pack does not list in `requires_env` will never
    // be surfaced to the user by `key_list`, so the tool fails at call time with a
    // literal `$NAME` in the Authorization header and no hint about what to set.
    let mut problems = Vec::new();
    for pack in seeded_integration_dirs() {
        let manifest_path = pack.join("integration.json");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        let declared: Vec<String> = manifest["requires_env"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let dir = pack.join("api_tools");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            let config: HttpApiToolConfig = serde_json::from_str(&raw).unwrap();
            for value in config.headers.values() {
                for name in env_refs(value) {
                    if !declared.contains(&name) {
                        problems.push(format!(
                            "{} uses ${name}, which {} does not declare in requires_env",
                            path.display(),
                            manifest_path.display()
                        ));
                    }
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "undeclared env references:\n  - {}",
        problems.join("\n  - ")
    );
}

/// `{name}` segments in a URL, ignoring `{{escaped}}` doubles.
fn url_placeholders(url: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes: Vec<char> = url.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '{' {
            if bytes.get(i + 1) == Some(&'{') {
                i += 2;
                continue;
            }
            if let Some(end) = bytes[i + 1..].iter().position(|c| *c == '}') {
                let name: String = bytes[i + 1..i + 1 + end].iter().collect();
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    out.push(name);
                }
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `$NAME` references in a header value.
fn env_refs(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            if end > start {
                out.push(chars[start..end].iter().collect());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// A GET tool cannot mutate anything, so gating it behind an approval prompt is
/// pure friction — and it is the failure mode that actually happened: the
/// `metalcraft-contacts`, `metalcraft-email` and `metalcraft-packs` packs all
/// shipped with no arm in
/// [`metalcraft_agent::approval::OperationKind::classify`], so every one of their
/// reads fell through to `Execute` and asked permission to look something up.
///
/// This catches the next pack that forgets.
#[test]
fn seeded_read_only_api_tools_auto_approve() {
    use metalcraft_agent::approval::{OperationKind, PermissionLevel};

    let args = serde_json::json!({});
    let gated: Vec<String> = seeded_api_tools()
        .into_iter()
        .filter(|(_, c)| c.method.eq_ignore_ascii_case("GET"))
        .filter(|(_, c)| {
            OperationKind::classify(&c.name, &args).default_permission()
                != PermissionLevel::AutoApprove
        })
        .map(|(_, c)| c.name)
        .collect();

    assert!(
        gated.is_empty(),
        "these GET-only tools require approval — add an arm to OperationKind::classify:\n  - {}",
        gated.join("\n  - ")
    );
}

// ── Metalcraft Images ───────────────────────────────────────────────────────────
//
// This pack is the only seeded one whose tools spend the user's money, which makes
// two of its properties worth nailing down rather than trusting to review.

/// A tool that runs a *synchronous* generation must declare a real timeout.
///
/// The default is 30 seconds ([`HttpApiToolConfig::timeout`]), and image generation
/// is a fal render plus a download plus an upload to storage, held open on one
/// request. At 30s the tool returns an error while the server goes on to succeed and
/// charge — and an agent's natural reaction to an error is to try again. That is a
/// double charge for one image, which is the most expensive mistake this pack can
/// make, so the timeout is asserted rather than assumed.
#[test]
fn image_generation_tools_declare_a_long_enough_timeout() {
    const NEEDS_TIME: &[&str] = &[
        "mimg_generate_image",
        "mimg_edit_image",
        "mimg_describe_image",
        "mimg_upload_image",
    ];
    let mut checked = 0;
    for (path, config) in seeded_api_tools() {
        if !NEEDS_TIME.contains(&config.name.as_str()) {
            continue;
        }
        checked += 1;
        let secs = config.timeout_secs.unwrap_or(0);
        assert!(
            secs >= 120,
            "{} declares timeout_secs={secs}; a synchronous generation needs at least 120 \
             or it will time out on work that then completes and charges",
            path.display()
        );
    }
    assert_eq!(checked, NEEDS_TIME.len(), "a spending tool went missing from the pack");
}

/// The pack's whole safety story is that `METALCRAFT_TOKEN` — an account credential
/// with the user's full reach — only ever travels to Metalcraft's own service. A tool
/// pointed anywhere else, by typo or otherwise, would hand that token to a stranger.
#[test]
fn image_tools_only_send_the_account_token_to_images_metalcraftai() {
    let mut checked = 0;
    for (path, config) in seeded_api_tools() {
        if !config.name.starts_with("mimg_") {
            continue;
        }
        checked += 1;
        assert!(
            config.url.starts_with("https://images.metalcraftai.com/"),
            "{} points at {} — a mimg_ tool carries METALCRAFT_TOKEN and must only \
             ever reach images.metalcraftai.com",
            path.display(),
            config.url
        );
    }
    assert!(checked >= 8, "expected the full mimg_ toolset, found {checked}");
}

/// Reads auto-approve; anything that spends credits or hands out access asks first.
/// Asserted by name because the classification is a policy decision, not a pattern:
/// a future `mimg_*` tool inheriting the wrong arm silently is exactly the failure
/// this pins down.
#[test]
fn image_tool_approval_matches_what_each_tool_costs() {
    use metalcraft_agent::approval::{OperationKind, PermissionLevel};

    let args = serde_json::json!({});
    let level = |name: &str| OperationKind::classify(name, &args).default_permission();

    for free in ["mimg_list_models", "mimg_list_generations", "mimg_get_generation"] {
        assert_eq!(
            level(free),
            PermissionLevel::AutoApprove,
            "{free} costs nothing and changes nothing — gating it is pure friction"
        );
    }
    for spends in [
        "mimg_generate_image",
        "mimg_edit_image",
        // A description is one inference call against the user's credits.
        "mimg_describe_image",
        // Spends nothing, but mints a link anyone holding it can open.
        "mimg_share_image",
        // Reads a local file and sends it off the machine.
        "mimg_upload_image",
    ] {
        assert_ne!(
            level(spends),
            PermissionLevel::AutoApprove,
            "{spends} spends money or discloses something — it must ask first"
        );
    }
}
