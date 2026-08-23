use async_trait::async_trait;
use std::path::Path;

pub struct ReadFileTool;

#[async_trait]
impl metalcraft::Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a file. Optionally specify start_line and end_line to read a range."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path (absolute or relative to working directory)"
                },
                "start_line": {
                    "type": "integer",
                    "description": "First line to read (1-based, inclusive). Omit to read from start."
                },
                "end_line": {
                    "type": "integer",
                    "description": "Last line to read (1-based, inclusive). Omit to read to end."
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
                    tool: "read_file".into(),
                    message: "Missing required parameter: path".into(),
                })?;

        let path = Path::new(path_str);
        let content =
            std::fs::read_to_string(path).map_err(|e| metalcraft::GraphError::ToolCallFailed {
                tool: "read_file".into(),
                message: format!("Failed to read {}: {}", path_str, e),
            })?;

        let lines: Vec<&str> = content.lines().collect();
        let start = args["start_line"]
            .as_u64()
            .map(|n| n.saturating_sub(1) as usize)
            .unwrap_or(0);
        let end = args["end_line"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(lines.len());
        let end = end.min(lines.len());

        if start >= lines.len() {
            return Ok(serde_json::json!({
                "path": path_str,
                "content": "",
                "total_lines": lines.len(),
                "note": format!("start_line {} exceeds file length {}", start + 1, lines.len())
            }));
        }

        let selected: String = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>4}\t{}", start + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        let truncated = crate::tools::truncate_output(&selected, 50_000);

        Ok(serde_json::json!({
            "path": path_str,
            "content": truncated,
            "lines_shown": format!("{}-{}", start + 1, end),
            "total_lines": lines.len()
        }))
    }
}
