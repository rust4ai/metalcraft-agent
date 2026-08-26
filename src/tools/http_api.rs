use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Ceiling on a tool-declared timeout. A pack is third-party content, so the
/// bound it asks for is a request, not an instruction: without a cap, one
/// mistyped config could park an agent step on a socket indefinitely. Ten
/// minutes is the longest any known upstream (buildr.space's 600s build) holds a
/// request open.
const MAX_TIMEOUT_SECS: u64 = 600;

fn make_error(tool: &str, message: impl std::fmt::Display) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.to_string(),
    }
}

/// JSON config schema for a user-defined HTTP API tool.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HttpApiToolConfig {
    pub name: String,
    pub description: String,
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub parameters: serde_json::Value,
    #[serde(default = "default_body_mapping")]
    pub body_mapping: String,
    #[serde(default)]
    pub body_template: Option<String>,
    #[serde(default)]
    pub body_defaults: HashMap<String, serde_json::Value>,
    /// Present only when `body_mapping == "params_nested"`: maps each flat
    /// argument name to a dotted JSON path in the request body (e.g.
    /// `"prompt": "payload.structured_data.params.prompt"`). Lets a tool expose
    /// simple scalar parameters (which OpenAI's function-schema validation
    /// accepts) while still producing a deeply nested JSON body — without the
    /// string-substitution hazards of `body_mapping == "template"`. Arguments
    /// with no entry here are inserted at the top level under their own name.
    /// Absent (optional) arguments are simply skipped, and `body_defaults`
    /// provides the nested base the paths are written onto.
    #[serde(default)]
    pub param_paths: HashMap<String, String>,
    /// Marks this tool as a status **poll** (e.g. checking an async job until it
    /// finishes). Polling means calling the same tool with the same arguments
    /// repeatedly on purpose, which would otherwise look like a runaway loop, so
    /// the step guard exempts poll tools from tight loop detection. See
    /// [`crate::guard`].
    #[serde(default)]
    pub poll: bool,
    /// Present only when `body_mapping == "multipart"`: describes which argument
    /// carries the local file path and what form-field name to send it under.
    #[serde(default)]
    pub multipart: Option<MultipartConfig>,
    /// How long to wait on this tool's HTTP request, in seconds. Defaults to
    /// [`DEFAULT_TIMEOUT_SECS`], which suits an API that answers in one round
    /// trip and is far too short for one that holds the request open while it
    /// works — a remote build, a clone, a container coming up. Those failed in
    /// the *tool* while the server was still succeeding, which is the worst
    /// shape of failure: the agent sees an error for work that then completes.
    /// Clamped to [`MAX_TIMEOUT_SECS`]; `0` and anything unparseable fall back
    /// to the default, so a typo cannot disable the bound.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Config for a `multipart/form-data` upload tool. The argument named by
/// `file_param` is treated as a local file path (constrained to the upload
/// root) and sent as the file part `file_field`; all other arguments become
/// text fields.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MultipartConfig {
    /// Incoming argument that holds the local file path to upload.
    pub file_param: String,
    /// Multipart field name the server expects the file under (e.g. `"file"`).
    pub file_field: String,
}

fn default_body_mapping() -> String {
    "params".to_string()
}

/// A generic HTTP tool constructed from a JSON config file.
/// Implements the metalcraft::Tool trait so it can be registered like any native tool.
/// Credentials this pod can supply on the ecosystem's behalf, and the only hosts each
/// may be sent to: `(the name a pack asks for, what to send instead, where)`.
///
/// The Octaweave pack asks for `$OCTAWEAVE_API_KEY` — an `owk_` workspace key a person
/// mints and pastes. Octaweave also accepts an `mck_` account token, which names the
/// human and carries exactly their existing reach (`ECOSYSTEM_PIVOT_PLAN.md` §3.1), and
/// every managed pod already holds one. So the paste asks a second time for a
/// credential the pod has.
///
/// **The host list is the whole safety property.** A pack is a stranger's code that
/// this pod runs against real accounts; one that named its variable
/// `OCTAWEAVE_API_KEY` and pointed at its own server would otherwise be handed this
/// pod's Metalcraft token. A fallback keyed on the variable name alone would be a
/// credential-exfiltration primitive with a friendly name.
///
/// A key the operator actually set always wins — this only fills a gap.
const ECOSYSTEM_FALLBACKS: &[(&str, &str, &[&str])] =
    &[("OCTAWEAVE_API_KEY", "METALCRAFT_TOKEN", &["octaweave.com"])];

/// The pod's own credential for `var`, if this host is one it may be sent to.
fn ecosystem_fallback(var: &str, host: Option<&str>) -> Option<String> {
    crate::key_store::lookup_present(fallback_name_for(var, host?)?)
}

