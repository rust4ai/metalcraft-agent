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

    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");
    let model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.4".to_string());

    let approval_mode = if auto_approve {
        ApprovalMode::AutoApprove
    } else {
        ApprovalMode::default_interactive()
    };

    let compaction_config = CompactionConfig::default();

    // Build agent graph for a persona
    let build_agent = |persona: &Persona, cwd: &str, api_key: &str, model_name: &str, approval_mode: ApprovalMode| {
        let system_prompt = persona.build_system_prompt(&skills_dir, cwd);
        let tool_config = metalcraft_agent::tools::ToolConfig {
            api_key: api_key.to_string(),
            model_name: model_name.to_string(),
            system_prompt: system_prompt.clone(),
            skills_dir: skills_dir.clone(),
            available_skills: persona.skills.clone(),
        };
        let registry = metalcraft_agent::tools::create_registry_for_with_config(
            &persona.tools,
            Some(&tool_config),
        );
        let client = openai::Client::new(api_key)?;
        let model = client.completion_model(model_name);
        let compaction_model = client.completion_model(model_name);
        let hook = approval::build_hook(approval_mode);
        let graph = create_react_agent_with_hooks(model, registry, &system_prompt, hook)?.into_arc();
        Ok::<_, Box<dyn std::error::Error>>((graph, compaction_model))
    };

    fn print_persona_banner(persona: &Persona, persona_slug: &str, model_name: &str, auto_approve: bool) {
        println!("╭─────────────────────────────────────────────╮");
        println!("│  Persona: {:<33}│", persona.name);
        println!("│  Slug:    {:<33}│", persona_slug);
        println!("│  Model:   {:<33}│", model_name);
        println!("╰─────────────────────────────────────────────╯");
        println!("  {}", persona.description);
        println!("  Tools: {}", persona.tools.join(", "));
        if !persona.skills.is_empty() {
            println!("  Skills: {}", persona.skills.join(", "));
        }
        if auto_approve {
            println!("  Mode: auto-approve");
        } else {
            println!("  Mode: interactive (read-only tools auto-approved)");
        }
        println!();
    }

    print_persona_banner(&persona, persona_slug, &model_name, auto_approve);

    let (mut graph, mut compaction_model) = build_agent(&persona, &cwd, &api_key, &model_name, approval_mode.clone())?;
    let step_guard = guard::build_agent_guard(guard::GuardConfig::default());
    let mut current_persona_slug = persona_slug.to_string();

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
    println!("Interactive mode. Commands: /quit, /clear, /tokens, /persona [list|set <name>]\n");

    let mut rl = DefaultEditor::new()?;
    let mut prompt_str = format!("[{}]> ", current_persona_slug);
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
        if input == "/persona" || input == "/persona list" {
            println!("Current persona: {}\n", current_persona_slug);
            println!("Available personas:");
            let available = Persona::list_available(&personas_dir);
            for slug in &available {
                let marker = if *slug == current_persona_slug { " <-- active" } else { "" };
                if let Ok(p) = Persona::load(slug, &personas_dir) {
                    println!("  {:<24} {}{}", slug, p.description, marker);
                } else {
                    println!("  {}", slug);
                }
            }
            println!("\nUse: /persona set <name>");
            println!();
            continue;
        }
        if let Some(new_slug) = input.strip_prefix("/persona set ") {
            let new_slug = new_slug.trim();
            match Persona::load(new_slug, &personas_dir) {
                Ok(new_persona) => {
                    match build_agent(&new_persona, &cwd, &api_key, &model_name, approval_mode.clone()) {
                        Ok((new_graph, new_compaction_model)) => {
                            graph = new_graph;
                            compaction_model = new_compaction_model;
                            current_persona_slug = new_slug.to_string();
                            prompt_str = format!("[{}]> ", current_persona_slug);
                            state = None;
                            println!();
                            print_persona_banner(&new_persona, new_slug, &model_name, auto_approve);
                            println!("Conversation cleared (new persona context).\n");
                        }
                        Err(e) => {
                            eprintln!("Failed to build agent for '{}': {}\n", new_slug, e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{}\n", e);
                    println!("Available: {:?}\n", Persona::list_available(&personas_dir));
                }
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
