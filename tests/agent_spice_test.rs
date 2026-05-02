use async_trait::async_trait;
use spice_framework::{
    test, suite, AgentConfig, AgentOutput, AgentUnderTest, Runner, RunnerConfig, Turn,
    ToolCall as SpiceToolCall,
};
use std::sync::Arc;
use std::time::Duration;

struct WeatherAgent;

#[async_trait]
impl AgentUnderTest for WeatherAgent {
    async fn run(
        &self,
        user_message: &str,
        _config: &AgentConfig,
    ) -> Result<AgentOutput, spice_framework::SpiceError> {
        dotenvy::dotenv().ok();
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| spice_framework::SpiceError::ConfigError("OPENAI_API_KEY not set".into()))?;

        let model = std::env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| "gpt-4o".to_string());

        let system_prompt = "You are a helpful weather assistant. Use the get_weather tool to look up weather for cities the user asks about. Always call report_result when done with a summary of the weather including temperature and conditions.";

        let ai = metalcraft_agent::ai_client::AiClient::new(
            reqwest::Client::new(),
            api_key,
            model,
        );
        let registry = metalcraft_agent::tools::ToolRegistry::new();

        let messages = vec![serde_json::json!({
            "role": "user",
            "content": user_message,
        })];

        let start = std::time::Instant::now();
        let result = metalcraft_agent::agent::run(
            &ai,
            &registry,
            system_prompt,
            messages,
            10,
        )
        .await;
        let duration = start.elapsed();

        let turns: Vec<Turn> = result
            .turns
            .iter()
            .map(|t| Turn {
                index: t.index as usize,
                output_text: None,
                tool_calls: t
                    .tool_calls
                    .iter()
                    .map(|tc| SpiceToolCall {
                        id: format!("tc_{}", tc.name),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .collect(),
                tool_results: t.tool_results.clone(),
                stop_reason: None,
                duration: Duration::from_millis(t.duration_ms),
            })
            .collect();

        let tools_called: Vec<String> = turns
            .iter()
            .flat_map(|t| t.tool_calls.iter().map(|tc| tc.name.clone()))
            .collect();

        Ok(AgentOutput {
            final_text: result.summary,
            turns,
            tools_called,
            duration,
            error: if result.success {
                None
            } else {
                Some("Agent reported failure".into())
            },
        })
    }

    fn available_tools(&self, _config: &AgentConfig) -> Vec<String> {
        vec![
            "get_weather".into(),
            "report_result".into(),
        ]
    }

    fn name(&self) -> &str {
        "weather-agent"
    }
}

#[tokio::test]
async fn weather_agent_spice_suite() {
    let agent = Arc::new(WeatherAgent);

    let suite = suite(
        "Weather Agent",
        vec![
            test(
                "chicago-weather",
                "What is the weather in Chicago?",
            )
            .name("Chicago weather lookup")
            .expect_tools(&["get_weather"])
            .expect_tool_call_order(&["get_weather", "report_result"])
            .expect_text_contains("Chicago")
            .expect_turns(1..=4)
            .retries(2)
            .build(),
            test(
                "miami-weather",
                "Tell me the weather in Miami",
            )
            .name("Miami weather lookup")
            .expect_tools(&["get_weather"])
            .expect_text_contains("Miami")
            .expect_turns(1..=4)
            .retries(2)
            .build(),
            test(
                "multi-city-weather",
                "What's the weather like in Seattle and Denver?",
            )
            .name("Multi-city weather lookup")
            .expect_tools(&["get_weather"])
            .expect_turns(1..=5)
            .retries(2)
            .build(),
            test(
                "unknown-city-weather",
                "What's the weather in Timbuktu?",
            )
            .name("Unknown city returns fallback weather")
            .expect_tools(&["get_weather"])
            .expect_turns(1..=4)
            .retries(2)
            .build(),
        ],
    );

    let config = RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(120),
        ..Default::default()
    };

    let report = Runner::new(config).run(suite, agent).await;

    println!("\n{}", "=".repeat(60));
    println!("Suite: {}", report.suite_name);
    println!(
        "Results: {} passed, {} failed out of {}",
        report.passed, report.failed, report.total
    );
    println!("{}", "=".repeat(60));

    for test_report in &report.tests {
        let status = if test_report.passed { "PASS" } else { "FAIL" };
        println!("[{}] {}", status, test_report.test_id);
        if !test_report.passed {
            for ar in &test_report.assertion_results {
                if !ar.passed {
                    println!("  - {}", ar.message.as_deref().unwrap_or(&ar.description));
                }
            }
            if let Some(err) = &test_report.error {
                println!("  - error: {}", err);
            }
        }
    }

    assert!(
        report.failed == 0,
        "{} test(s) failed out of {}",
        report.failed,
        report.total
    );
}
