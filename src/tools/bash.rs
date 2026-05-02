use async_trait::async_trait;
use tokio::process::Command;
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 300;

pub struct BashTool;

#[async_trait]
impl metalcraft::Tool for BashTool {
    fn name(&self) -> &str { "bash" }
    fn description(&self) -> &str {
        "Execute a bash command and return its output. Commands run in the working directory. Use timeout_secs to set max execution time (default 60s, max 300s)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Maximum execution time in seconds (default 60, max 300)"
                }
            },
            "required": ["command"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let command = args["command"].as_str().ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "bash".into(),
            message: "Missing required parameter: command".into(),
        })?;

        let timeout = args["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);

        let result = tokio::time::timeout(
            Duration::from_secs(timeout),
            Command::new("bash")
                .arg("-c")
                .arg(command)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);

                let stdout = crate::tools::truncate_output(&stdout, 30_000);
                let stderr = crate::tools::truncate_output(&stderr, 10_000);

                Ok(serde_json::json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => Err(metalcraft::GraphError::ToolCallFailed {
                tool: "bash".into(),
                message: format!("Failed to execute command: {}", e),
            }),
            Err(_) => Err(metalcraft::GraphError::ToolCallFailed {
                tool: "bash".into(),
                message: format!("Command timed out after {}s", timeout),
            }),
        }
    }
}