/// Which credential stands in for `var` at `host` — the decision, without reading any
/// secret, so the host rule is testable on its own.
fn fallback_name_for(var: &str, host: &str) -> Option<&'static str> {
    let host = host.to_ascii_lowercase();
    ECOSYSTEM_FALLBACKS
        .iter()
        .find(|(name, _, hosts)| {
            *name == var
                && hosts
                    .iter()
                    .any(|h| host == *h || host.ends_with(&format!(".{h}")))
        })
        .map(|(_, fallback, _)| *fallback)
}

/// The host a request is about to reach, lowercased. `None` when the URL is not one
/// — which denies the fallback, since an unknown destination is not an allowed one.
fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo strip: `https://octaweave.com@evil.example/` is not octaweave.com, and
    // reading the part before the `@` is exactly how it would pass for it.
    let host_port = authority.rsplit('@').next()?;
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?.to_string(),
        None => host_port.split(':').next()?.to_string(),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

pub struct HttpApiTool {
    config: HttpApiToolConfig,
}

impl HttpApiToolConfig {
    /// The HTTP timeout this tool should run under: what it declared, clamped to
    /// [`MAX_TIMEOUT_SECS`], with `0`/absent meaning [`DEFAULT_TIMEOUT_SECS`].
    pub fn timeout(&self) -> Duration {
        let secs = match self.timeout_secs {
            Some(s) if s > 0 => s.min(MAX_TIMEOUT_SECS),
            _ => DEFAULT_TIMEOUT_SECS,
        };
        Duration::from_secs(secs)
    }
}

impl HttpApiTool {
    /// Load an HttpApiTool from a JSON config file.
    pub fn from_config_file(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let config: HttpApiToolConfig = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
        Ok(Self { config })
    }

    /// The parsed config this tool runs on — the runtime's own view of it,
    /// rather than the JSON on disk, so a caller checking what a pack declared
    /// is checking what actually took effect.
    pub fn config(&self) -> &HttpApiToolConfig {
        &self.config
    }

    /// From a list of tool names, return those that are HTTP-API tools flagged
    /// as status polls (`"poll": true`). Used to tell the step guard which tools
    /// are allowed to repeat while waiting on async work.
    pub fn poll_tool_names(names: &[String]) -> std::collections::HashSet<String> {
        names
            .iter()
            .filter(|n| Self::try_load(n).is_some_and(|t| t.config.poll))
            .cloned()
            .collect()
    }

    /// Names of every installed HTTP-API tool — the local `api_tools/` dir plus
    /// every enabled integration (user-local shadows a pack on collision).
    /// Used to grant a "full-access" sub-agent the integration tools (e.g. the
    /// starflask media tools) without the orchestrator having to know them by name.
    pub fn installed_tool_names() -> Vec<String> {
        let dir = crate::paths::api_tools_dir();
        crate::integrations::list_files_layered(&dir, "api_tools", "json")
            .into_iter()
            .filter_map(|(path, _origin)| {
                path.file_stem().and_then(|s| s.to_str()).map(String::from)
            })
            .collect()
    }

    /// Names of the HTTP-API tools provided by a single enabled integration
    /// pack (e.g. just the `github_*` tools for `"github"`). Lets a delegated
    /// sub-agent be scoped to exactly one integration instead of every installed
    /// one. Returns empty if the pack is disabled or unknown.
    pub fn installed_tool_names_for_integration(pack_id: &str) -> Vec<String> {
        let dir = crate::paths::api_tools_dir();
        crate::integrations::list_files_layered(&dir, "api_tools", "json")
            .into_iter()
            .filter(|(_path, origin)| origin.pack_id() == Some(pack_id))
            .filter_map(|(path, _origin)| {
                path.file_stem().and_then(|s| s.to_str()).map(String::from)
            })
            .collect()
    }

    /// Try to load a tool by name, resolving the local api_tools directory
    /// first and falling back to any enabled integration (e.g. the
    /// discord pack ships `discord_send_message` and friends).
    /// Returns None if no matching config file exists anywhere.
    pub fn try_load(name: &str) -> Option<Self> {
        let dir = crate::paths::api_tools_dir();
        let (path, _origin) =
            crate::integrations::resolve_file(&dir, "api_tools", &format!("{name}.json"))?;
        match Self::from_config_file(&path) {
            Ok(tool) => Some(tool),
            Err(e) => {
                log::warn!("Failed to load api tool config '{}': {}", name, e);
                None
            }
        }
    }

    /// Expand `$NAME` references in a string. Each name is resolved via the
    /// key store first, then the process environment (see
    /// [`crate::key_store::lookup`]), so managed keys and `.env` values both
    /// work. Unknown names expand to an empty string.
    fn expand_env(s: &str) -> String {
        Self::expand_env_for(s, None)
    }

