use metalcraft::{create_react_agent_with_hooks, AgentState, Executor, RunOutcome};
use metalcraft_agent::approval::{self, ApprovalMode};
use metalcraft_agent::context::{self, CompactionConfig};
use metalcraft_agent::guard;
use metalcraft_agent::persona::Persona;
use rig::client::CompletionClient;
use rig::providers::openai;
use rustyline::DefaultEditor;

fn print_usage(personas_dir: &std::path::Path) {
    eprintln!("Usage: metalcraft-agent [--auto-approve] <persona> [task]");
    eprintln!();
    eprintln!("  If [task] is given, run once and exit.");
    eprintln!("  If [task] is omitted, enter interactive mode.");
    eprintln!("  --auto-approve  Skip approval prompts for all tools.");
    eprintln!();
    let available = Persona::list_available(personas_dir);
    if available.is_empty() {
        eprintln!("No personas found in {}", personas_dir.display());
    } else {
        eprintln!("Available personas:");
        for slug in &available {
            if let Ok(p) = Persona::load(slug, personas_dir) {
                eprintln!("  {:<20} {}", slug, p.description);
            } else {
                eprintln!("  {}", slug);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    dotenvy::dotenv().ok();

    let personas_dir = Persona::default_personas_dir();
    let skills_dir = Persona::default_skills_dir();

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let auto_approve = raw_args.iter().any(|a| a == "--auto-approve");
    let args: Vec<String> = raw_args
        .into_iter()
        .filter(|a| a != "--auto-approve")
        .collect();

    let default_persona = "coding-agent".to_string();
    let persona_slug = if args.is_empty() {
        &default_persona
    } else {
        &args[0]
    };
    let one_shot_task = if args.len() > 1 {
        Some(args[1..].join(" "))
    } else {
        None
    };

    let persona = Persona::load(persona_slug, &personas_dir)
        .map_err(|e| {
            eprintln!("Error: {}", e);
            print_usage(&personas_dir);
            std::process::exit(1);
        })
        .unwrap();

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let system_prompt = persona.build_system_prompt(&skills_dir, &cwd);

    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");
    let model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());

    let tool_config = metalcraft_agent::tools::ToolConfig {
        api_key: api_key.clone(),
        model_name: model_name.clone(),
        system_prompt: system_prompt.clone(),
        skills_dir: skills_dir.clone(),
        available_skills: persona.skills.clone(),
    };
    let registry = metalcraft_agent::tools::create_registry_for_with_config(
        &persona.tools,
        Some(&tool_config),
    );

    let approval_mode = if auto_approve {
        ApprovalMode::AutoApprove
    } else {
        ApprovalMode::default_interactive()
    };

    println!("[{}] {}", persona.name, persona.description);
    println!("Tools: {}", persona.tools.join(", "));
    if !persona.skills.is_empty() {
        println!("Skills: {}", persona.skills.join(", "));
    }
    if auto_approve {
        println!("Mode: auto-approve");
    } else {
        println!("Mode: interactive (read-only tools auto-approved)");
    }
    println!();

    let client = openai::Client::new(&api_key)?;
    let model = client.completion_model(&model_name);
    // Second model instance for context compaction (the first is moved into the graph)
    let compaction_model = client.completion_model(&model_name);
    let compaction_config = CompactionConfig::default();

    let hook = approval::build_hook(approval_mode);
    let graph = create_react_agent_with_hooks(model, registry, &system_prompt, hook)?.into_arc();
    let step_guard = guard::build_agent_guard(guard::GuardConfig::default());

    // One-shot mode
    if let Some(task) = one_shot_task {
        println!("Task: {}\n", task);

        let executor = Executor::new_from_arc(graph).max_steps(30).with_step_guard(step_guard.clone());
        let outcome = executor.run(AgentState::new(&task), "agent").await?;

        match outcome {
            RunOutcome::Completed(state) => {
                println!("\n--- Done ---");
                println!("{}", state.final_answer().unwrap_or("(no answer)"));
            }
            RunOutcome::Interrupted { reason, .. } => {
                println!("\nInterrupted: {reason}");
            }
        }
        return Ok(());
    }

    // Interactive mode
    println!("Interactive mode. Type /quit to exit, /clear to reset, /tokens for usage.\n");

    let mut rl = DefaultEditor::new()?;
    let prompt_str = format!("[{}]> ", persona_slug);
    let mut state: Option<AgentState> = None;

    loop {
        let line = match rl.readline(&prompt_str) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof) => {
                println!("\nBye.");
                break;
            }
            Err(e) => {
                eprintln!("Input error: {}", e);
                break;
            }
        };

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" {
            println!("Bye.");
            break;
        }
        if input == "/clear" {
            state = None;
            println!("Conversation cleared.\n");
            continue;
        }
        if input == "/tokens" {
            match &state {
                Some(s) => println!("~{} tokens, {} messages\n", context::estimate_tokens(s), s.messages.len()),
                None => println!("No conversation yet.\n"),
            }
            continue;
        }

        let _ = rl.add_history_entry(input);

        let mut turn_state = match state.take() {
            Some(prev) => prev.continue_with(input),
            None => AgentState::new(input),
        };

        // Compact context if approaching token limit
        match context::compact_if_needed(&mut turn_state, &compaction_model, &compaction_config).await {
            Ok(true) => {
                println!("(context compacted to ~{} tokens)", context::estimate_tokens(&turn_state));
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("Warning: compaction failed: {}", e);
            }
        }

        let executor = Executor::new_from_arc(graph.clone()).max_steps(30).with_step_guard(step_guard.clone());
        let outcome = executor.run(turn_state, "agent").await;

        match outcome {
            Ok(RunOutcome::Completed(completed_state)) => {
                println!("\n{}", completed_state.final_answer().unwrap_or("(no answer)"));
                state = Some(completed_state);
            }
            Ok(RunOutcome::Interrupted { state: s, reason, .. }) => {
                println!("\nInterrupted: {reason}");
                state = Some(s);
            }
            Err(e) => {
                eprintln!("\nError: {}", e);
                state = None;
            }
        }
        println!();
    }

    Ok(())
}
