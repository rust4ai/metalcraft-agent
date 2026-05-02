use async_trait::async_trait;
use metalcraft::{
    create_react_agent, create_react_agent_with_hooks, AgentState, BeforeToolCallAction,
    Executor, RunOutcome,
};
use metalcraft_agent::persona::Persona;
use rig::client::CompletionClient;
use rig::providers::openai;
use spice_framework::{
    suite, test, AgentConfig, AgentOutput, AgentUnderTest, Runner, RunnerConfig, ToolCall, Turn,
};
use std::sync::Arc;
use std::time::Duration;

struct PersonaAgent {
    persona_slug: String,
}

impl PersonaAgent {
    fn setup(
        &self,
        _config: &AgentConfig,
    ) -> Result<(Persona, String, String, String), spice_framework::SpiceError> {
        dotenvy::dotenv().ok();
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| spice_framework::SpiceError::ConfigError("OPENAI_API_KEY not set".into()))?;
        let model_name = std::env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| "gpt-4o".to_string());

        let personas_dir = Persona::default_personas_dir();
        let skills_dir = Persona::default_skills_dir();
        let persona = Persona::load(&self.persona_slug, &personas_dir)
            .map_err(|e| spice_framework::SpiceError::ConfigError(e))?;

        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let system_prompt = persona.build_system_prompt(&skills_dir, &cwd);

        Ok((persona, system_prompt, api_key, model_name))
    }
}

#[async_trait]
impl AgentUnderTest for PersonaAgent {
    async fn run(
        &self,
        user_message: &str,
        config: &AgentConfig,
    ) -> Result<AgentOutput, spice_framework::SpiceError> {
        let (persona, system_prompt, api_key, model_name) = self.setup(config)?;
        let tool_config = metalcraft_agent::tools::ToolConfig {
            api_key: api_key.clone(),
            model_name: model_name.clone(),
            system_prompt: system_prompt.clone(),
            skills_dir: std::path::PathBuf::from("skills"),
            available_skills: persona.skills.clone(),
        };
        let registry = metalcraft_agent::tools::create_registry_for_with_config(
            &persona.tools,
            Some(&tool_config),
        );

        let client = openai::Client::new(&api_key)
            .map_err(|e| spice_framework::SpiceError::ConfigError(e.to_string()))?;
        let model = client.completion_model(&model_name);

        // Check config for denied_tools (for approval testing)
        let denied_tools: Vec<String> = config
            .data
            .get("denied_tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let graph = if denied_tools.is_empty() {
            create_react_agent(model, registry, &system_prompt)
        } else {
            let hook: metalcraft::BeforeToolCallHook =
                Arc::new(move |name: &str, _args: &serde_json::Value| {
                    if denied_tools.contains(&name.to_string()) {
                        BeforeToolCallAction::Deny(format!("Tool '{}' denied by policy", name))
                    } else {
                        BeforeToolCallAction::Proceed
                    }
                });
            create_react_agent_with_hooks(model, registry, &system_prompt, Some(hook))
        }
        .map_err(|e| spice_framework::SpiceError::ConfigError(e.to_string()))?;

        let start = std::time::Instant::now();
        let executor = Executor::new(graph).max_steps(20);
        let outcome = executor
            .run(AgentState::new(user_message), "test")
            .await
            .map_err(|e| spice_framework::SpiceError::AgentError(e.to_string()))?;

        let duration = start.elapsed();

        let (final_text, tools_called, turns, error) = match outcome {
            RunOutcome::Completed(state) => {
                let text = state.final_answer().unwrap_or("(none)").to_string();
                let tools = state.tools_called();
                let turns: Vec<Turn> = state
                    .turns()
                    .into_iter()
                    .map(|t| Turn {
                        index: t.index,
                        output_text: t.assistant_text,
                        tool_calls: t
                            .tool_calls
                            .into_iter()
                            .map(|tc| ToolCall {
                                id: tc.id,
                                name: tc.name,
                                arguments: tc.args,
                            })
                            .collect(),
                        tool_results: t
                            .tool_results
                            .into_iter()
                            .map(|tr| serde_json::json!({
                                "id": tr.id,
                                "name": tr.name,
                                "result": tr.result,
                            }))
                            .collect(),
                        stop_reason: None,
                        duration: Duration::ZERO,
                    })
                    .collect();
                (text, tools, turns, None)
            }
            RunOutcome::Interrupted { reason, .. } => {
                (String::new(), vec![], vec![], Some(format!("Interrupted: {reason}")))
            }
        };

        Ok(AgentOutput {
            final_text,
            turns,
            tools_called,
            duration,
            error,
        })
    }

    fn available_tools(&self, _config: &AgentConfig) -> Vec<String> {
        let personas_dir = Persona::default_personas_dir();
        Persona::load(&self.persona_slug, &personas_dir)
            .map(|p| p.tools)
            .unwrap_or_default()
    }

    fn name(&self) -> &str {
        "persona-agent"
    }
}

fn print_report(report: &spice_framework::SuiteReport) {
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
}

