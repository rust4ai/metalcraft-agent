//! Daemon entrypoint shared by the CLI binary (`bin/metalcraft-daemon.rs`) and
//! the env-driven library entrypoint [`run_daemon`] used by the
//! `starkbot-metal` umbrella crate.
//!
//! [`DaemonConfig`] holds everything the daemon needs. The CLI builds it from
//! [`DaemonConfig::from_env`] and then overrides fields from flags; the umbrella
//! uses `from_env` directly. [`run`] performs the actual work (spawn the
//! workshop API + event listener, then run the flow polling loop).

use crate::approval::ApprovalMode;
use crate::diagnostics::DiagnosticsLogger;
use crate::flows::{self, FlowSchedule};
use crate::paths;
use crate::persona::Persona;
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest};
use crate::workshop_api;

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;

type DynError = Box<dyn std::error::Error>;

/// Fully-resolved daemon configuration.
pub struct DaemonConfig {
    pub flows_dir: PathBuf,
    pub persona_slug: String,
    pub model_name: String,
    pub poll_seconds: u64,
    pub once: bool,
    pub auto_approve: bool,

    // Workshop admin API
    pub workshop_api_key: Option<String>,
    pub workshop_api_port: u16,
    /// Serve the workshop API even without a static key, authenticating callers
    /// via Metalcraft ID (OIDC) tokens only. Set on managed pods that mint no
    /// static key. When `workshop_api_key` is also present, both credential
    /// paths work; the flag only matters in the no-key case.
    pub workshop_api_oidc: bool,
}

