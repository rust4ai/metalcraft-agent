use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::flows::{self, FlowSchedule};
use metalcraft_agent::paths;
use metalcraft_agent::runtime::{self, AgentRuntimeContext, RunOneShotRequest, DEFAULT_MODEL};

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use chrono::Local;

struct FlowRunState {
    last_started_at: Option<chrono::DateTime<Local>>,
    is_running: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    metalcraft_agent::seed::ensure_defaults();

    let mut flows_dir = paths::flows_dir();
    let mut persona_slug = "coding-agent".to_string();
    let mut model_name = DEFAULT_MODEL.to_string();
    let mut poll_seconds: u64 = 30;
    let mut once = false;
    let mut auto_approve = false;

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
        "Starting metalcraft-flowd with flows_dir={}, persona={}, model={}, poll_seconds={}, once={}",
        flows_dir.display(),
        persona_slug,
        model_name,
        poll_seconds,
        once
    );

    let mut state_by_flow: HashMap<String, FlowRunState> = HashMap::new();

    loop {
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

            match flows::collect_reachable_prompt_texts(&flow.saved) {
                Ok(prompts) => {
                    if prompts.is_empty() {
                        log::warn!("Flow '{}' has no reachable prompt nodes", flow.saved.id);
                    }
                    for (index, prompt) in prompts.iter().enumerate() {
                        log::info!(
                            "Flow '{}' prompt {}/{}",
                            flow.saved.id,
                            index + 1,
                            prompts.len()
                        );

                        let outcome = runtime::run_one_shot_task(
                            &context,
                            RunOneShotRequest {
                                persona_slug: &persona_slug,
                                cwd: &cwd,
                                model_name: &model_name,
                                task: prompt,
                                approval_mode: approval_mode.clone(),
                                diagnostics: None,
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

        if once {
            break;
        }

        tokio::time::sleep(Duration::from_secs(poll_seconds)).await;
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
    println!("metalcraft-flowd [--flows-dir <path>] [--persona <slug>] [--model <name>] [--poll-seconds <n>] [--once] [--auto-approve]");
}