#[tokio::test]
async fn coding_agent_spice_suite() {
    let agent = Arc::new(PersonaAgent {
        persona_slug: "coding-agent".into(),
    });

    let suite = suite(
        "Coding Agent",
        vec![
            test(
                "list-project",
                "List the files in the current directory and describe what this project is.",
            )
            .name("Project listing")
            .expect_turns(1..=5)
            .retries(2)
            .build(),
            test(
                "read-cargo-toml",
                "Read the Cargo.toml file and tell me what dependencies this project uses.",
            )
            .name("Read Cargo.toml")
            .expect_turns(1..=4)
            .expect(|output| {
                if output.final_text.to_lowercase().contains("metalcraft") {
                    Ok(())
                } else {
                    Err(format!("Expected 'metalcraft' in output, got: {}", &output.final_text[..200.min(output.final_text.len())]))
                }
            })
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
    print_report(&report);

    assert!(
        report.failed == 0,
        "{} test(s) failed out of {}",
        report.failed,
        report.total
    );
}

#[tokio::test]
async fn research_agent_spice_suite() {
    let agent = Arc::new(PersonaAgent {
        persona_slug: "research-agent".into(),
    });

    let suite = suite(
        "Research Agent",
        vec![test(
            "explore-structure",
            "What is the project structure of this repository? List the main source files.",
        )
        .name("Explore project structure")
        .expect_turns(1..=5)
        .retries(2)
        .build()],
    );

    let config = RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(120),
        ..Default::default()
    };

    let report = Runner::new(config).run(suite, agent).await;
    print_report(&report);

    assert!(
        report.failed == 0,
        "{} test(s) failed out of {}",
        report.failed,
        report.total
    );
}

#[tokio::test]
async fn approval_deny_write_spice_suite() {
    let agent = Arc::new(PersonaAgent {
        persona_slug: "coding-agent".into(),
    });

    let suite = suite(
        "Approval - Deny Writes",
        vec![test(
            "deny-write-file",
            "Create a file called /tmp/metalcraft-test-deny.txt with the content 'hello'. \
             If you cannot write the file, say DENIED in your response.",
        )
        .name("Write denied by policy")
        .config(AgentConfig::new(serde_json::json!({
            "denied_tools": ["write_file"]
        })))
        .expect_turns(1..=6)
        .expect(|output| {
            if output.final_text.to_lowercase().contains("denied") {
                Ok(())
            } else {
                Err(format!("Expected 'denied' (case-insensitive) in output, got: {}", &output.final_text[..200.min(output.final_text.len())]))
            }
        })
        .retries(2)
        .build()],
    );

    let config = RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(120),
        ..Default::default()
    };

    let report = Runner::new(config).run(suite, agent).await;
    print_report(&report);

    assert!(
        report.failed == 0,
        "{} test(s) failed out of {}",
        report.failed,
        report.total
    );
}

#[tokio::test]
async fn approval_deny_bash_spice_suite() {
    let agent = Arc::new(PersonaAgent {
        persona_slug: "coding-agent".into(),
    });

    let suite = suite(
        "Approval - Deny Bash",
        vec![test(
            "deny-bash",
            "Run the command 'echo hello' using bash. \
             If you cannot run commands, say DENIED in your response.",
        )
        .name("Bash denied by policy")
        .config(AgentConfig::new(serde_json::json!({
            "denied_tools": ["bash"]
        })))
        .expect_turns(1..=6)
        .expect(|output| {
            if output.final_text.to_lowercase().contains("denied") {
                Ok(())
            } else {
                Err(format!("Expected 'denied' (case-insensitive) in output, got: {}", &output.final_text[..200.min(output.final_text.len())]))
            }
        })
        .retries(2)
        .build()],
    );

    let config = RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(120),
        ..Default::default()
    };

    let report = Runner::new(config).run(suite, agent).await;
    print_report(&report);

    assert!(
        report.failed == 0,
        "{} test(s) failed out of {}",
        report.failed,
        report.total
    );
}

#[tokio::test]
async fn sub_agent_spice_suite() {
    let agent = Arc::new(PersonaAgent {
        persona_slug: "coding-agent".into(),
    });

    let suite = suite(
        "Sub-Agent",
        vec![test(
            "delegate-research",
            "Use a sub-agent to find out what dependencies are listed in the Cargo.toml file. \
             Report what the sub-agent found.",
        )
        .name("Sub-agent delegation")
        .expect(|output| {
            let text = output.final_text.to_lowercase();
            if text.contains("metalcraft") || text.contains("dependencies") || text.contains("rig") {
                Ok(())
            } else {
                Err(format!(
                    "Expected sub-agent to report dependencies, got: {}",
                    &output.final_text[..300.min(output.final_text.len())]
                ))
            }
        })
        .retries(2)
        .build()],
    );

    let config = RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(180),
        ..Default::default()
    };

    let report = Runner::new(config).run(suite, agent).await;
    print_report(&report);

    assert!(
        report.failed == 0,
        "{} test(s) failed out of {}",
        report.failed,
        report.total
    );
}
