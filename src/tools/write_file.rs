use async_trait::async_trait;
use std::path::Path;

pub struct WriteFileTool;

#[async_trait]
impl metalcraft::Tool for WriteFileTool {
    fn name(&self) -> &str { "write_file" }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file and parent directories if they don't exist. Overwrites existing content."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to write to"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let path_str = args["path"].as_str().ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "write_file".into(),
            message: "Missing required parameter: path".into(),
        })?;
        let content = args["content"].as_str().ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "write_file".into(),
            message: "Missing required parameter: content".into(),
        })?;

        let path = Path::new(path_str);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| metalcraft::GraphError::ToolCallFailed {
                tool: "write_file".into(),
                message: format!("Failed to create directories for {}: {}", path_str, e),
            })?;
        }

        std::fs::write(path, content).map_err(|e| metalcraft::GraphError::ToolCallFailed {
            tool: "write_file".into(),
            message: format!("Failed to write {}: {}", path_str, e),
        })?;

        let line_count = content.lines().count();
        Ok(serde_json::json!({
            "path": path_str,
            "bytes_written": content.len(),
            "lines": line_count
        }))
    }
}