    /// The same expansion, told which host the value is about to be sent to.
    ///
    /// Only headers are expanded this way, because a header is where a credential
    /// goes — and because knowing the destination is what makes
    /// [`ecosystem_fallback`] safe to offer at all.
    fn expand_env_for(s: &str, host: Option<&str>) -> String {
        let mut result = s.to_string();
        // Find all $WORD patterns (not inside braces for simplicity)
        while let Some(start) = result.find('$') {
            let rest = &result[start + 1..];
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            if end == 0 {
                break;
            }
            let var_name = &rest[..end];
            let replacement = crate::key_store::lookup(var_name)
                .or_else(|| ecosystem_fallback(var_name, host))
                .unwrap_or_default();
            result = format!("{}{}{}", &result[..start], replacement, &rest[end..]);
        }
        result
    }

    /// Expand `{param}` placeholders in the URL with values from args.
    fn expand_url(&self, args: &serde_json::Value) -> String {
        let mut url = self.config.url.clone();
        if let Some(obj) = args.as_object() {
            for (key, value) in obj {
                // Treat null / empty-string args as "not provided" so the
                // placeholder is left for clean_unexpanded_placeholders to strip,
                // rather than substituting an empty value. Otherwise an optional
                // filter like `?name={name}` becomes `?name=`, which some APIs
                // (e.g. Cloudflare zone listing) read as "match the empty string"
                // and return nothing. Mirrors the params_nested empty/null skip.
                if value.is_null() || value.as_str() == Some("") {
                    continue;
                }
                let placeholder = format!("{{{key}}}");
                if url.contains(&placeholder) {
                    let val_str = value
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| value.to_string());
                    url = url.replace(&placeholder, &val_str);
                }
            }
        }
        Self::expand_env(&Self::clean_unexpanded_placeholders(&url))
    }

    /// Remove unexpanded `{param}` placeholders from the URL query string.
    /// Handles optional parameters that the caller didn't provide.
    fn clean_unexpanded_placeholders(url: &str) -> String {
        let Some(qmark) = url.find('?') else {
            return url.to_string();
        };
        let (base, query) = url.split_at(qmark + 1);
        let cleaned: Vec<&str> = query
            .split('&')
            .filter(|segment| !segment.contains('{'))
            .collect();
        if cleaned.is_empty() {
            base.trim_end_matches('?').to_string()
        } else {
            format!("{}{}", base, cleaned.join("&"))
        }
    }

    /// Names of `{placeholder}` tokens in the configured URL. An argument with
    /// one of these names is consumed by URL expansion ([`Self::expand_url`]),
    /// so it must not *also* be written into the request body — strict body
    /// validators (e.g. cal.com v2) reject the unexpected field. Callers use
    /// this to drop URL-consumed args when building the JSON body.
    fn url_placeholder_names(&self) -> std::collections::HashSet<String> {
        let url = &self.config.url;
        let mut out = std::collections::HashSet::new();
        let mut rest = url.as_str();
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            if let Some(close) = rest.find('}') {
                let name = &rest[..close];
                if !name.is_empty() {
                    out.insert(name.to_string());
                }
                rest = &rest[close + 1..];
            } else {
                break;
            }
        }
        out
    }

    /// Build the request body based on body_mapping config. Arguments consumed
    /// as `{placeholder}` values in the URL are never included in the body (see
    /// [`Self::url_placeholder_names`]).
    fn build_body(&self, args: &serde_json::Value) -> Option<serde_json::Value> {
        let url_params = self.url_placeholder_names();
        match self.config.body_mapping.as_str() {
            "none" => None,
            "params" => {
                let mut merged = serde_json::Map::new();
                for (k, v) in &self.config.body_defaults {
                    merged.insert(k.clone(), v.clone());
                }
                if let Some(obj) = args.as_object() {
                    for (k, v) in obj {
                        if url_params.contains(k) {
                            continue;
                        }
                        merged.insert(k.clone(), v.clone());
                    }
                }
                Some(serde_json::Value::Object(merged))
            }
            "params_nested" => {
                // Start from the (possibly nested) defaults as the base tree,
                // then write each provided argument at its dotted path. Uses
                // real serde_json values throughout, so string arguments
                // containing quotes/newlines are encoded safely.
                let mut root = serde_json::Map::new();
                for (k, v) in &self.config.body_defaults {
                    root.insert(k.clone(), v.clone());
                }
                if let Some(obj) = args.as_object() {
                    for (key, value) in obj {
                        // Args consumed as URL placeholders never go in the body.
                        if url_params.contains(key) {
                            continue;
                        }
                        // Skip absent optionals. Models routinely emit `null` or
                        // `""` for omitted optional params; writing those would
                        // clobber the nested defaults (e.g. blank out model_key)
                        // and get rejected by the API.
                        if value.is_null() || value.as_str() == Some("") {
                            continue;
                        }
                        let path = self
                            .config
                            .param_paths
                            .get(key)
                            .map(String::as_str)
                            .unwrap_or(key.as_str());
                        Self::insert_at_path(&mut root, path, value.clone());
                    }
                }
                Some(serde_json::Value::Object(root))
            }
            "template" => {
                // Simple template: just use the template string with {param} replacements
                if let Some(template) = &self.config.body_template {
                    let mut result = template.clone();
                    if let Some(obj) = args.as_object() {
                        for (key, value) in obj {
                            let placeholder = format!("{{{key}}}");
                            let val_str = value
                                .as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| value.to_string());
                            result = result.replace(&placeholder, &val_str);
                        }
                    }
                    serde_json::from_str(&result).ok()
                } else {
                    Some(args.clone())
                }
            }
            _ => Some(args.clone()),
        }
    }

    /// Insert `value` into `root` at a dotted `path` (e.g.
    /// `"payload.structured_data.params.prompt"`), creating intermediate JSON
    /// objects as needed. If an intermediate segment exists but is not an
    /// object, it is overwritten with one. An empty path is a no-op.
    fn insert_at_path(
        root: &mut serde_json::Map<String, serde_json::Value>,
        path: &str,
        value: serde_json::Value,
    ) {
        let mut segments = path.split('.').filter(|s| !s.is_empty()).peekable();
        let Some(first) = segments.next() else {
            return;
        };
        if segments.peek().is_none() {
            root.insert(first.to_string(), value);
            return;
        }
        let mut current = root
            .entry(first.to_string())
            .and_modify(|v| {
                if !v.is_object() {
                    *v = serde_json::Value::Object(serde_json::Map::new());
                }
            })
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .expect("just ensured object");
        while let Some(seg) = segments.next() {
            if segments.peek().is_none() {
                current.insert(seg.to_string(), value);
                return;
            }
            current = current
                .entry(seg.to_string())
                .and_modify(|v| {
                    if !v.is_object() {
                        *v = serde_json::Value::Object(serde_json::Map::new());
                    }
                })
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .expect("just ensured object");
        }
    }

    /// Build a `multipart/form-data` body. The arg named by `multipart.file_param`
    /// is read from disk (constrained to the upload root) and sent as the file
    /// part; every other arg becomes a text field. Do **not** set a manual
    /// `Content-Type` header on a multipart tool — reqwest supplies the
    /// `multipart/form-data; boundary=…` value itself.
    fn build_multipart(
        &self,
        args: &serde_json::Value,
    ) -> metalcraft::Result<reqwest::multipart::Form> {
        let mp = self.config.multipart.as_ref().ok_or_else(|| {
            make_error(
                &self.config.name,
                "body_mapping=\"multipart\" requires a `multipart` config block",
            )
        })?;
        let obj = args.as_object().ok_or_else(|| {
            make_error(&self.config.name, "multipart tool expects object arguments")
        })?;
        let path_str = obj
            .get(&mp.file_param)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                make_error(
                    &self.config.name,
                    format!("missing required file path argument `{}`", mp.file_param),
                )
            })?;

        let resolved = Self::resolve_within_upload_root(path_str)
            .map_err(|e| make_error(&self.config.name, e))?;
        let bytes = std::fs::read(&resolved).map_err(|e| {
            make_error(
                &self.config.name,
                format!("failed to read {}: {e}", resolved.display()),
            )
        })?;
        let file_name = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("upload")
            .to_string();

        let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
        let mut form = reqwest::multipart::Form::new().part(mp.file_field.clone(), part);
        for (k, v) in obj {
            if k == &mp.file_param {
                continue;
            }
            let val = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            form = form.text(k.clone(), val);
        }
        Ok(form)
    }

    /// Resolve a caller-supplied upload path against the configured upload root
    /// (see [`crate::paths::upload_root`]).
    fn resolve_within_upload_root(path_str: &str) -> Result<PathBuf, String> {
        Self::resolve_within_root(&crate::paths::upload_root(), path_str)
    }

    /// Canonicalize `path_str` and confirm it resolves *inside* `root`. Both
    /// paths are canonicalized first, so symlinks that escape the root are
    /// rejected too. Returns the canonical path on success.
    fn resolve_within_root(root: &Path, path_str: &str) -> Result<PathBuf, String> {
        let canon_root = root.canonicalize().map_err(|e| {
            format!(
                "upload root {} is not accessible: {e} \
                 (create it or set METALCRAFT_UPLOAD_ROOT)",
                root.display()
            )
        })?;
        let canon = Path::new(path_str)
            .canonicalize()
            .map_err(|e| format!("cannot access '{path_str}': {e}"))?;
        if !canon.starts_with(&canon_root) {
            return Err(format!(
                "refusing to upload '{path_str}': resolves outside the permitted upload root {}",
                canon_root.display()
            ));
        }
        Ok(canon)
    }
}