impl DaemonConfig {
    /// Build a config from environment variables, applying the same defaults the
    /// CLI uses. The CLI starts from this and overrides fields from flags; the
    /// umbrella uses it as-is.
    ///
    /// Flow/agent settings that were previously CLI-only are read from
    /// `STARKBOT_*` vars so a containerised daemon needs no arguments.
    pub fn from_env() -> Self {
        let flows_dir = std::env::var("STARKBOT_FLOWS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| paths::flows_dir());

        let persona_slug = runtime::configured_default_persona();

        let model_name = runtime::configured_default_model();

        let poll_seconds = std::env::var("STARKBOT_POLL_SECONDS")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(30);

        let once = env_flag("STARKBOT_ONCE", false);
        let auto_approve = env_flag("STARKBOT_AUTO_APPROVE", false);

        let workshop_api_key = std::env::var("WORKSHOP_API_KEY").ok().filter(|s| !s.is_empty());
        let workshop_api_oidc = env_flag("WORKSHOP_API_ENABLED", false);
        let workshop_api_port = std::env::var("WORKSHOP_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .or_else(|| std::env::var("PORT").ok().and_then(|p| p.parse().ok()))
            .unwrap_or(3002);

        Self {
            flows_dir,
            persona_slug,
            model_name,
            poll_seconds,
            once,
            auto_approve,
            workshop_api_key,
            workshop_api_port,
            workshop_api_oidc,
        }
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        if self.auto_approve {
            ApprovalMode::AutoApprove
        } else {
            ApprovalMode::default_interactive()
        }
    }
}

/// Env-driven entrypoint for the umbrella crate: loads `.env`, initialises
/// logging + seeds, builds the config from the environment, and runs.
pub async fn run_daemon() -> Result<(), DynError> {
    dotenvy::dotenv().ok();
    // `try_init` (not `init`) so this is safe even if a logger is already set.
    let _ = env_logger::try_init();
    crate::seed::ensure_defaults();
    run(DaemonConfig::from_env()).await
}

/// One-shot: on a managed pod that sets `ENABLE_METALCRAFT_PACKS`, enable every
/// pack tagged [`crate::integration_packs::ECOSYSTEM_TAG`] the first time the
/// daemon boots, then drop a marker so it never runs again.
///
/// This is deliberately *not* a reconciler. After the single seed the user owns
/// pack state — a pack they later disable stays disabled across reboots, even
/// though the env var is still set. The marker is the whole one-shot mechanism;
/// the env var only says "this pod opted in". Runs after
/// [`crate::seed::ensure_defaults`] so the ecosystem packs are already on disk
/// (and `set_enabled` installs from the embedded seed anyway).
///
/// No-op when the var is unset/falsey or the marker already exists — the common
/// path on every boot after the first is a single `exists()` check.
fn maybe_autoenable_ecosystem_packs() {
    if !env_flag("ENABLE_METALCRAFT_PACKS", false) {
        return;
    }
    let marker = paths::ecosystem_packs_seeded_marker();
    if marker.exists() {
        return;
    }

    let ids = crate::integration_packs::ecosystem_pack_ids();
    let mut enabled = Vec::new();
    for id in &ids {
        match crate::integration_packs::set_enabled(id, true) {
            Ok(()) => enabled.push(id.clone()),
            Err(e) => log::warn!("auto-enable: could not enable ecosystem pack '{id}': {e}"),
        }
    }

    // Write the marker whatever happened above. This is a convenience seed, not
    // a reconciler: we must not retry-enable on the next boot, or we'd re-enable
    // a pack the user deliberately turned off. Record the timestamp + what we
    // enabled for operator forensics.
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = format!("{}\n{}\n", chrono::Utc::now().to_rfc3339(), enabled.join("\n"));
    match std::fs::write(&marker, body) {
        Ok(()) => log::info!(
            "auto-enabled {} Metalcraft ecosystem pack(s) on first boot: [{}]",
            enabled.len(),
            enabled.join(", ")
        ),
        // Marker write failed → next boot retries. set_enabled is idempotent, so
        // a retry is harmless; log loudly so a persistently-unwritable data dir
        // is visible rather than silently re-seeding every boot.
        Err(e) => log::warn!(
            "auto-enable: enabled [{}] but could not write seed marker {} ({e}); \
             will retry on next boot",
            enabled.join(", "),
            marker.display()
        ),
    }
}

/// Run the daemon: spawn the workshop API (if keyed), then run the flow polling
/// loop until Ctrl+C (or once, if `config.once`). The workshop API also hosts
/// the gateway channels (inbound webhooks + management). Assumes
/// `.env`/logging/seeds are already set up.
pub async fn run(config: DaemonConfig) -> Result<(), DynError> {
    let approval_mode = config.approval_mode();
    let DaemonConfig {
        flows_dir,
        persona_slug,
        model_name,
        poll_seconds,
        once,
        auto_approve: _,
        workshop_api_key,
        workshop_api_port,
        workshop_api_oidc,
    } = config;

    // One-time migration of legacy global PIPESTREAMR_* keys into channel scope
    // (and a v2 keys.json schema upgrade). Idempotent — a no-op once migrated.
    crate::metalcraft_gateway::migrate_legacy_keys();

    // One-shot: auto-enable the Metalcraft ecosystem packs on a managed pod's
    // first boot (gated by ENABLE_METALCRAFT_PACKS). Placed here in `run` — not
    // in `run_daemon` — so it fires for BOTH entrypoints (the `metalcraft-daemon`
    // CLI bin and the umbrella crate), which both funnel through here after
    // `seed::ensure_defaults`. Idempotent via its marker file.
    maybe_autoenable_ecosystem_packs();

    let context = AgentRuntimeContext::from_environment()?;
    let cwd = std::env::current_dir()?.display().to_string();

    log::info!(
        "Starting metalcraft-daemon with flows_dir={}, persona={}, model={}, poll_seconds={}, once={}",
        flows_dir.display(),
        persona_slug,
        model_name,
        poll_seconds,
        once
    );

    // Spawn the workshop admin API when either a static key was supplied
    // (legacy / self-hosted) or OIDC-only mode is enabled (managed pods that
    // mint no static key). Runs alongside the flow scheduler so a single daemon
    // process can both run flows and serve project edits from the workshop.
    // An empty key disables the static-bearer path in `auth_middleware`, leaving
    // Metalcraft ID (`mck_`) tokens as the only accepted credential.
    if workshop_api_key.is_some() || workshop_api_oidc {
        let port = workshop_api_port;
        let key = workshop_api_key.clone().unwrap_or_default();
        let router = workshop_api::build_router(key);
        tokio::spawn(async move {
            workshop_api::serve(port, router).await;
        });
        log::info!("Workshop API spawned on port {port}");
    }

    // Self-heal the Metalcraft Gateway connection (rotated secret / reassigned
    // number) — no-op while nothing is connected.
    tokio::spawn(async move { crate::metalcraft_gateway::heal_loop().await });

    // Always-visible startup banner, printed regardless of RUST_LOG.
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
    match (&workshop_api_key, workshop_api_oidc) {
        (Some(_), true) => {
            println!("  workshop API:   enabled on port {workshop_api_port} (static key + OIDC)")
        }
        (Some(_), false) => {
            println!("  workshop API:   enabled on port {workshop_api_port} (static key)")
        }
        (None, true) => {
            println!("  workshop API:   enabled on port {workshop_api_port} (OIDC only)")
        }
        (None, false) => {
            println!("  workshop API:   disabled (set WORKSHOP_API_KEY or WORKSHOP_API_ENABLED)")
        }
    }
    println!("──────────────────────────────────────────────");

    // Flow polling loop.
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

                if crate::flow_exec::is_v2_flow(&flow.saved) {
                    // v2 flows run on the stateful state-machine executor.
                    match crate::flow_exec::run_flow_v2(
                        &context,
                        flow.saved.clone(),
                        &cwd,
                        Some(persona_slug.as_str()),
                        &model_name,
                        &serde_json::json!({}),
                    )
                    .await
                    {
                        Ok(summary) => log::info!(
                            "Flow '{}' finished: {} ({} steps)",
                            flow.saved.id,
                            summary.status,
                            summary.steps.len()
                        ),
                        Err(e) => log::error!("Flow '{}' failed: {}", flow.saved.id, e),
                    }
                    if let Some(state) = state_by_flow.get_mut(&flow_id) {
                        state.is_running = false;
                    }
                    continue;
                }

                // Flag any missing packs/personas before running so the daemon log shows
                // why a scheduled flow may misbehave (v2 flows surface this in their run
                // record; the legacy prompt path only has the log).
                for w in crate::flow_install::runtime_warnings(&flow.saved) {
                    log::warn!("Flow '{}': {w}", flow.saved.id);
                }

                match flows::collect_reachable_prompts(&flow.saved) {
                    Ok(prompts) => {
                        if prompts.is_empty() {
                            log::warn!("Flow '{}' has no reachable prompt nodes", flow.saved.id);
                        }
                        for (index, prompt) in prompts.iter().enumerate() {
                            // Per-prompt/flow persona wins; otherwise fall back to the daemon persona.
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
                                Ok(metalcraft::RunOutcome::Failed { node, error, .. }) => {
                                    log::error!("Flow '{}' prompt failed at {node}: {error}", flow.saved.id);
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

            // Auto-resume any paused run whose wake time has arrived: `wait`
            // nodes (via the `after` handle) and `approval` nodes that timed out
            // (via the `timeout` handle).
            for mut run in crate::flow_runs::list_runs(&crate::paths::runs_dir()) {
                if run.status != "paused" {
                    continue;
                }
                // If the flow behind this run is gone and the record carries no flow
                // snapshot to resume from (a pre-snapshot/legacy run), resume can never
                // succeed — mark it failed once instead of re-attempting (and error-
                // logging) it every poll iteration forever.
                if run.flow.is_none()
                    && metalcraft_flows::load_flow(&crate::paths::flows_dir(), &run.flow_id)
                        .is_none()
                {
                    log::warn!(
                        "Failing paused run '{}': flow '{}' no longer exists and the run has no snapshot to resume from",
                        run.id,
                        run.flow_id
                    );
                    run.status = "failed".into();
                    run.pause = None;
                    let _ = crate::flow_runs::save_run(&crate::paths::runs_dir(), &run);
                    continue;
                }
                let Some(pause) = &run.pause else { continue };
                let due = pause
                    .wake_at
                    .as_deref()
                    .and_then(|w| chrono::DateTime::parse_from_rfc3339(w).ok())
                    .map(|t| t.with_timezone(&chrono::Utc) <= chrono::Utc::now())
                    .unwrap_or(false);
                if !due {
                    continue;
                }
                let handle = if pause.reason == "wait" { "after" } else { "timeout" };
                log::info!(
                    "Auto-resuming flow run '{}' (flow '{}', {} → {handle})",
                    run.id,
                    run.flow_id,
                    pause.reason
                );
                match crate::flow_exec::resume_flow(&context, &run.id, handle, None).await {
                    Ok(summary) => log::info!("Run '{}' resumed: {}", run.id, summary.status),
                    Err(e) => log::error!("Failed to resume run '{}': {}", run.id, e),
                }
            }

            // Fire any scheduled follow-ups whose time has come (see
            // `crate::scheduled_tasks`). Runs in the same tick as flow polling.
            run_due_scheduled_tasks(&context, &cwd, &persona_slug, &model_name, &approval_mode)
                .await;
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

/// Run every scheduled follow-up that is now due: claim it (so a later tick
/// can't double-fire it), run its stored task as a one-shot agent under the
/// requested persona, deliver the result, and record the terminal status.
async fn run_due_scheduled_tasks(
    context: &AgentRuntimeContext,
    cwd: &str,
    default_persona: &str,
    model_name: &str,
    approval_mode: &ApprovalMode,
) {
    use crate::scheduled_tasks::{self, TaskStatus};

    use crate::scheduled_tasks::IoBinding;
    use crate::workshop_api::FollowupDelivery;

    let due = scheduled_tasks::claim_due(chrono::Utc::now());
    for task in due {
        log::info!("Running scheduled follow-up {}: {}", task.id, task.task);

        match &task.io_binding {
            // Workshop chat: run the follow-up as a real turn on that chat so
            // the reply is persisted and streamed to any open subscriber.
            IoBinding::WorkshopChat { chat_id } => {
                match workshop_api::deliver_followup_to_chat(context, chat_id, &task.task).await {
                    FollowupDelivery::Delivered => {
                        scheduled_tasks::mark(&task.id, TaskStatus::Done);
                    }
                    FollowupDelivery::ChatBusy => {
                        // Retry shortly rather than dropping it.
                        log::info!("Chat {chat_id} busy; requeuing follow-up {}", task.id);
                        scheduled_tasks::requeue(
                            &task.id,
                            chrono::Utc::now() + chrono::Duration::seconds(30),
                        );
                    }
                    FollowupDelivery::ChatMissing => {
                        log::warn!("Chat {chat_id} gone; dropping follow-up {}", task.id);
                        scheduled_tasks::mark(&task.id, TaskStatus::Failed);
                    }
                }
            }
            // Gateway / unbound: run as a one-shot under the requested persona.
            // (Gateway adapter delivery is wired in a later pass; for now the
            // result is logged so a completed follow-up isn't lost.)
            _ => {
                let persona = task.persona.as_deref().unwrap_or(default_persona);
                let logger = DiagnosticsLogger::new().ok().map(Arc::new);
                let outcome = runtime::run_one_shot_task(
                    context,
                    RunOneShotRequest {
                        persona_slug: persona,
                        cwd,
                        model_name,
                        task: &task.task,
                        approval_mode: approval_mode.clone(),
                        diagnostics: logger,
                    },
                )
                .await;
                match outcome {
                    Ok(metalcraft::RunOutcome::Completed(state)) => {
                        let answer = state.final_answer().unwrap_or("(no answer)");
                        log::info!(
                            "Scheduled follow-up {} result [{:?}]: {answer}",
                            task.id,
                            task.io_binding
                        );
                        scheduled_tasks::mark(&task.id, TaskStatus::Done);
                    }
                    Ok(other) => {
                        log::warn!("Scheduled follow-up {} did not complete: {other:?}", task.id);
                        scheduled_tasks::mark(&task.id, TaskStatus::Failed);
                    }
                    Err(e) => {
                        log::error!("Scheduled follow-up {} errored: {e}", task.id);
                        scheduled_tasks::mark(&task.id, TaskStatus::Failed);
                    }
                }
            }
        }
    }
}

struct FlowRunState {
    last_started_at: Option<chrono::DateTime<Local>>,
    is_running: bool,
}

fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
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
