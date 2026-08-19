use metalcraft::{AgentState, LlmCallHook, RunOutcome};
use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::cli;
use metalcraft_agent::context;
use metalcraft_agent::diagnostics::DiagnosticsLogger;
use metalcraft_agent::guard;
use metalcraft_agent::agent_preset::{AgentPreset, DEFAULT_PRESET};
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
    eprintln!("{} {}", ui::error("Usage:"), ui::command("metalcraft-agent [--auto-approve] [--preset <slug>] [--persona <slug>] [task]"));
    eprintln!();
    eprintln!("  If [task] is given, run once and exit.");
    eprintln!("  If [task] is omitted, enter interactive mode.");
    eprintln!("  --preset <slug>   Agent preset to run as (default: general-agent; also METALCRAFT_PRESET).");
    eprintln!("  --persona <slug>  Persona to use; overrides the preset's default (also METALCRAFT_PERSONA).");
    eprintln!("  --auto-approve    Skip approval prompts for all tools.");
    eprintln!("  --migrate-agent-packs [--dry-run]");
    eprintln!("                    Wrap legacy integration packs into agent packs, then exit.");
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

    // Workshop API server mode. Triggered by `--api [KEY]`, a WORKSHOP_API_KEY in
    // the environment (the env-only trigger preserves the historical behavior of
    // running the server when the key is exported), or WORKSHOP_API_ENABLED for
    // OIDC-only mode (managed pods that mint no static key).
    let api_key = invocation
        .api_key
        .clone()
        .or_else(|| std::env::var("WORKSHOP_API_KEY").ok())
        .filter(|s| !s.is_empty());
    let api_oidc = matches!(
        std::env::var("WORKSHOP_API_ENABLED")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    );
    if invocation.api_requested || api_key.is_some() || api_oidc {
        // OIDC-only mode runs with an empty key (static-bearer path disabled,
        // callers authenticate via Metalcraft ID tokens). A bare `--api` with no
        // key and no OIDC opt-in is still an error — an all-rejecting endpoint.
        let key = match (api_key, api_oidc) {
            (Some(k), _) => k,
            (None, true) => String::new(),
            (None, false) => {
                eprintln!("{} --api requires an API key (--api <KEY> or WORKSHOP_API_KEY) or WORKSHOP_API_ENABLED=1 for OIDC-only", ui::error("Error:"));
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

    // Migration is explicit and terminal: run it, print the report, exit. It must
    // never happen on boot — an upgraded pod restructures its data dir when the
    // operator says so, not because it restarted.
    if invocation.migrate_agent_packs {
        let report = metalcraft_agent::agent_packs::migrate::run(invocation.dry_run);
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("{} {e}", ui::error("could not render the report:")),
        }
        let failed = report.failed.len();
        if failed > 0 {
            eprintln!(
                "\n{} {failed} integration pack(s) could not be wrapped; they are unchanged.",
                ui::warning("Warning:")
            );
        }
        std::process::exit(if failed > 0 { 1 } else { 0 });
    }

    let auto_approve = invocation.auto_approve;

    // Agent preset resolution: explicit `--preset`, else METALCRAFT_PRESET, else the
    // built-in `general-agent`. The preset is what a user picks; the persona is an
    // implementation detail of it.
    let preset_slug_owned = invocation
        .preset
        .clone()
        .or_else(|| std::env::var("METALCRAFT_PRESET").ok());
    let presets_dir = metalcraft_agent::paths::agent_presets_dir();
    let active_preset =
        AgentPreset::load(preset_slug_owned.as_deref().unwrap_or(DEFAULT_PRESET), &presets_dir).ok();

    // Persona resolution: explicit `--persona/-p` wins, then METALCRAFT_PERSONA, then
    // the active preset's default persona, then the Orchestrator — the agent that
    // delegates the actual work via sub_agent rather than requiring the caller to pick
    // a specialist up front.
    let persona_slug_owned = invocation
        .persona
        .clone()
        .or_else(|| std::env::var("METALCRAFT_PERSONA").ok())
        .or_else(|| active_preset.as_ref().map(|p| p.default_persona.clone()))
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

    // Build the turn runner once and reuse it for the whole session (cheap: no
    // per-turn graph/client rebuild). It's rebuilt only when /cd, /persona, or
    // /model change what the runtime must be.
    let mut turn_runner = runtime::TurnRunner::new(runtime::build_agent_runtime(
        &runtime_context,
        &persona,
        &cwd,
        &model_name,
        approval_mode.clone(),
        Some(llm_call_hook.clone()),
        None, // CLI runs don't emit OTLP traces
        runtime::RuntimeOptions {
            prompt_extras: metalcraft_agent::persona::PromptExtras::load().await,
            // sub_agent may only delegate inside the active preset's roster.
            preset_personas: active_preset.as_ref().map(|p| p.callable_personas()),
            instance_id: None,
            ..Default::default()
        },
        |client, model_name| client.completion_model(model_name),
    )?);
    // One session-long step guard (loop/error-spiral tracker), reused across
    // persona/model switches so its history survives them.
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
                // The CLI runs against the pod-global memory, not an agent instance;
                // sub_agent still obeys the active preset's roster.
                instance_id: None,
                preset_personas: active_preset.as_ref().map(|p| p.callable_personas()),
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
                        runtime::RuntimeOptions {
                            prompt_extras: metalcraft_agent::persona::PromptExtras::load().await,
                            preset_personas: active_preset.as_ref().map(|p| p.callable_personas()),
                            instance_id: None,
                            ..Default::default()
                        },
                        |client, model_name| client.completion_model(model_name),
                    ) {
                        Ok(built) => {
                            turn_runner = runtime::TurnRunner::new(built);
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
        if input == "/preset" || input == "/preset list" {
            match &active_preset {
                Some(p) => {
                    println!("{} {}", ui::label("Current agent:"), ui::accent(&p.slug));
                    println!("  {}\n", p.name);
                    println!("{}", ui::heading("Personas it can call:"));
                    for slug in p.callable_personas() {
                        let marker = if slug == p.default_persona { " (default)" } else { "" };
                        println!("  {}{}", ui::accent(&slug), marker);
                    }
                }
                None => println!("{}", ui::warning("No agent preset active.")),
            }
            println!();
            println!("{}", ui::heading("Available agents:"));
            for summary in AgentPreset::list_summaries(&presets_dir) {
                println!("  {:<20} {}", ui::accent(&summary.slug), summary.description);
            }
            println!();
            println!("{}", ui::label("Switching agents starts a fresh session — restart with --preset <slug>."));
            println!();
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
                        runtime::RuntimeOptions {
                            prompt_extras: metalcraft_agent::persona::PromptExtras::load().await,
                            preset_personas: active_preset.as_ref().map(|p| p.callable_personas()),
                            instance_id: None,
                            ..Default::default()
                        },
                        |client, model_name| client.completion_model(model_name),
                    ) {
                        Ok(built) => {
                            turn_runner = runtime::TurnRunner::new(built);
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
                runtime::RuntimeOptions {
                    prompt_extras: metalcraft_agent::persona::PromptExtras::load().await,
                    preset_personas: active_preset.as_ref().map(|p| p.callable_personas()),
                    instance_id: None,
                    ..Default::default()
                },
                |client, model_name| client.completion_model(model_name),
            ) {
                Ok(built) => {
                    turn_runner = runtime::TurnRunner::new(built);
                    model_name = new_model.to_string();
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

        let turn_state = match state.take() {
            Some(prev) => prev.continue_with(input),
            None => AgentState::new(input),
        };

        // The runner compacts the context (if needed) then runs the turn; a
        // compaction failure is logged inside and the turn proceeds uncompacted.
        let (compacted, outcome) = turn_runner.run(turn_state, step_guard.clone()).await;
        if compacted {
            println!("{}", ui::dim("(context compacted)"));
        }

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