#[async_trait]
impl metalcraft::Tool for HttpApiTool {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.config.parameters.clone()
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let method: reqwest::Method = self.config.method.parse().map_err(|_| {
            make_error(
                &self.config.name,
                format!("Invalid HTTP method: {}", self.config.method),
            )
        })?;

        let url = self.expand_url(&args);

        let client = Client::builder()
            .timeout(self.config.timeout())
            .user_agent("metalcraft-agent/0.1 (http_api_tool)")
            .build()
            .map_err(|e| {
                make_error(
                    &self.config.name,
                    format!("Failed to create HTTP client: {e}"),
                )
            })?;

        let mut req = client.request(method, &url);

        // Apply headers with env var expansion. The host goes in because a credential
        // this pod supplies on the ecosystem's behalf may only travel to that
        // ecosystem's own services — see `ecosystem_fallback`.
        let host = host_of(&url);
        for (key, value) in &self.config.headers {
            let expanded_value = Self::expand_env_for(value, host.as_deref());
            req = req.header(key.as_str(), expanded_value);
        }

        // Apply body. Multipart uploads build a form (reading a local file
        // constrained to the upload root); every other mapping serializes JSON.
        if self.config.body_mapping == "multipart" {
            req = req.multipart(self.build_multipart(&args)?);
        } else if let Some(body) = self.build_body(&args) {
            req = req.json(&body);
        }

