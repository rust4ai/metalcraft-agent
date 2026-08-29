//! Daemon entrypoint shared by the CLI binary (`bin/metalcraft-daemon.rs`) and
//! the env-driven library entrypoint [`run_daemon`] used by the
//! `starkbot-metal` umbrella crate.
//!
//! [`DaemonConfig`] holds everything the daemon needs. The CLI builds it from
//! [`DaemonConfig::from_env`] and then overrides fields from flags; the umbrella
//! uses `from_env` directly. [`run`] performs the actual work (spawn the
//! workshop API + event listener, then run the flow polling loop).

use crate::approval::ApprovalMode;
use crate::diagnostics::{DiagnosticsLogger, SessionInfo};
use crate::flows;
use crate::paths;
use crate::persona::Persona;
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest};
use crate::workshop_api;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

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

        let workshop_api_key = std::env::var("WORKSHOP_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
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

    // One-time migration: mirror any legacy gateway channel instance into the
    // channel model so the pull loop / status / inbound routing read from it.
    crate::metalcraft_gateway::migrate_instance_to_channel();

    // Self-heal the Metalcraft Gateway connection (rotated secret / reassigned
    // number) — no-op while nothing is connected.
    tokio::spawn(async move { crate::metalcraft_gateway::heal_loop().await });

    // Drain the memory capture queue nightly: distil conversations into durable
    // memories, merge duplicates, and let unused ones decay. Its own loop rather
    // than a flow, because the mechanical half has to run whether or not anyone
    // has armed anything — see `crate::memory::dream::dream_loop`.
    tokio::spawn(async move { crate::memory::dream::dream_loop().await });

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

    // Flow polling loop. Each schedule carries its own bookmark — the occurrence
    // it last reached — so schedules fire independently, including two schedules
    // of the same flow. Runs execute inline and sequentially within an iteration,
    // so no concurrency guard is needed.
    //
    // The bookmarks are read from disk and written back on every change. Held
    // only in memory, as they were, a restart erased them — and an interval
    // schedule with no record of a previous run fires immediately, so "every 24
    // hours" fired again on every pod roll. See `crate::schedule_timing`.
    let mut bookmarks = crate::schedule_timing::bookmarks::load();

    loop {
        // Run one polling iteration, but let Ctrl+C cancel it. The workshop API
        // and event listener install tokio's SIGINT handler via
        // `with_graceful_shutdown`, which replaces the OS default of killing the
        // process. Without selecting on ctrl_c here too, the main loop would keep
        // polling forever and Ctrl+C would only stop the spawned servers.
        let iteration = async {
            for candidate in flows::load_due_candidates() {
                let sid = candidate.scheduled.id.clone();
                // A schedule that names no zone is read in the pod's, and only
                // then in the host clock. Before this the host clock was the
                // only fallback, and in the cluster that is UTC — so an 08:00
                // brief armed by anything that did not think to state a zone
                // (the agent's own scheduling tool, a pack suggestion, a
                // hand-written document) arrived in the middle of the night.
                let zone = candidate
                    .scheduled
                    .schedule
                    .timezone
                    .clone()
                    .or_else(crate::pod_settings::default_timezone);
                let Some(decision) = crate::schedule_timing::decide(
                    bookmarks.get(&sid).copied(),
                    &candidate.trigger,
                    zone.as_deref(),
                    Utc::now(),
                ) else {
                    continue;
                };
                // Bookmark first, and persist before running: a flow that
                // crashes the pod mid-run must not come back owed the same
                // firing forever.
                bookmarks.insert(sid.clone(), decision.occurrence);
                crate::schedule_timing::bookmarks::save(&bookmarks);
                if !decision.run {
                    continue;
                }

                let flow = candidate.flow;
                let scheduled = candidate.scheduled;
                // Per-schedule persona override, falling back to the daemon default.
                let default_persona = scheduled
                    .schedule
                    .persona
                    .as_deref()
                    .unwrap_or(persona_slug.as_str());

                log::info!(
                    "Running flow '{}' ({}) [schedule '{}': {}]",
                    flow.id,
                    flow.name,
                    sid,
                    scheduled.schedule.display_name()
                );

                if crate::flow_exec::is_v2_flow(&flow) {
                    // The agent this schedule was armed with. Every firing is a
                    // conversation inside one long-lived agent, which is what lets a
                    // recurring flow notice it said the same thing yesterday.
                    //
                    // A schedule written before arming bound an agent — or one whose
                    // agent has since been deleted — falls back to the flow's own
                    // agent rather than firing as nobody, which is what left a
                    // nightly cron doing real work that appeared on no screen.
                    let instance_id = scheduled
                        .instance_id
                        .clone()
                        .filter(|id| crate::agent_instance::load(id).is_ok())
                        .or_else(|| {
                            let label = scheduled
                                .schedule
                                .name
                                .as_deref()
                                .filter(|n| !n.trim().is_empty())
                                .unwrap_or(&flow.name);
                            match crate::agent_instance::for_flow(
                                &flow.id,
                                label,
                                &crate::flow_bindings::preset_for(&flow.id),
                            ) {
                                Ok(i) => Some(i.id),
                                Err(e) => {
                                    log::warn!(
                                        "flow '{}' fires with no agent, so it will leave no \
                                         conversation: {e}",
                                        flow.id
                                    );
                                    None
                                }
                            }
                        });
                    // v2 flows run on the stateful state-machine executor.
                    let inputs = scheduled
                        .schedule
                        .inputs
                        .clone()
                        .unwrap_or_else(|| serde_json::json!({}));
                    match crate::flow_exec::run_flow_v2_as(
                        &context,
                        flow.clone(),
                        &cwd,
                        Some(default_persona),
                        &model_name,
                        &inputs,
                        instance_id,
                    )
                    .await
                    {
                        Ok(summary) => log::info!(
                            "Flow '{}' finished: {} ({} steps)",
                            flow.id,
                            summary.status,
                            summary.steps.len()
                        ),
                        Err(e) => log::error!("Flow '{}' failed: {}", flow.id, e),
                    }
                    continue;
                }

                // Flag any missing packs/personas before running so the daemon log shows
                // why a scheduled flow may misbehave (v2 flows surface this in their run
                // record; the legacy prompt path only has the log).
                for w in crate::flow_install::runtime_warnings(&flow) {
                    log::warn!("Flow '{}': {w}", flow.id);
                }

                match flows::collect_reachable_prompts(&flow) {
                    Ok(prompts) => {
                        if prompts.is_empty() {
                            log::warn!("Flow '{}' has no reachable prompt nodes", flow.id);
                        }
                        for (index, prompt) in prompts.iter().enumerate() {
                            // Per-prompt/flow persona wins; otherwise fall back to the
                            // schedule's persona (or the daemon default).
                            let effective_persona =
                                prompt.persona.as_deref().unwrap_or(default_persona);
                            log::info!(
                                "Flow '{}' prompt {}/{} (persona: {})",
                                flow.id,
                                index + 1,
                                prompts.len(),
                                effective_persona
                            );

                            let logger = DiagnosticsLogger::new().ok().map(Arc::new);
                            if let Some(ref diagnostics) = logger {
                                if let Ok(persona) =
                                    Persona::load(effective_persona, &context.personas_dir)
                                {
                                    let system_prompt =
                                        persona.build_system_prompt(&context.skills_dir, &cwd);
                                    diagnostics.log_session_info(SessionInfo {
                                        persona_name: &persona.name,
                                        persona_slug: effective_persona,
                                        model_name: &model_name,
                                        cwd: &cwd,
                                        system_prompt: &system_prompt,
                                        tools: &persona.tools,
                                        skills: &persona.skills,
                                        auto_approve: matches!(
                                            approval_mode,
                                            ApprovalMode::AutoApprove
                                        ),
                                        flow_id: Some(&flow.id),
                                        instance_id: scheduled.instance_id.as_deref(),
                                    });
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
                                    instance_id: scheduled.instance_id.clone(),
                                    preset_personas: None,
                                },
                            )
                            .await;

                            match outcome {
                                Ok(metalcraft::RunOutcome::Completed(state)) => {
                                    log::info!(
                                        "Flow '{}' prompt completed: {}",
                                        flow.id,
                                        state.final_answer().unwrap_or("(no answer)")
                                    );
                                }
                                Ok(metalcraft::RunOutcome::Interrupted { reason, .. }) => {
                                    log::warn!("Flow '{}' prompt interrupted: {}", flow.id, reason);
                                    break;
                                }
                                Ok(metalcraft::RunOutcome::Failed { node, error, .. }) => {
                                    log::error!(
                                        "Flow '{}' prompt failed at {node}: {error}",
                                        flow.id
                                    );
                                    break;
                                }
                                Err(err) => {
                                    log::error!("Flow '{}' prompt failed: {}", flow.id, err);
                                    break;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        log::error!("Flow '{}' is not runnable: {}", flow.id, err);
                    }
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
                let handle = if pause.reason == "wait" {
                    "after"
                } else {
                    "timeout"
                };
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

            reap_stale_sessions().await;
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

/// Remove sessions nobody has touched in a month.
///
/// **Agents are never swept.** This used to reap idle *instances*, which put an
/// agent's memory — the thing it exists to have — on a seven-day timer. What ages
/// out is the transcript: a session nobody has opened in 30 days is history, and
/// the agent that wrote it goes on knowing everything it learned there.
///
/// The directory still needs a bound for the same reason the old sweep did:
/// `read_persisted_chats()` walks every file on every listing, and a pod
/// answering texts all year would otherwise grow one entry per conversation
/// forever.
///
/// Hourly rather than every tick: the poll runs every 30s and this walks the
/// whole chats directory, which is exactly the work being economised.
async fn reap_stale_sessions() {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    const EVERY: Duration = Duration::from_secs(3600);

    {
        let mut last = match LAST.lock() {
            Ok(l) => l,
            Err(e) => e.into_inner(),
        };
        match *last {
            Some(t) if t.elapsed() < EVERY => return,
            _ => *last = Some(Instant::now()),
        }
    }

    let report = crate::workshop_api::reap_stale_chats().await;
    if !report.reaped.is_empty() {
        log::info!(
            "reaped {} session(s) idle for over {} days; their agents are kept",
            report.reaped.len(),
            crate::workshop_api::SESSION_TTL_DAYS
        );
    }
    for (id, e) in &report.failed {
        log::warn!("could not reap session '{id}': {e}");
    }
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
                        // A follow-up is not a flow run; it has no armed agent.
                        instance_id: None,
                        preset_personas: None,
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
                        log::warn!(
                            "Scheduled follow-up {} did not complete: {other:?}",
                            task.id
                        );
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

fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}
