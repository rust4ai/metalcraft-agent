use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::diagnostics::DiagnosticsLogger;
use metalcraft_agent::event_listener::{self, EventListenerConfig};
use metalcraft_agent::flows::{self, FlowSchedule};
use metalcraft_agent::paths;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{self, AgentRuntimeContext, RunOneShotRequest, DEFAULT_MODEL};
use metalcraft_agent::workshop_api;

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;

struct FlowRunState {
    last_started_at: Option<chrono::DateTime<Local>>,
    is_running: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env FIRST so a file-provided METALCRAFT_DATA_DIR is honored by
    // seeding (which resolves the data dir) and by RUST_LOG below. Otherwise
    // seeding lands in the fallback dir while the runtime reads the override.
    dotenvy::dotenv().ok();
    env_logger::init();

    metalcraft_agent::seed::ensure_defaults();

    let mut flows_dir = paths::flows_dir();
    let mut persona_slug = "coding-agent".to_string();
    let mut model_name = DEFAULT_MODEL.to_string();
    let mut poll_seconds: u64 = 30;
    let mut once = false;
    let mut auto_approve = false;

    // Event listener options
    let mut event_port: u16 = std::env::var("EVENTD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);
    let mut event_host = std::env::var("EVENTD_HOST")
        .unwrap_or_else(|_| "localhost".into());
    let mut event_persona: Option<String> = None;
    let mut event_types: Vec<String> = vec!["message_create".into()];
    let mut event_platforms: Option<Vec<String>> = None;
    let mut admin_user_ids: Vec<String> = std::env::var("EVENTD_ADMIN_USER_IDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Workshop API options. `--api <KEY>` enables it; `WORKSHOP_API_KEY` env
    // var works too so Railway / Docker can drive it without flag wiring.
    let mut workshop_api_key: Option<String> = std::env::var("WORKSHOP_API_KEY").ok();
    let mut workshop_api_port: u16 = std::env::var("WORKSHOP_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or_else(|| std::env::var("PORT").ok().and_then(|p| p.parse().ok()))
        .unwrap_or(3002);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--flows-dir" => {
                let value = args.next().ok_or("--flows-dir requires a path")?;
                flows_dir = PathBuf::from(value);
            }
            "--persona" => {
                persona_slug = args.next().ok_or("--persona requires a value")?;
            }
            "--model" => {
                model_name = args.next().ok_or("--model requires a value")?;
            }
            "--poll-seconds" => {
                let value = args.next().ok_or("--poll-seconds requires a value")?;
                poll_seconds = value.parse()?;
            }
            "--once" => {
                once = true;
            }
            "--auto-approve" => {
                auto_approve = true;
            }
            "--event-port" => {
                let value = args.next().ok_or("--event-port requires a value")?;
                event_port = value.parse()?;
            }
            "--event-host" => {
                event_host = args.next().ok_or("--event-host requires a value")?;
            }
            "--event-persona" => {
                event_persona = Some(args.next().ok_or("--event-persona requires a value")?);
            }
            "--events" => {
                let value = args.next().ok_or("--events requires a value")?;
                event_types = value.split(',').map(|s| s.trim().to_string()).collect();
            }
            "--platforms" => {
                let value = args.next().ok_or("--platforms requires a value")?;
                event_platforms = Some(value.split(',').map(|s| s.trim().to_string()).collect());
            }
            "--admin-user-ids" => {
                let value = args.next().ok_or("--admin-user-ids requires a value")?;
                admin_user_ids = value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
            "--api" => {
                workshop_api_key = Some(args.next().ok_or("--api requires a key")?);
            }
            "--api-port" => {
                let value = args.next().ok_or("--api-port requires a value")?;
                workshop_api_port = value.parse()?;
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => {
                return Err(format!("unknown argument: {other}").into());
            }
        }
    }

    let context = AgentRuntimeContext::from_environment()?;
    let approval_mode = if auto_approve {
        ApprovalMode::AutoApprove
    } else {
        ApprovalMode::default_interactive()
    };
    let cwd = std::env::current_dir()?.display().to_string();

    log::info!(
        "Starting metalcraft-daemon with flows_dir={}, persona={}, model={}, poll_seconds={}, once={}",
        flows_dir.display(),
        persona_slug,
        model_name,
        poll_seconds,
        once
    );

    // Spawn the workshop admin API if a key was supplied. Runs alongside
    // the flow scheduler so a single daemon process can both run flows and
    // serve project edits from the workshop desktop app.
    if let Some(key) = workshop_api_key.clone() {
        let port = workshop_api_port;
        let router = workshop_api::build_router(key);
        tokio::spawn(async move {
            workshop_api::serve(port, router).await;
        });
        log::info!("Workshop API spawned on port {port}");
    }

    // Spawn event listener if gateway is configured
    let mut event_listener_enabled = false;
    if std::env::var("AGENT_GATEWAY_URL").is_ok() {
        let webhook_secret = match std::env::var("EVENTD_WEBHOOK_SECRET") {
            Ok(s) if !s.is_empty() => s,
            _ => {
                log::error!("EVENTD_WEBHOOK_SECRET is required when AGENT_GATEWAY_URL is set. The event listener will not accept unauthenticated webhooks.");
                return Err("Missing required env var: EVENTD_WEBHOOK_SECRET".into());
            }
        };

        if admin_user_ids.is_empty() {
            log::error!("EVENTD_ADMIN_USER_IDS (or --admin-user-ids) is required when AGENT_GATEWAY_URL is set. Set it to a comma-separated list of Discord/platform user IDs allowed to trigger the agent.");
            return Err("Missing required config: admin user IDs".into());
        }

        if std::env::var("AGENT_GATEWAY_API_KEY").unwrap_or_default().is_empty() {
            log::error!("AGENT_GATEWAY_API_KEY is required when AGENT_GATEWAY_URL is set.");
            return Err("Missing required env var: AGENT_GATEWAY_API_KEY".into());
        }

        log::info!(
            "Event listener: {} admin user(s) configured, listening for {:?}",
            admin_user_ids.len(),
            event_types,
        );

        let listener_config = EventListenerConfig {
            port: event_port,
            host: event_host,
            persona_slug: event_persona.unwrap_or_else(|| persona_slug.clone()),
            model_name: model_name.clone(),
            events: event_types,
            platforms: event_platforms,
            webhook_secret,
            admin_user_ids,
            approval_mode: approval_mode.clone(),
            cwd: cwd.clone(),
        };

        let listener_context = AgentRuntimeContext::from_environment()?;
        tokio::spawn(async move {
            event_listener::start(listener_config, listener_context).await;
        });
        log::info!("Event listener spawned on port {event_port}");
        event_listener_enabled = true;
    }

    // Always-visible startup banner. Unlike the log::info! lines above, this
    // prints regardless of RUST_LOG (env_logger defaults to `error`), so a bare
    // `./metalcraft-daemon` isn't silent.
    println!("──────────────────────────────────────────────");
    println!("  metalcraft-daemon running");
    println!("  persona:        {persona_slug}");
    println!("  model:          {model_name}");
    println!("  flows dir:      {}", flows_dir.display());
    if once {
        println!("  mode:           run once, then exit");
    } else {
        println!("  mode:           polling every {poll_seconds}s");
    }
    match &workshop_api_key {
        Some(_) => println!("  workshop API:   enabled on port {workshop_api_port}"),
        None => println!("  workshop API:   disabled (pass --api <KEY> to enable)"),
    }
    if event_listener_enabled {
        println!("  event listener: enabled on port {event_port}");
    } else {
        println!("  event listener: disabled (set AGENT_GATEWAY_URL to enable)");
    }
    println!("──────────────────────────────────────────────");

    // Flow polling loop
    let mut state_by_flow: HashMap<String, FlowRunState> = HashMap::new();

    loop {
        // Run one polling iteration, but let Ctrl+C cancel it. The workshop API
        // and event listener install tokio's SIGINT handler via
        // `with_graceful_shutdown`, which replaces the OS default of killing the
        // process. Without selecting on ctrl_c here too, the main loop would keep
        // polling forever and Ctrl+C would only stop the spawned servers.
        let iteration = async {
        let runnable = flows::load_enabled_flows(&flows_dir);
        for flow in runnable {
            let flow_id = flow.saved.id.clone();
            let due = is_due(state_by_flow.get(&flow_id), &flow.schedule);
            if !due {
                continue;
            }

            let state = state_by_flow.entry(flow_id.clone()).or_insert(FlowRunState {
                last_started_at: None,
                is_running: false,
            });

            if state.is_running {
                log::warn!("Skipping flow '{}' because a previous run is still marked active", flow_id);
                continue;
            }

            state.is_running = true;
            state.last_started_at = Some(Local::now());

            log::info!("Running flow '{}' ({})", flow.saved.id, flow.saved.name);

            match flows::collect_reachable_prompts(&flow.saved) {
                Ok(prompts) => {
                    if prompts.is_empty() {
                        log::warn!("Flow '{}' has no reachable prompt nodes", flow.saved.id);
                    }
                    for (index, prompt) in prompts.iter().enumerate() {
                        // Per-prompt/flow persona wins; otherwise fall back to --persona.
                        let effective_persona = prompt.persona.as_deref().unwrap_or(&persona_slug);
                        log::info!(
                            "Flow '{}' prompt {}/{} (persona: {})",
                            flow.saved.id,
                            index + 1,
                            prompts.len(),
                            effective_persona
                        );

                        let logger = DiagnosticsLogger::new().ok().map(Arc::new);
                        if let Some(ref diagnostics) = logger {
                            if let Ok(persona) = Persona::load(effective_persona, &context.personas_dir) {
                                let system_prompt = persona.build_system_prompt(&context.skills_dir, &cwd);
                                diagnostics.log_session_info(
                                    &persona.name,
                                    effective_persona,
                                    &model_name,
                                    &cwd,
                                    &system_prompt,
                                    &persona.tools,
                                    &persona.skills,
                                    matches!(approval_mode, ApprovalMode::AutoApprove),
                                    Some(&flow.saved.id),
                                );
                            }
                        }

                        let outcome = runtime::run_one_shot_task(
                            &context,
                            RunOneShotRequest {
                                persona_slug: effective_persona,
                                cwd: &cwd,
                                model_name: &model_name,
                                task: &prompt.prompt,
                                approval_mode: approval_mode.clone(),
                                diagnostics: logger,
                            },
                        )
                        .await;

                        match outcome {
                            Ok(metalcraft::RunOutcome::Completed(state)) => {
                                log::info!(
                                    "Flow '{}' prompt completed: {}",
                                    flow.saved.id,
                                    state.final_answer().unwrap_or("(no answer)")
                                );
                            }
                            Ok(metalcraft::RunOutcome::Interrupted { reason, .. }) => {
                                log::warn!("Flow '{}' prompt interrupted: {}", flow.saved.id, reason);
                                break;
                            }
                            Err(err) => {
                                log::error!("Flow '{}' prompt failed: {}", flow.saved.id, err);
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    log::error!("Flow '{}' is not runnable: {}", flow.saved.id, err);
                }
            }

            if let Some(state) = state_by_flow.get_mut(&flow_id) {
                state.is_running = false;
            }
        }
        };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                log::info!("Received Ctrl+C — shutting down metalcraft-daemon");
                break;
            }
            _ = iteration => {}
        }

        if once {
            break;
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                log::info!("Received Ctrl+C — shutting down metalcraft-daemon");
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(poll_seconds)) => {}
        }
    }

    Ok(())
}

fn is_due(state: Option<&FlowRunState>, schedule: &FlowSchedule) -> bool {
    match schedule {
        FlowSchedule::Manual => false,
        FlowSchedule::EveryMinutes(minutes) => elapsed_due(state, Duration::from_secs(minutes * 60)),
        FlowSchedule::EveryHours(hours) => elapsed_due(state, Duration::from_secs(hours * 60 * 60)),
        FlowSchedule::Cron(expr) => {
            let schedule = match cron::Schedule::from_str(expr) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Invalid cron expression '{}': {}", expr, e);
                    return false;
                }
            };
            let now = Local::now();
            let after = state
                .and_then(|s| s.last_started_at)
                .unwrap_or(now - chrono::TimeDelta::seconds(1));
            schedule.after(&after).next().map_or(false, |next| next <= now)
        }
    }
}