        let response = req
            .send()
            .await
            .map_err(|e| make_error(&self.config.name, format!("Request to {url} failed: {e}")))?;

        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();

        // Try to parse as JSON, otherwise wrap in a status/body envelope
        match serde_json::from_str::<serde_json::Value>(&body_text) {
            Ok(json) => Ok(serde_json::json!({
                "status": status,
                "data": json,
            })),
            Err(_) => Ok(serde_json::json!({
                "status": status,
                "body": crate::tools::truncate_output(&body_text, 50_000),
            })),
        }
    }
}

#[cfg(test)]
mod ecosystem_credential_tests {
    use super::{HttpApiTool, fallback_name_for, host_of};

    /// A pack is a stranger's code. The pod will stand in for a credential it holds,
    /// but only when the request is going to the service that credential belongs to —
    /// otherwise "name your variable OCTAWEAVE_API_KEY" would be all it takes to be
    /// handed this pod's Metalcraft token.
    #[test]
    fn a_stood_in_credential_only_travels_to_its_own_service() {
        assert_eq!(
            fallback_name_for("OCTAWEAVE_API_KEY", "octaweave.com"),
            Some("METALCRAFT_TOKEN")
        );
        assert_eq!(
            fallback_name_for("OCTAWEAVE_API_KEY", "API.Octaweave.com"),
            Some("METALCRAFT_TOKEN"),
            "subdomains of the service, case-insensitively"
        );

        for hostile in [
            "octaweave.com.evil.example",
            "evil.example",
            "notoctaweave.com",
            "",
        ] {
            assert_eq!(
                fallback_name_for("OCTAWEAVE_API_KEY", hostile),
                None,
                "{hostile} must not receive a credential this pod supplied"
            );
        }
        // A name nobody registered gets nothing anywhere.
        assert_eq!(fallback_name_for("SOME_OTHER_KEY", "octaweave.com"), None);
    }

    #[test]
    fn a_url_that_hides_its_host_resolves_to_no_host() {
        assert_eq!(
            host_of("https://octaweave.com/api/v1/notes").as_deref(),
            Some("octaweave.com")
        );
        assert_eq!(
            host_of("https://Octaweave.com:8443/x").as_deref(),
            Some("octaweave.com")
        );
        // The allowed host in the one position that is not the host.
        assert_eq!(
            host_of("https://octaweave.com@evil.example/x").as_deref(),
            Some("evil.example")
        );
        assert_eq!(host_of("file:///etc/passwd"), None);
        assert_eq!(host_of("not a url"), None);
    }

