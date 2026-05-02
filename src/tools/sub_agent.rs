use async_trait::async_trait;
use metalcraft::{create_react_agent, AgentState, Executor, RunOutcome};
use rig::client::CompletionClient;
use rig::providers::openai;

pub struct SubAgentTool {
    api_key: String,
    model_name: String,
    system_prompt: String,
}

impl SubAgentTool {
    pub fn new(api_key: String, model_name: String, system_prompt: String) -> Self {
        Self {
            api_key,
            model_name,
            system_prompt,
        }
    }
}

#[async_trait]
impl metalcraft::Tool for SubAgentTool {
    fn name(&self) -> &str { "sub_agent" }

    fn description(&self) -> &str {
        "Spawn a sub-agent to handle an independent subtask. Sub-agents run autonomously \
         with their own tool set and return a result. Use this for research, exploration, \
         or any task that can be delegated. Multiple sub-agents run concurrently."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the sub-agent to perform"
                },
                "tool_set": {
                    "type": "string",
                    "enum": ["read_only", "full"],
                    "description": "Tool set for the sub-agent. 'read_only' (default) = read_file, list_files, grep, find_files. 'full' = all tools."
                }
            },
            "required": ["task"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let task = args["task"].as_str().ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "sub_agent".into(),
            message: "Missing required parameter: task".into(),
        })?;

        let tool_set = args["tool_set"].as_str().unwrap_or("read_only");

        let tool_names: Vec<String> = match tool_set {
            "full" => vec![
                "read_file", "write_file", "edit_file", "bash",
                "list_files", "grep", "find_files",
            ],
            _ => vec!["read_file", "list_files", "grep", "find_files"],
        }
        .into_iter()
        .map(String::from)
        .collect();

        let registry = crate::tools::create_registry_for(&tool_names);

        let client = openai::Client::new(&self.api_key).map_err(|e| {
            metalcraft::GraphError::ToolCallFailed {
                tool: "sub_agent".into(),
                message: format!("Failed to create OpenAI client: {e}"),
            }
        })?;
        let model = client.completion_model(&self.model_name);

        let sub_prompt = format!(
            "{}\n\nYou are a sub-agent. Complete the given task efficiently and report your findings. \
             Be concise in your final answer.",
            self.system_prompt
        );

        let graph = create_react_agent(model, registry, &sub_prompt).map_err(|e| {
            metalcraft::GraphError::ToolCallFailed {
                tool: "sub_agent".into(),
                message: format!("Failed to build sub-agent graph: {e}"),
            }
        })?;

        let executor = Executor::new(graph).max_steps(15);

        // Run with timeout to prevent runaway sub-agents
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            executor.run(AgentState::new(task), "sub-agent"),
        )
        .await;

        match result {
            Ok(Ok(RunOutcome::Completed(state))) => {
                let answer = state.final_answer().unwrap_or("(no answer)").to_string();
                let tools_used = state.tools_called();
                let turn_count = state.turns().len();
                Ok(serde_json::json!({
                    "result": answer,
                    "tools_used": tools_used,
                    "turns": turn_count,
                }))
            }
            Ok(Ok(RunOutcome::Interrupted { reason, .. })) => {
                Ok(serde_json::json!({
                    "result": format!("Sub-agent interrupted: {reason}"),
                    "error": true,
                }))
            }
            Ok(Err(e)) => {
                Ok(serde_json::json!({
                    "result": format!("Sub-agent error: {e}"),
                    "error": true,
                }))
            }
            Err(_) => {
                Ok(serde_json::json!({
                    "result": "Sub-agent timed out after 120 seconds",
                    "error": true,
                }))
            }
        }
    }
}
