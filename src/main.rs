use metalcraft::{AgentState, Executor, LlmCallHook, RunOutcome};
use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::cli;
use metalcraft_agent::context::{self, CompactionConfig};
use metalcraft_agent::diagnostics::DiagnosticsLogger;
use metalcraft_agent::guard;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{self, AgentRuntimeContext, AVAILABLE_MODELS, DEFAULT_MODEL};
use metalcraft_agent::ui;
use metalcraft_agent::workshop_api::{self, WorkshopApiConfig};
use rig::client::CompletionClient;
use rustyline::DefaultEditor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn resolve_cd_target(input: &str, current: &Path) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("missing path".to_string());
    }

    let expanded: PathBuf = if trimmed == "~" {
        PathBuf::from(std::env::var("HOME").map_err(|_| "HOME not set".to_string())?)
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(trimmed)
    };

    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        current.join(expanded)
    };

    let canonical = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("{}: {}", candidate.display(), e))?;

    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }

    Ok(canonical)
}

fn display_cwd(cwd: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = cwd.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    cwd.to_string()
}

fn build_prompt_str(persona_slug: &str, cwd: &str) -> String {
    let basename = Path::new(cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cwd);
    format!("[{} {}]> ", persona_slug, basename)
}

fn print_usage(personas_dir: &std::path::Path) {
    eprintln!("{} {}", ui::error("Usage:"), ui::command("metalcraft-agent [--auto-approve] [--persona <slug>] [task]"));
    eprintln!();
    eprintln!("  If [task] is given, run once and exit.");
    eprintln!("  If [task] is omitted, enter interactive mode.");
    eprintln!("  --persona <slug>  Persona to use (default: orchestrator-agent; also METALCRAFT_PERSONA).");
    eprintln!("  --auto-approve    Skip approval prompts for all tools.");
    eprintln!();
    let available = Persona::list_available(personas_dir);
    if available.is_empty() {
        eprintln!("{} {}", ui::warning("No personas found in"), ui::path(personas_dir.display().to_string()));
    } else {
        eprintln!("{}", ui::heading("Available personas:"));
        for slug in &available {
            if let Ok(p) = Persona::load(slug, personas_dir) {
                eprintln!("  {:<20} {}", ui::accent(slug), p.description);
            } else {
                eprintln!("  {}", ui::accent(slug));
            }
        }
    }
}