    /// Without a host there is no permission to stand in — an expansion that does not
    /// know where it is going gets the empty string, as it always did.
    #[test]
    fn expansion_without_a_destination_offers_nothing() {
        let expanded = HttpApiTool::expand_env_for("Bearer $OCTAWEAVE_API_KEY", None);
        assert_eq!(expanded, "Bearer ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool(config: HttpApiToolConfig) -> HttpApiTool {
        HttpApiTool { config }
    }

    fn base_config() -> HttpApiToolConfig {
        HttpApiToolConfig {
            name: "test".into(),
            description: "test tool".into(),
            method: "GET".into(),
            url: "http://localhost/api".into(),
            headers: HashMap::new(),
            parameters: json!({"type": "object", "properties": {}}),
            body_mapping: "params".into(),
            body_template: None,
            body_defaults: HashMap::new(),
            param_paths: HashMap::new(),
            poll: false,
            multipart: None,
            timeout_secs: None,
        }
    }

    // -- clean_unexpanded_placeholders --

    #[test]
    fn clean_placeholders_no_query_string() {
        let result =
            HttpApiTool::clean_unexpanded_placeholders("http://example.com/api/v1/channels/123");
        assert_eq!(result, "http://example.com/api/v1/channels/123");
    }

    #[test]
    fn clean_placeholders_all_expanded() {
        let result = HttpApiTool::clean_unexpanded_placeholders(
            "http://example.com/api?limit=10&platform=discord",
        );
        assert_eq!(result, "http://example.com/api?limit=10&platform=discord");
    }

    #[test]
    fn clean_placeholders_removes_unexpanded() {
        let result = HttpApiTool::clean_unexpanded_placeholders(
            "http://example.com/api?limit={limit}&platform=discord",
        );
        assert_eq!(result, "http://example.com/api?platform=discord");
    }

    #[test]
    fn clean_placeholders_all_unexpanded() {
        let result = HttpApiTool::clean_unexpanded_placeholders(
            "http://example.com/api?limit={limit}&offset={offset}",
        );
        assert_eq!(result, "http://example.com/api");
    }

    #[test]
    fn clean_placeholders_mixed() {
        let result = HttpApiTool::clean_unexpanded_placeholders(
            "http://example.com/api?a=1&b={b}&c=3&d={d}",
        );
        assert_eq!(result, "http://example.com/api?a=1&c=3");
    }

    // -- build_body with body_defaults --

    #[test]
    fn build_body_params_no_defaults() {
        let tool = make_tool(base_config());
        let args = json!({"channel_id": "123", "content": "hello"});
        let body = tool.build_body(&args).unwrap();
        assert_eq!(body, args);
    }

    #[test]
    fn build_body_params_with_defaults() {
        let mut cfg = base_config();
        cfg.body_defaults
            .insert("platform".into(), json!("discord"));
        let tool = make_tool(cfg);
        let args = json!({"channel_id": "123", "content": "hello"});
        let body = tool.build_body(&args).unwrap();
        assert_eq!(body["platform"], "discord");
        assert_eq!(body["channel_id"], "123");
        assert_eq!(body["content"], "hello");
    }

    #[test]
    fn build_body_args_override_defaults() {
        let mut cfg = base_config();
        cfg.body_defaults
            .insert("platform".into(), json!("discord"));
        let tool = make_tool(cfg);
        let args = json!({"platform": "slack", "content": "hello"});
        let body = tool.build_body(&args).unwrap();
        assert_eq!(body["platform"], "slack");
    }

    #[test]
    fn build_body_omits_url_path_params() {
        // A `{uid}` path param (plus a `{status}` query placeholder) must not
        // leak into the JSON body — strict validators (cal.com v2) reject them.
        let mut cfg = base_config();
        cfg.url = "https://api.cal.com/v2/bookings/{uid}/cancel?status={status}".into();
        let tool = make_tool(cfg);
        let args = json!({"uid": "bk_123", "status": "accepted", "cancellationReason": "conflict"});
        let body = tool.build_body(&args).unwrap();
        assert_eq!(body, json!({"cancellationReason": "conflict"}));
        assert!(body.get("uid").is_none());
        assert!(body.get("status").is_none());
    }

    #[test]
    fn build_body_params_nested_omits_url_path_params() {
        let mut cfg = base_config();
        cfg.url = "https://api.cal.com/v2/grants/{grant_id}/events".into();
        cfg.body_mapping = "params_nested".into();
        cfg.param_paths
            .insert("name".into(), "attendee.name".into());
        let tool = make_tool(cfg);
        let body = tool
            .build_body(
                &json!({"grant_id": "g_1", "start": "2026-01-01T09:00:00Z", "name": "Alex"}),
            )
            .unwrap();
        assert!(body.get("grant_id").is_none());
        assert_eq!(body["start"], "2026-01-01T09:00:00Z");
        assert_eq!(body["attendee"]["name"], "Alex");
    }

    // -- build_body params_nested + insert_at_path --

    #[test]
    fn build_body_params_nested_builds_deep_structure() {
        let mut cfg = base_config();
        cfg.body_mapping = "params_nested".into();
        cfg.body_defaults.insert("type".into(), json!("image"));
        cfg.body_defaults.insert(
            "payload".into(),
            json!({ "structured_data": { "model_key": "ideogram-v3" } }),
        );
        cfg.param_paths.insert(
            "prompt".into(),
            "payload.structured_data.params.prompt".into(),
        );
        cfg.param_paths.insert(
            "model_key".into(),
            "payload.structured_data.model_key".into(),
        );
        cfg.param_paths.insert(
            "aspect_ratio".into(),
            "payload.structured_data.params.aspect_ratio".into(),
        );
        let tool = make_tool(cfg);

        // Only prompt provided: default model_key survives, params nested.
        let body = tool.build_body(&json!({"prompt": "a red fox"})).unwrap();
        assert_eq!(body["type"], "image");
        assert_eq!(
            body["payload"]["structured_data"]["model_key"],
            "ideogram-v3"
        );
        assert_eq!(
            body["payload"]["structured_data"]["params"]["prompt"],
            "a red fox"
        );

        // Provided model_key overrides the default.
        let body = tool
            .build_body(&json!({"prompt": "x", "model_key": "gpt-image-2", "aspect_ratio": "16:9"}))
            .unwrap();
        assert_eq!(
            body["payload"]["structured_data"]["model_key"],
            "gpt-image-2"
        );
        assert_eq!(
            body["payload"]["structured_data"]["params"]["aspect_ratio"],
            "16:9"
        );
    }

    #[test]
    fn build_body_params_nested_skips_empty_and_null_so_defaults_survive() {
        let mut cfg = base_config();
        cfg.body_mapping = "params_nested".into();
        cfg.body_defaults.insert(
            "payload".into(),
            json!({ "structured_data": { "model_key": "ideogram-v3" } }),
        );
        cfg.param_paths.insert(
            "model_key".into(),
            "payload.structured_data.model_key".into(),
        );
        cfg.param_paths.insert(
            "prompt".into(),
            "payload.structured_data.params.prompt".into(),
        );
        let tool = make_tool(cfg);

        // A model that fills omitted optionals with "" / null must NOT clobber
        // the default model_key.
        let body = tool
            .build_body(&json!({"prompt": "a fox", "model_key": "", "aspect_ratio": null}))
            .unwrap();
        assert_eq!(
            body["payload"]["structured_data"]["model_key"],
            "ideogram-v3"
        );
        assert_eq!(
            body["payload"]["structured_data"]["params"]["prompt"],
            "a fox"
        );
        assert!(
            body["payload"]["structured_data"]["params"]
                .get("aspect_ratio")
                .is_none()
        );
    }

    #[test]
    fn build_body_params_nested_encodes_special_chars_safely() {
        let mut cfg = base_config();
        cfg.body_mapping = "params_nested".into();
        cfg.param_paths
            .insert("prompt".into(), "payload.prompt".into());
        let tool = make_tool(cfg);
        // A prompt with quotes and a newline would break template substitution;
        // here it round-trips as a real JSON string value.
        let tricky = "a \"fancy\" fox\nwith a hat";
        let body = tool.build_body(&json!({"prompt": tricky})).unwrap();
        assert_eq!(body["payload"]["prompt"], tricky);
    }

    #[test]
    fn insert_at_path_creates_and_overwrites() {
        let mut root = serde_json::Map::new();
        HttpApiTool::insert_at_path(&mut root, "a.b.c", json!(1));
        assert_eq!(json!(root)["a"]["b"]["c"], 1);
        // Writing a sibling reuses the existing intermediate objects.
        HttpApiTool::insert_at_path(&mut root, "a.b.d", json!(2));
        assert_eq!(json!(root)["a"]["b"]["c"], 1);
        assert_eq!(json!(root)["a"]["b"]["d"], 2);
        // A single segment writes at the top level.
        HttpApiTool::insert_at_path(&mut root, "top", json!("v"));
        assert_eq!(json!(root)["top"], "v");
    }

    #[test]
    fn build_body_none_mapping() {
        let mut cfg = base_config();
        cfg.body_mapping = "none".into();
        let tool = make_tool(cfg);
        assert!(tool.build_body(&json!({"a": 1})).is_none());
    }

    // -- expand_url with placeholder cleaning --

    #[test]
    fn expand_url_replaces_provided_params() {
        let mut cfg = base_config();
        cfg.url =
            "http://localhost/channels/{channel_id}/messages?limit={limit}&platform=discord".into();
        let tool = make_tool(cfg);
        let args = json!({"channel_id": "456", "limit": 20});
        let url = tool.expand_url(&args);
        assert!(url.contains("/channels/456/messages"));
        assert!(url.contains("limit=20"));
        assert!(url.contains("platform=discord"));
    }

    #[test]
    fn expand_url_cleans_missing_optional_params() {
        let mut cfg = base_config();
        cfg.url =
            "http://localhost/channels/{channel_id}/messages?limit={limit}&platform=discord".into();
        let tool = make_tool(cfg);
        let args = json!({"channel_id": "456"});
        let url = tool.expand_url(&args);
        assert!(url.contains("/channels/456/messages"));
        assert!(!url.contains("limit"));
        assert!(url.contains("platform=discord"));
    }

    #[test]
    fn expand_url_drops_empty_and_null_optional_params() {
        // A model that fills omitted optional filters with "" or null must not
        // produce `?name=&type=` segments; those are dropped so the API doesn't
        // read them as "match the empty string" (the Cloudflare zone-listing bug).
        let mut cfg = base_config();
        cfg.url =
            "https://api.cloudflare.com/client/v4/zones?name={name}&per_page={per_page}".into();
        let tool = make_tool(cfg);
        let url = tool.expand_url(&json!({"name": "", "per_page": 50}));
        assert!(
            !url.contains("name="),
            "empty name should be dropped, got {url}"
        );
        assert!(url.contains("per_page=50"));

        let url_null = tool.expand_url(&json!({"name": null, "per_page": 50}));
        assert!(
            !url_null.contains("name="),
            "null name should be dropped, got {url_null}"
        );
        assert!(url_null.contains("per_page=50"));
    }

    // -- JSON config deserialization --

    #[test]
    fn deserialize_config_with_body_defaults() {
        let json_str = r#"{
            "name": "test_tool",
            "description": "A test",
            "method": "POST",
            "url": "http://example.com/api",
            "parameters": {"type": "object", "properties": {}},
            "body_mapping": "params",
            "body_defaults": {"platform": "discord"}
        }"#;
        let config: HttpApiToolConfig = serde_json::from_str(json_str).unwrap();
        assert_eq!(config.body_defaults.get("platform").unwrap(), "discord");
    }

