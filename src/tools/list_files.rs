use async_trait::async_trait;
use std::path::Path;

pub struct ListFilesTool;

#[async_trait]
impl metalcraft::Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }
    fn description(&self) -> &str {
        "List files and directories at a given path. Set recursive=true for tree view (max depth 3)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "List recursively (default false, max depth 3)"
                }
            },
            "required": ["path"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let path_str =
            args["path"]
                .as_str()
                .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                    tool: "list_files".into(),
                    message: "Missing required parameter: path".into(),
                })?;
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        let path = Path::new(path_str);

        if !path.is_dir() {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: "list_files".into(),
                message: format!("{} is not a directory", path_str),
            });
        }

        let mut entries = Vec::new();
        let max_depth = if recursive { 3 } else { 1 };
        list_dir(path, path, max_depth, 0, &mut entries);

        let output = entries.join("\n");
        let output = crate::tools::truncate_output(&output, 30_000);

        Ok(serde_json::json!({
            "path": path_str,
            "entries": output,
            "count": entries.len()
        }))
    }
}

fn list_dir(base: &Path, dir: &Path, max_depth: usize, depth: usize, entries: &mut Vec<String>) {
    if depth >= max_depth {
        return;
    }

    let mut items: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    items.sort_by_key(|e| e.file_name());

    for entry in items {
        let path = entry.path();
        let name = path.strip_prefix(base).unwrap_or(&path);
        let is_dir = path.is_dir();

        // Skip hidden files and common noise
        if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
            if fname.starts_with('.') || fname == "node_modules" || fname == "target" {
                continue;
            }
        }

        let suffix = if is_dir { "/" } else { "" };
        entries.push(format!("{}{}", name.display(), suffix));

        if is_dir && depth + 1 < max_depth {
            list_dir(base, &path, max_depth, depth + 1, entries);
        }
    }
}
