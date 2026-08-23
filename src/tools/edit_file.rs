use async_trait::async_trait;
use std::path::Path;

pub struct EditFileTool;

#[async_trait]
impl metalcraft::Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Edit a file by replacing an exact string match. The old_string must appear exactly once in the file. Use this instead of write_file when modifying existing files."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact string to find and replace. Must be unique in the file."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement string"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let path_str =
            args["path"]
                .as_str()
                .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                    tool: "edit_file".into(),
                    message: "Missing required parameter: path".into(),
                })?;
        let old_string =
            args["old_string"]
                .as_str()
                .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                    tool: "edit_file".into(),
                    message: "Missing required parameter: old_string".into(),
                })?;
        let new_string =
            args["new_string"]
                .as_str()
                .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                    tool: "edit_file".into(),
                    message: "Missing required parameter: new_string".into(),
                })?;

        let path = Path::new(path_str);
        let content =
            std::fs::read_to_string(path).map_err(|e| metalcraft::GraphError::ToolCallFailed {
                tool: "edit_file".into(),
                message: format!("Failed to read {}: {}", path_str, e),
            })?;

        let match_count = content.matches(old_string).count();
        if match_count == 0 {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: "edit_file".into(),
                message: format!(
                    "old_string not found in {}. Make sure it matches exactly.",
                    path_str
                ),
            });
        }
        if match_count > 1 {
            return Err(metalcraft::GraphError::ToolCallFailed {
                tool: "edit_file".into(),
                message: format!(
                    "old_string found {} times in {}. It must be unique. Include more surrounding context.",
                    match_count, path_str
                ),
            });
        }

        let new_content = content.replacen(old_string, new_string, 1);
        std::fs::write(path, &new_content).map_err(|e| metalcraft::GraphError::ToolCallFailed {
            tool: "edit_file".into(),
            message: format!("Failed to write {}: {}", path_str, e),
        })?;

        Ok(serde_json::json!({
            "path": path_str,
            "status": "edited",
            "lines_before": content.lines().count(),
            "lines_after": new_content.lines().count()
        }))
    }
}