    // -- multipart upload path guard --

    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "metalcraft-upload-test-{tag}-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_within_root_accepts_file_inside() {
        let root = temp_dir("root-ok");
        let file = root.join("doc.pdf");
        std::fs::write(&file, b"hi").unwrap();
        let resolved = HttpApiTool::resolve_within_root(&root, file.to_str().unwrap()).unwrap();
        assert!(resolved.starts_with(root.canonicalize().unwrap()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_within_root_rejects_file_outside() {
        let root = temp_dir("root-reject");
        let outside = temp_dir("outside");
        let file = outside.join("secret.key");
        std::fs::write(&file, b"sshhh").unwrap();
        let err = HttpApiTool::resolve_within_root(&root, file.to_str().unwrap()).unwrap_err();
        assert!(err.contains("outside the permitted upload root"));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn resolve_within_root_rejects_missing_file() {
        let root = temp_dir("root-missing");
        let err = HttpApiTool::resolve_within_root(&root, root.join("nope.pdf").to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("cannot access"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn deserialize_config_with_multipart() {
        let json_str = r#"{
            "name": "upload",
            "description": "Upload a file",
            "method": "POST",
            "url": "http://example.com/api/documents",
            "parameters": {"type": "object", "properties": {}},
            "body_mapping": "multipart",
            "multipart": {"file_param": "file_path", "file_field": "file"}
        }"#;
        let config: HttpApiToolConfig = serde_json::from_str(json_str).unwrap();
        let mp = config.multipart.unwrap();
        assert_eq!(mp.file_param, "file_path");
        assert_eq!(mp.file_field, "file");
    }

    #[test]
    fn deserialize_config_without_body_defaults() {
        let json_str = r#"{
            "name": "test_tool",
            "description": "A test",
            "method": "GET",
            "url": "http://example.com/api",
            "parameters": {"type": "object", "properties": {}}
        }"#;
        let config: HttpApiToolConfig = serde_json::from_str(json_str).unwrap();
        assert!(config.body_defaults.is_empty());
        assert_eq!(config.body_mapping, "params");
    }

    // -- timeout_secs --

    #[test]
    fn timeout_defaults_when_undeclared() {
        let config = base_config();
        assert_eq!(config.timeout_secs, None);
        assert_eq!(config.timeout(), Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn timeout_honours_a_declared_value() {
        let mut config = base_config();
        config.timeout_secs = Some(330);
        assert_eq!(config.timeout(), Duration::from_secs(330));
    }

    #[test]
    fn timeout_is_clamped_to_the_ceiling() {
        let mut config = base_config();
        config.timeout_secs = Some(86_400);
        assert_eq!(config.timeout(), Duration::from_secs(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn timeout_of_zero_falls_back_to_the_default() {
        // A typo must not mean "wait forever" — reqwest reads a zero timeout as
        // no timeout at all.
        let mut config = base_config();
        config.timeout_secs = Some(0);
        assert_eq!(config.timeout(), Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn deserialize_config_with_timeout_secs() {
        let json_str = r#"{
            "name": "slow_tool",
            "description": "Holds the request open while it works",
            "method": "POST",
            "url": "http://example.com/api/build",
            "parameters": {"type": "object", "properties": {}},
            "timeout_secs": 300
        }"#;
        let config: HttpApiToolConfig = serde_json::from_str(json_str).unwrap();
        assert_eq!(config.timeout(), Duration::from_secs(300));
    }
}
