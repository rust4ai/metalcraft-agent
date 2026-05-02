use async_trait::async_trait;

pub struct ReportResultTool;

#[async_trait]
impl metalcraft::Tool for ReportResultTool {
    fn name(&self) -> &str { "report_result" }
    fn description(&self) -> &str {
        "Call this when you have completed the user's request. The summary is shown to the user."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "success": {
                    "type": "boolean",
                    "description": "Whether the request was completed successfully"
                },
                "summary": {
                    "type": "string",
                    "description": "A human-readable summary with the answer to the user's question"
                }
            },
            "required": ["success", "summary"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let success = args["success"].as_bool().unwrap_or(true);
        let summary = args["summary"].as_str().unwrap_or("No summary provided");
        Ok(serde_json::json!({
            "success": success,
            "summary": summary,
        }))
    }
}
