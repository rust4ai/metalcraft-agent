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

        // A backwards range is a `lines[start..end]` panic, and a panic inside a
        // tool takes the turn down rather than being answered — the model has no
        // way to learn it asked for something impossible. Seen in the wild: a
        // model that had `start_line` and `end_line` the wrong way round.
        if end < start {
            return Ok(serde_json::json!({
                "path": path_str,
                "content": "",
                "total_lines": lines.len(),
                "note": format!(
                    "end_line {} is before start_line {}; nothing to read. Pass the range in \
                     ascending order.",
                    end, start + 1
                )
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

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft::Tool;

    /// A range given backwards used to index a slice backwards, which panics —
    /// and a panic in a tool ends the turn instead of telling the model what it
    /// got wrong.
    #[tokio::test]
    async fn a_backwards_range_is_answered_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "a\nb\nc\nd\n").expect("write");

        let result = ReadFileTool
            .call(serde_json::json!({
                "path": path.to_str().unwrap(),
                "start_line": 150,
                "end_line": 120,
            }))
            .await
            .expect("a backwards range is an answer, not a failure");

        assert_eq!(result["content"], "");
        assert!(
            result["note"].as_str().unwrap().contains("start_line"),
            "the note should say what to pass instead, got {:?}",
            result["note"]
        );
    }

    /// The same shape, but with a start inside the file — the case that panicked.
    #[tokio::test]
    async fn a_backwards_range_inside_the_file_is_answered_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").expect("write");

        let result = ReadFileTool
            .call(serde_json::json!({
                "path": path.to_str().unwrap(),
                "start_line": 4,
                "end_line": 2,
            }))
            .await
            .expect("a backwards range is an answer, not a failure");

        assert_eq!(result["content"], "");
    }
}
