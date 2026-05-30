use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

fn make_error(tool: &str, message: impl std::fmt::Display) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.to_string(),
    }
}

/// JSON config schema for a user-defined HTTP API tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Config for a `multipart/form-data` upload tool. The argument named by
/// `file_param` is treated as a local file path (constrained to the upload
/// root) and sent as the file part `file_field`; all other arguments become
/// text fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct HttpApiTool {
    config: HttpApiToolConfig,
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

    /// Try to load a tool by name, resolving the local api_tools directory
    /// first and falling back to any enabled integration pack (e.g. the
    /// discord pack ships `discord_send_message` and friends).
    /// Returns None if no matching config file exists anywhere.
    pub fn try_load(name: &str) -> Option<Self> {
        let dir = crate::paths::api_tools_dir();
        let (path, _origin) =
            crate::integration_packs::resolve_file(&dir, "api_tools", &format!("{name}.json"))?;
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
            let replacement = crate::key_store::lookup(var_name).unwrap_or_default();
            result = format!("{}{}{}", &result[..start], replacement, &rest[end..]);
        }
        result
    }

    /// Expand `{param}` placeholders in the URL with values from args.
    fn expand_url(&self, args: &serde_json::Value) -> String {
        let mut url = self.config.url.clone();
        if let Some(obj) = args.as_object() {
            for (key, value) in obj {
                let placeholder = format!("{{{key}}}");
                if url.contains(&placeholder) {
                    let val_str = value.as_str().map(|s| s.to_string()).unwrap_or_else(|| value.to_string());
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

    /// Build the request body based on body_mapping config.
    fn build_body(&self, args: &serde_json::Value) -> Option<serde_json::Value> {
        match self.config.body_mapping.as_str() {
            "none" => None,
            "params" => {
                if self.config.body_defaults.is_empty() {
                    Some(args.clone())
                } else {
                    let mut merged = serde_json::Map::new();
                    for (k, v) in &self.config.body_defaults {
                        merged.insert(k.clone(), v.clone());
                    }
                    if let Some(obj) = args.as_object() {
                        for (k, v) in obj {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                    Some(serde_json::Value::Object(merged))
                }
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
                            let val_str = value.as_str().map(|s| s.to_string()).unwrap_or_else(|| value.to_string());
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
    fn insert_at_path(root: &mut serde_json::Map<String, serde_json::Value>, path: &str, value: serde_json::Value) {
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
    fn build_multipart(&self, args: &serde_json::Value) -> metalcraft::Result<reqwest::multipart::Form> {
        let mp = self.config.multipart.as_ref().ok_or_else(|| {
            make_error(&self.config.name, "body_mapping=\"multipart\" requires a `multipart` config block")
        })?;
        let obj = args.as_object().ok_or_else(|| {
            make_error(&self.config.name, "multipart tool expects object arguments")
        })?;
        let path_str = obj.get(&mp.file_param).and_then(|v| v.as_str()).ok_or_else(|| {
            make_error(
                &self.config.name,
                format!("missing required file path argument `{}`", mp.file_param),
            )
        })?;

        let resolved = Self::resolve_within_upload_root(path_str)
            .map_err(|e| make_error(&self.config.name, e))?;
        let bytes = std::fs::read(&resolved).map_err(|e| {
            make_error(&self.config.name, format!("failed to read {}: {e}", resolved.display()))
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
            let val = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
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
        let method: reqwest::Method = self
            .config
            .method
            .parse()
            .map_err(|_| make_error(&self.config.name, format!("Invalid HTTP method: {}", self.config.method)))?;

        let url = self.expand_url(&args);

        let client = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .user_agent("metalcraft-agent/0.1 (http_api_tool)")
            .build()
            .map_err(|e| make_error(&self.config.name, format!("Failed to create HTTP client: {e}")))?;

        let mut req = client.request(method, &url);

        // Apply headers with env var expansion
        for (key, value) in &self.config.headers {
            let expanded_value = Self::expand_env(value);
            req = req.header(key.as_str(), expanded_value);
        }

        // Apply body. Multipart uploads build a form (reading a local file
        // constrained to the upload root); every other mapping serializes JSON.
        if self.config.body_mapping == "multipart" {
            req = req.multipart(self.build_multipart(&args)?);
        } else if let Some(body) = self.build_body(&args) {
            req = req.json(&body);
        }

        let response = req.send().await.map_err(|e| {
            make_error(&self.config.name, format!("Request to {url} failed: {e}"))
        })?;

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
        }
    }

    // -- clean_unexpanded_placeholders --

    #[test]
    fn clean_placeholders_no_query_string() {
        let result = HttpApiTool::clean_unexpanded_placeholders("http://example.com/api/v1/channels/123");
        assert_eq!(result, "http://example.com/api/v1/channels/123");
    }

    #[test]
    fn clean_placeholders_all_expanded() {
        let result = HttpApiTool::clean_unexpanded_placeholders("http://example.com/api?limit=10&platform=discord");
        assert_eq!(result, "http://example.com/api?limit=10&platform=discord");
    }

    #[test]
    fn clean_placeholders_removes_unexpanded() {
        let result = HttpApiTool::clean_unexpanded_placeholders("http://example.com/api?limit={limit}&platform=discord");
        assert_eq!(result, "http://example.com/api?platform=discord");
    }

    #[test]
    fn clean_placeholders_all_unexpanded() {
        let result = HttpApiTool::clean_unexpanded_placeholders("http://example.com/api?limit={limit}&offset={offset}");
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
        cfg.body_defaults.insert("platform".into(), json!("discord"));
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
        cfg.body_defaults.insert("platform".into(), json!("discord"));
        let tool = make_tool(cfg);
        let args = json!({"platform": "slack", "content": "hello"});
        let body = tool.build_body(&args).unwrap();
        assert_eq!(body["platform"], "slack");
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
        cfg.param_paths.insert("prompt".into(), "payload.structured_data.params.prompt".into());
        cfg.param_paths.insert("model_key".into(), "payload.structured_data.model_key".into());
        cfg.param_paths.insert("aspect_ratio".into(), "payload.structured_data.params.aspect_ratio".into());
        let tool = make_tool(cfg);

        // Only prompt provided: default model_key survives, params nested.
        let body = tool.build_body(&json!({"prompt": "a red fox"})).unwrap();
        assert_eq!(body["type"], "image");
        assert_eq!(body["payload"]["structured_data"]["model_key"], "ideogram-v3");
        assert_eq!(body["payload"]["structured_data"]["params"]["prompt"], "a red fox");

        // Provided model_key overrides the default.
        let body = tool
            .build_body(&json!({"prompt": "x", "model_key": "gpt-image-2", "aspect_ratio": "16:9"}))
            .unwrap();
        assert_eq!(body["payload"]["structured_data"]["model_key"], "gpt-image-2");
        assert_eq!(body["payload"]["structured_data"]["params"]["aspect_ratio"], "16:9");
    }

    #[test]
    fn build_body_params_nested_skips_empty_and_null_so_defaults_survive() {
        let mut cfg = base_config();
        cfg.body_mapping = "params_nested".into();
        cfg.body_defaults.insert(
            "payload".into(),
            json!({ "structured_data": { "model_key": "ideogram-v3" } }),
        );
        cfg.param_paths.insert("model_key".into(), "payload.structured_data.model_key".into());
        cfg.param_paths.insert("prompt".into(), "payload.structured_data.params.prompt".into());
        let tool = make_tool(cfg);

        // A model that fills omitted optionals with "" / null must NOT clobber
        // the default model_key.
        let body = tool
            .build_body(&json!({"prompt": "a fox", "model_key": "", "aspect_ratio": null}))
            .unwrap();
        assert_eq!(body["payload"]["structured_data"]["model_key"], "ideogram-v3");
        assert_eq!(body["payload"]["structured_data"]["params"]["prompt"], "a fox");
        assert!(body["payload"]["structured_data"]["params"].get("aspect_ratio").is_none());
    }

    #[test]
    fn build_body_params_nested_encodes_special_chars_safely() {
        let mut cfg = base_config();
        cfg.body_mapping = "params_nested".into();
        cfg.param_paths.insert("prompt".into(), "payload.prompt".into());
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
        cfg.url = "http://localhost/channels/{channel_id}/messages?limit={limit}&platform=discord".into();
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
        cfg.url = "http://localhost/channels/{channel_id}/messages?limit={limit}&platform=discord".into();
        let tool = make_tool(cfg);
        let args = json!({"channel_id": "456"});
        let url = tool.expand_url(&args);
        assert!(url.contains("/channels/456/messages"));
        assert!(!url.contains("limit"));
        assert!(url.contains("platform=discord"));
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
}
