use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
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

    /// Try to load a tool by name from the api_tools directory.
    /// Returns None if no matching config file exists.
    pub fn try_load(name: &str) -> Option<Self> {
        let dir = crate::paths::api_tools_dir();
        let path = dir.join(format!("{name}.json"));
        if path.exists() {
            match Self::from_config_file(&path) {
                Ok(tool) => Some(tool),
                Err(e) => {
                    log::warn!("Failed to load api tool config '{}': {}", name, e);
                    None
                }
            }
        } else {
            None
        }
    }

    /// Expand `$ENV_VAR` references in a string using environment variables.
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
            let replacement = std::env::var(var_name).unwrap_or_default();
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

        // Apply body
        if let Some(body) = self.build_body(&args) {
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
