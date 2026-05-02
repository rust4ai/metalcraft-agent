use async_trait::async_trait;
use std::path::Path;

const MAX_MATCHES: usize = 100;

pub struct GrepTool;

#[async_trait]
impl metalcraft::Tool for GrepTool {
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> &str {
        "Search file contents for a pattern (substring or regex). Returns matching lines with file paths and line numbers. Searches recursively in directories."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (substring match, case-sensitive)"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in"
                },
                "include": {
                    "type": "string",
                    "description": "File extension filter, e.g. 'rs' or 'py'"
                }
            },
            "required": ["pattern", "path"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let pattern = args["pattern"].as_str().ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "grep".into(),
            message: "Missing required parameter: pattern".into(),
        })?;
        let path_str = args["path"].as_str().ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "grep".into(),
            message: "Missing required parameter: path".into(),
        })?;
        let include = args["include"].as_str();

        let path = Path::new(path_str);
        let mut matches = Vec::new();

        if path.is_file() {
            search_file(path, pattern, &mut matches);
        } else if path.is_dir() {
            search_dir(path, pattern, include, &mut matches);
        } else {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: "grep".into(),
                message: format!("{} does not exist", path_str),
            });
        }

        let truncated = matches.len() > MAX_MATCHES;
        if truncated {
            matches.truncate(MAX_MATCHES);
        }

        let output = matches.join("\n");

        Ok(serde_json::json!({
            "matches": output,
            "count": matches.len(),
            "truncated": truncated
        }))
    }
}

fn search_file(path: &Path, pattern: &str, matches: &mut Vec<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return, // skip binary/unreadable files
    };

    for (i, line) in content.lines().enumerate() {
        if matches.len() >= MAX_MATCHES {
            break;
        }
        if line.contains(pattern) {
            matches.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
        }
    }
}

fn search_dir(dir: &Path, pattern: &str, include: Option<&str>, matches: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        if matches.len() >= MAX_MATCHES {
            break;
        }

        let path = entry.path();
        if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
            if fname.starts_with('.') || fname == "node_modules" || fname == "target" {
                continue;
            }
        }

        if path.is_dir() {
            search_dir(&path, pattern, include, matches);
        } else if path.is_file() {
            if let Some(ext_filter) = include {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if ext != ext_filter {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            search_file(&path, pattern, matches);
        }
    }
}