fn print_persona_banner(persona: &Persona, persona_slug: &str, model_name: &str, cwd: &str, auto_approve: bool) {
    println!("{}", ui::heading("╭─────────────────────────────────────────────╮"));
    println!("│  {} {:<33}│", ui::label("Persona:"), persona.name);
    println!("│  {} {:<33}│", ui::label("Slug:"), persona_slug);
    println!("│  {} {:<33}│", ui::label("Model:"), model_name);
    println!("{}", ui::heading("╰─────────────────────────────────────────────╯"));
    println!("  {}", persona.description);
    println!("  {} {}", ui::label("Cwd:"), ui::path(display_cwd(cwd)));
    println!("  {} {}", ui::label("Tools:"), persona.tools.join(", "));
    if !persona.skills.is_empty() {
        println!("  {} {}", ui::label("Skills:"), persona.skills.join(", "));
    }
    if auto_approve {
        println!("  {} {}", ui::label("Mode:"), ui::success("auto-approve"));
    } else {
        println!("  {} {}", ui::label("Mode:"), ui::dim("interactive (read-only tools auto-approved)"));
    }
    println!();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env FIRST so a file-provided METALCRAFT_DATA_DIR is honored by
    // seeding (which resolves the data dir) and by RUST_LOG below.
    dotenvy::dotenv().ok();
    env_logger::init();

    metalcraft_agent::seed::ensure_defaults();

    let runtime_context = AgentRuntimeContext::from_environment()?;

    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    let invocation = match cli::parse_cli_invocation(&raw_args) {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("{} {}", ui::error("Error:"), e);
            print_usage(&runtime_context.personas_dir);
            std::process::exit(1);
        }
    };

    // Workshop API server mode. Triggered by `--api [KEY]` or by a
    // WORKSHOP_API_KEY in the environment (the env-only trigger preserves the
    // historical behavior of running the server when the key is exported).
    let api_key = invocation
        .api_key
        .clone()
        .or_else(|| std::env::var("WORKSHOP_API_KEY").ok());
    if invocation.api_requested || api_key.is_some() {
        let key = match api_key {
            Some(k) => k,
            None => {
                eprintln!("{} --api requires an API key: --api <KEY> or set WORKSHOP_API_KEY", ui::error("Error:"));
                std::process::exit(1);
            }
        };

        let api_port: u16 = invocation
            .api_port
            .or_else(|| std::env::var("WORKSHOP_API_PORT").ok().and_then(|p| p.parse().ok()))
            .unwrap_or(3002);

        workshop_api::start(WorkshopApiConfig {
            port: api_port,
            api_key: key,
        })
        .await;
        return Ok(());
    }

    let auto_approve = invocation.auto_approve;

    // Persona resolution: explicit `--persona/-p`, else METALCRAFT_PERSONA, else
    // the Orchestrator — the default agent, which delegates the actual work via
    // sub_agent rather than requiring the caller to pick a specialist up front.
    let persona_slug_owned = invocation
        .persona
        .clone()
        .or_else(|| std::env::var("METALCRAFT_PERSONA").ok())
        .unwrap_or_else(|| "orchestrator-agent".to_string());
    let persona_slug = persona_slug_owned.as_str();
    let one_shot_task = invocation.task.clone();

    let persona = Persona::load(persona_slug, &runtime_context.personas_dir)
        .map_err(|e| {
            eprintln!("{} {}", ui::error("Error:"), e);
            print_usage(&runtime_context.personas_dir);
            std::process::exit(1);
        })
        .unwrap();

    let mut cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let mut model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let available_models = AVAILABLE_MODELS.to_vec();

    let is_headless = !atty::is(atty::Stream::Stdin);
    let approval_mode = if auto_approve || is_headless {
        if is_headless && !auto_approve {
            eprintln!("{} {}", ui::warning("Notice:"), "no TTY detected, auto-approving all tools");
        }
        ApprovalMode::AutoApprove
    } else {
        ApprovalMode::default_interactive()
    };

    let compaction_config = CompactionConfig::default();

    print_persona_banner(&persona, persona_slug, &model_name, &cwd, auto_approve);

    let diagnostics: Arc<DiagnosticsLogger> = match DiagnosticsLogger::new() {
        Ok(logger) => {
            let system_prompt = persona.build_system_prompt(&runtime_context.skills_dir, &cwd);
            logger.log_session_info(
                &persona.name,
                persona_slug,
                &model_name,
                &cwd,
                &system_prompt,
                &persona.tools,
                &persona.skills,
                auto_approve,
                None,
            );
            println!("  {} {}\n", ui::label("Session:"), ui::path(logger.session_dir().display().to_string()));
            Arc::new(logger)
        }
        Err(e) => {
            return Err(format!("failed to create session logger: {e}").into());
        }
    };

    let llm_call_hook: LlmCallHook = {
        let logger = diagnostics.clone();
        Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
            logger.log_llm_request(snapshot);
        }) as LlmCallHook
    };

    let runtime::BuiltAgentRuntime {
        mut graph,
        mut compaction_model,
    } = runtime::build_agent_runtime(
        &runtime_context,
        &persona,
        &cwd,
        &model_name,
        approval_mode.clone(),
        Some(llm_call_hook.clone()),
        None, // CLI runs don't emit OTLP traces
        |client, model_name| client.completion_model(model_name),
    )?;
    let step_guard = guard::build_agent_guard(guard::GuardConfig::default(), Some(diagnostics.clone()));
    let mut current_persona_slug = persona_slug.to_string();

    if let Some(task) = one_shot_task {
        println!("{} {}\n", ui::label("Task:"), task);

        match runtime::run_one_shot_task(
            &runtime_context,
            runtime::RunOneShotRequest {
                persona_slug,
                cwd: &cwd,
                model_name: &model_name,
                task: &task,
                approval_mode: approval_mode.clone(),
                diagnostics: Some(diagnostics.clone()),
            },
        )
        .await?
        {
            RunOutcome::Completed(state) => {
                println!("\n{}", ui::success("--- Done ---"));
                println!("{}", state.final_answer().unwrap_or("(no answer)"));
            }
            RunOutcome::Interrupted { reason, .. } => {
                println!("\n{} {reason}", ui::warning("Interrupted:"));
            }
            RunOutcome::Failed { node, error, .. } => {
                println!("\n{} {node}: {error}", ui::warning("Failed:"));
            }
        }
        return Ok(());
    }

    if is_headless {
        eprintln!("{} no TTY and no task provided. Use: metalcraft-agent \"<task>\" (optionally --persona <slug>)", ui::error("Error:"));
        std::process::exit(1);
    }

    println!(
        "{} {}\n",
        ui::heading("Interactive mode."),
        ui::dim("Commands: /quit, /clear, /tokens, /cd [path], /persona [list|set <name>], /model [list|use <name>]")
    );

    let mut rl = DefaultEditor::new()?;
    let mut prompt_str = build_prompt_str(&current_persona_slug, &cwd);
    let mut state: Option<AgentState> = None;

    loop {
        let line = match rl.readline(&prompt_str) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof) => {
                println!("\n{}", ui::dim("Bye."));
                break;
            }
            Err(e) => {
                eprintln!("{} {}", ui::error("Input error:"), e);
                break;
            }
        };

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" {
            println!("{}", ui::dim("Bye."));
            break;
        }
        if input == "/clear" {
            state = None;
            println!("{}\n", ui::dim("Conversation cleared."));
            continue;
        }
        if input == "/tokens" {
            match &state {
                Some(s) => println!("{} ~{} tokens, {} messages\n", ui::label("Context:"), context::estimate_tokens(s), s.messages.len()),
                None => println!("{}\n", ui::dim("No conversation yet.")),
            }
            continue;
        }
        if input == "/cd" {
            println!("{} {}\n", ui::label("Working directory:"), ui::path(display_cwd(&cwd)));
            continue;
        }
        if let Some(target) = input.strip_prefix("/cd ") {
            let current_path = PathBuf::from(&cwd);
            match resolve_cd_target(target, &current_path) {
                Ok(new_path) => {
                    if let Err(e) = std::env::set_current_dir(&new_path) {
                        eprintln!("{} {}\n", ui::error("/cd: failed to change directory:"), e);
                        continue;
                    }
                    cwd = new_path.display().to_string();
                    let current_persona = Persona::load(&current_persona_slug, &runtime_context.personas_dir).unwrap();
                    match runtime::build_agent_runtime(
                        &runtime_context,
                        &current_persona,
                        &cwd,
                        &model_name,
                        approval_mode.clone(),
                        Some(llm_call_hook.clone()),
                        None, // CLI runs don't emit OTLP traces
                        |client, model_name| client.completion_model(model_name),
                    ) {
                        Ok(runtime::BuiltAgentRuntime {
                            graph: new_graph,
                            compaction_model: new_compaction_model,
                        }) => {
                            graph = new_graph;
                            compaction_model = new_compaction_model;
                            prompt_str = build_prompt_str(&current_persona_slug, &cwd);
                            println!("{} {}\n", ui::success("Working directory:"), ui::path(display_cwd(&cwd)));
                        }
                        Err(e) => {
                            eprintln!("{} {}\n", ui::error("/cd: failed to rebuild agent:"), e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{} {}\n", ui::error("/cd:"), e);
                }
            }
            continue;
        }
        if input == "/persona" || input == "/persona list" {
            println!("{} {}\n", ui::label("Current persona:"), ui::accent(&current_persona_slug));
            println!("{}", ui::heading("Available personas:"));
            let available = Persona::list_available(&runtime_context.personas_dir);
            for slug in &available {
                let marker = if *slug == current_persona_slug { format!(" {}", ui::success("<-- active")) } else { String::new() };
                if let Ok(p) = Persona::load(slug, &runtime_context.personas_dir) {
                    println!("  {:<24} {}{}", ui::accent(slug), p.description, marker);
                } else {
                    println!("  {}", ui::accent(slug));
                }
            }
            println!("\n{}", ui::dim("Use: /persona set <name>"));
            println!();
            continue;
        }
        if let Some(new_slug) = input.strip_prefix("/persona set ") {
            let new_slug = new_slug.trim();
            match Persona::load(new_slug, &runtime_context.personas_dir) {
                Ok(new_persona) => {
                    match runtime::build_agent_runtime(
                        &runtime_context,
                        &new_persona,
                        &cwd,
                        &model_name,
                        approval_mode.clone(),
                        Some(llm_call_hook.clone()),
                        None, // CLI runs don't emit OTLP traces
                        |client, model_name| client.completion_model(model_name),
                    ) {
                        Ok(runtime::BuiltAgentRuntime {
                            graph: new_graph,
                            compaction_model: new_compaction_model,
                        }) => {
                            graph = new_graph;
                            compaction_model = new_compaction_model;
                            current_persona_slug = new_slug.to_string();
                            prompt_str = build_prompt_str(&current_persona_slug, &cwd);
                            state = None;
                            println!();
                            print_persona_banner(&new_persona, new_slug, &model_name, &cwd, auto_approve);
                            diagnostics.log_config_change("persona_switch", serde_json::json!({
                                "new_persona": new_slug,
                                "model": &model_name,
                            }));
                            println!("{}\n", ui::dim("Conversation cleared (new persona context)."));
                        }
                        Err(e) => {
                            eprintln!("{} '{}': {}\n", ui::error("Failed to build agent for"), new_slug, e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{}\n", ui::error(e));
                    println!("{} {:?}\n", ui::label("Available:"), Persona::list_available(&runtime_context.personas_dir));
                }
            }
            continue;
        }
        if input == "/model" || input == "/model list" {
            println!("{} {}\n", ui::label("Current model:"), ui::accent(&model_name));
            println!("{}", ui::heading("Available models:"));
            for m in &available_models {
                let marker = if *m == model_name { format!(" {}", ui::success("<-- active")) } else { String::new() };
                println!("  {}{}", ui::accent(m), marker);
            }
            println!("\n{}", ui::dim("Use: /model use <name>"));
            println!();
            continue;
        }
        if let Some(new_model) = input.strip_prefix("/model use ") {
            let new_model = new_model.trim();
            if !available_models.contains(&new_model) {
                eprintln!("{} '{}' . {}\n", ui::error("Unknown model"), new_model, ui::dim(format!("Available: {}", available_models.join(", "))));
                continue;
            }
            let current_persona = Persona::load(&current_persona_slug, &runtime_context.personas_dir).unwrap();
            match runtime::build_agent_runtime(
                &runtime_context,
                &current_persona,
                &cwd,
                new_model,
                approval_mode.clone(),
                Some(llm_call_hook.clone()),
                None, // CLI runs don't emit OTLP traces
                |client, model_name| client.completion_model(model_name),
            ) {
                Ok(runtime::BuiltAgentRuntime {
                    graph: new_graph,
                    compaction_model: new_compaction_model,
                }) => {
                    model_name = new_model.to_string();
                    graph = new_graph;
                    compaction_model = new_compaction_model;
                    state = None;
                    println!();
                    print_persona_banner(&current_persona, &current_persona_slug, &model_name, &cwd, auto_approve);
                    diagnostics.log_config_change("model_switch", serde_json::json!({
                        "new_model": new_model,
                        "persona": &current_persona_slug,
                    }));
                    println!("{}\n", ui::dim("Conversation cleared (new model context)."));
                }
                Err(e) => {
                    eprintln!("{} '{}': {}\n", ui::error("Failed to switch to model"), new_model, e);
                }
            }
            continue;
        }

        let _ = rl.add_history_entry(input);

        let mut turn_state = match state.take() {
            Some(prev) => prev.continue_with(input),
            None => AgentState::new(input),
        };

        match context::compact_if_needed(&mut turn_state, &compaction_model, &compaction_config).await {
            Ok(true) => {
                println!("{} ~{} tokens", ui::dim("(context compacted to"), context::estimate_tokens(&turn_state));
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("{} {}", ui::warning("Warning: compaction failed:"), e);
            }
        }

        let executor = Executor::new_from_arc(graph.clone()).max_steps(90).with_step_guard(step_guard.clone());
        let outcome = executor.run(turn_state, "agent").await;

        match outcome {
            Ok(RunOutcome::Completed(completed_state)) => {
                println!("\n{}", completed_state.final_answer().unwrap_or("(no answer)"));
                state = Some(completed_state);
            }
            Ok(RunOutcome::Interrupted { state: s, reason, .. }) => {
                println!("\n{} {reason}", ui::warning("Interrupted:"));
                state = Some(s);
            }
            Ok(RunOutcome::Failed { state: s, node, error }) => {
                eprintln!("\n{} {node}: {error}", ui::error("Failed:"));
                // Keep the partial state so the next REPL turn can continue.
                state = Some(s);
            }
            Err(e) => {
                eprintln!("\n{} {}", ui::error("Error:"), e);
                state = None;
            }
        }
        println!();
    }

    Ok(())
}
