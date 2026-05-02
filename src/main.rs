use metalcraft::{create_react_agent, AgentState, Executor, RunOutcome};
use rig::client::CompletionClient;
use rig::providers::openai;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    dotenvy::dotenv().ok();

    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");
    let model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());

    let client = openai::Client::new(&api_key)?;
    let model = client.completion_model(&model_name);

    let registry = metalcraft_agent::tools::create_registry();

    let system_prompt = "You are a helpful weather assistant. \
        Use the get_weather tool to look up weather for cities. \
        Always call report_result when done with a summary of the weather.";

    let graph = create_react_agent(model, registry, system_prompt)?;

    println!("Graph:\n{}\n", graph.to_mermaid());

    let task = "What's the weather in Chicago?";
    println!("Task: {task}\n");

    let executor = Executor::new(graph).max_steps(20);
    let outcome = executor.run(AgentState::new(task), "agent").await?;

    match outcome {
        RunOutcome::Completed(state) => {
            println!("\nDone!");
            println!("Answer: {}", state.final_answer().unwrap_or("(none)"));
            println!("Messages: {}", state.messages.len());
        }
        RunOutcome::Interrupted { reason, .. } => {
            println!("\nInterrupted: {reason}");
        }
    }

    Ok(())
}