fn elapsed_due(state: Option<&FlowRunState>, interval: Duration) -> bool {
    match state.and_then(|s| s.last_started_at) {
        None => true,
        Some(last) => {
            let elapsed = Local::now() - last;
            match chrono::TimeDelta::from_std(interval) {
                Ok(td) => elapsed >= td,
                Err(_) => true,
            }
        }
    }
}

fn print_usage() {
    println!(
        "metalcraft-daemon [OPTIONS]\n\n\
         Flow options:\n  \
           --flows-dir <path>       Flows directory\n  \
           --persona <slug>         Persona for flow tasks (default: coding-agent)\n  \
           --model <name>           LLM model name\n  \
           --poll-seconds <n>       Poll interval (default: 30)\n  \
           --once                   Run once and exit\n  \
           --auto-approve           Skip tool approval prompts\n\n\
         Event listener options (requires AGENT_GATEWAY_URL):\n  \
           --event-port <n>         Webhook listener port (default: 3001)\n  \
           --event-host <host>      Host for gateway callback URL (default: localhost)\n  \
           --event-persona <slug>   Persona for event tasks (default: same as --persona)\n  \
           --events <list>          Comma-separated event types (default: message_create)\n  \
           --platforms <list>       Comma-separated platforms (default: all)\n  \
           --admin-user-ids <list>  Comma-separated platform user IDs allowed to trigger the agent (required)\n\n\
         Workshop API options:\n  \
           --api <KEY>              Enable workshop admin API with Bearer KEY (env: WORKSHOP_API_KEY)\n  \
           --api-port <n>           Workshop API port (default: 3002, env: WORKSHOP_API_PORT or PORT)\n\n\
         Required env vars for event listener:\n  \
           AGENT_GATEWAY_URL        Gateway base URL\n  \
           AGENT_GATEWAY_API_KEY    Gateway auth token\n  \
           EVENTD_WEBHOOK_SECRET    Secret for authenticating inbound webhooks\n  \
           EVENTD_ADMIN_USER_IDS    Comma-separated admin user IDs (alternative to --admin-user-ids)"
    );
}
