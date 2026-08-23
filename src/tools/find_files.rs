use async_trait::async_trait;
use std::path::Path;

const MAX_RESULTS: usize = 200;

pub struct FindFilesTool;

#[async_trait]
impl metalcraft::Tool for FindFilesTool {
    fn name(&self) -> &str {
        "find_files"
    }
    fn description(&self) -> &str {
        "Find files by name pattern (substring match). Searches recursively. Returns file paths."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "File name pattern to search for (substring match)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in"
                }
            },
            "required": ["pattern", "path"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let pattern =
            args["pattern"]
                .as_str()
                .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                    tool: "find_files".into(),
                    message: "Missing required parameter: pattern".into(),
                })?;
        let path_str =
            args["path"]
                .as_str()
                .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                    tool: "find_files".into(),
                    message: "Missing required parameter: path".into(),
                })?;

        let path = Path::new(path_str);
        if !path.is_dir() {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: "find_files".into(),
                message: format!("{} is not a directory", path_str),
            });
        }

        let mut results = Vec::new();
        find_recursive(path, path, pattern, &mut results);

        let truncated = results.len() > MAX_RESULTS;
        if truncated {
            results.truncate(MAX_RESULTS);
        }

        let output = results.join("\n");

        Ok(serde_json::json!({
            "files": output,
            "count": results.len(),
            "truncated": truncated
        }))
    }
}

fn find_recursive(base: &Path, dir: &Path, pattern: &str, results: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        if results.len() >= MAX_RESULTS {
            break;
        }

        let path = entry.path();
        if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
            if fname.starts_with('.') || fname == "node_modules" || fname == "target" {
                continue;
            }
            if fname.contains(pattern) {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                results.push(rel.display().to_string());
            }
        }

        if path.is_dir() {
            find_recursive(base, &path, pattern, results);
        }
    }
}
