//! The heartbeat: what happens when a goal wakes up.
//!
//! One tick is **one bounded agent turn in a fresh conversation**, carrying
//! nothing but the goal's scratchpad. Fresh-per-tick is the load-bearing choice:
//! continuing one ever-growing conversation makes every tick cost more than the
//! last until compaction starts destroying exactly the detail the goal depends
//! on. Statelessness is what makes the scratchpad matter, and it is the
//! difference between a goal you can leave running for a week and one you
//! cannot.
//!
//! Everything a tick leaves behind is in three places: the scratchpad (what the
//! next tick reads), the goal record (status and counters), and the journal
//! (what a person reads).
//!
//! See `docs/goal-agent-plan.md`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::diagnostics::DiagnosticsLogger;
use crate::goals::{self, Goal, GoalStatus};
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest};
use crate::approval::ApprovalMode;

/// What this tick is for. The kind picks the frame the agent is given and the
/// model tier that runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TickKind {
    /// No usable plan yet — this tick's work is to write one.
    Plan,
    /// Take the next step.
    Work,
    /// Audit the plan against reality and groom the scratchpad.
    Review,
}

impl TickKind {
    /// The model tier this kind runs at.
    ///
    /// Strong where a mistake is expensive and hard to see: the plan shapes
    /// everything downstream, and a review rewrites the only state the goal has
    /// — a weak model there loses a goal rather than merely slowing it.
    pub fn tier(&self) -> &'static str {
        match self {
            Self::Plan | Self::Review => "strong",
            Self::Work => "standard",
        }
    }
}

/// How often a review tick displaces a work tick.
pub const REVIEW_EVERY: u32 = 5;

/// Decide what this tick is.
///
/// A scratchpad that has decayed past its bounds forces a review regardless of
/// the cadence — a goal that thrashes grooms more often, which is the correct
/// response to thrashing rather than a punishment for it.
pub fn tick_kind(goal: &Goal, scratchpad: &str) -> TickKind {
    if goals::progress_of(scratchpad).total == 0 {
        return TickKind::Plan;
    }
    if goals::needs_groom(scratchpad) {
        return TickKind::Review;
    }
    // `ticks` is what has already happened, so the tick about to run is n+1.
    let next = goal.counters.ticks + 1;
    if next.is_multiple_of(REVIEW_EVERY) {
        TickKind::Review
    } else {
        TickKind::Work
    }
}

/// Map a tier name onto a model this pod actually has.
///
/// "standard" resolves to whatever the pod is configured to use — on a managed
/// pod that is the sentinel the inference gateway turns into the user's own
/// chosen model, so pinning a name here would quietly override a choice made in
/// a client. The other two tiers step off the ends of [`AVAILABLE_MODELS`].
///
/// This is deliberately crude. The real fix is a pod-level tier map, at which
/// point this function is where it lands.
pub fn resolve_tier(tier: &str) -> String {
    let ladder = runtime::AVAILABLE_MODELS;
    match tier {
        "mini" => ladder.first().unwrap_or(&runtime::DEFAULT_MODEL).to_string(),
        "strong" => ladder.last().unwrap_or(&runtime::DEFAULT_MODEL).to_string(),
        _ => runtime::configured_default_model(),
    }
}

/// One step up the ladder. `strong` is the top; asking past it returns it.
fn escalate(tier: &str) -> &str {
    match tier {
        "mini" => "standard",
        _ => "strong",
    }
}

/// The tier for one tick: the kind's, the goal's override if it has one, then
/// one step up for every tick that has changed nothing.
///
/// Static assignment is brittle in one direction: a cheap tick that thrashes
/// costs more than the strong tick it was avoiding, and it does it repeatedly.
/// Escalating on `no_progress_streak` puts the decision where there is evidence
/// for it — an observed failure to move — rather than on a model's opinion of
/// how hard its own task is. A groom resets the streak, so this backs off again
/// on its own.
pub fn tier_for(goal: &Goal, kind: TickKind) -> String {
    let base = match kind {
        TickKind::Plan => goal.models.plan.as_deref(),
        TickKind::Work => goal.models.work.as_deref(),
        TickKind::Review => goal.models.review.as_deref(),
    }
    .unwrap_or_else(|| kind.tier());

    if goal.counters.no_progress_streak == 0 {
        return base.to_string();
    }
    escalate(base).to_string()
}

/// The model for one tick.
fn model_for(goal: &Goal, kind: TickKind) -> String {
    resolve_tier(&tier_for(goal, kind))
}

// ── the journal ──────────────────────────────────────────────────────────────

/// One line of what the goal did, for a person to read.
///
/// A structured record rather than chat messages: a tick summary is not a
/// conversation turn, and the clients that render a goal draw a progress bar and
/// a timeline from these fields. Questions for a person still go out through the
/// goal's [`IoBinding`](crate::scheduled_tasks::IoBinding) — this is the log, not
/// the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct JournalEntry {
    pub at: String,
    pub tick: u32,
    pub kind: TickKind,
    pub model: String,
    /// What the agent said it did — its final answer, trimmed.
    pub summary: String,
    /// Status after the tick, so a reader can see where it turned.
    pub status: GoalStatus,
    pub plan_done: u32,
    pub plan_total: u32,
    /// False when the tick left the scratchpad byte-identical: it thought,
    /// possibly spent, and recorded nothing.
    pub progressed: bool,
    pub duration_secs: u64,
}

fn journal_path(goal_id: &str) -> std::path::PathBuf {
    crate::paths::goal_dir(goal_id).join("journal.jsonl")
}

/// Append one entry. Append-only, one JSON per line: the journal is written far
/// more often than it is read, and a tick must never be lost because the file
/// grew large.
pub fn append_journal(goal_id: &str, entry: &JournalEntry) {
    use std::io::Write;
    let path = journal_path(goal_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{line}") {
                log::warn!("could not write goal journal {goal_id}: {e}");
            }
        }
        Err(e) => log::warn!("could not open goal journal {goal_id}: {e}"),
    }
}

/// The journal, newest last. `limit` takes the most recent entries.
pub fn read_journal(goal_id: &str, limit: usize) -> Vec<JournalEntry> {
    let Ok(content) = std::fs::read_to_string(journal_path(goal_id)) else {
        return Vec::new();
    };
    let mut entries: Vec<JournalEntry> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if entries.len() > limit {
        entries.drain(..entries.len() - limit);
    }
    entries
}

// ── due-ness ─────────────────────────────────────────────────────────────────

/// Whether this goal is owed a tick now.
///
/// The bookmark is `counters.last_tick_at` on the goal record itself, rather
/// than the separate file scheduled flows keep. A goal is a single recurring
/// thing with one interval, so its own record is the natural place — and it
/// survives a restart for free, which is the bug that file exists to fix.
///
/// A goal that has never ticked is due immediately: it was created by someone
/// who has just asked for it, and waiting half an hour to start would read as
/// broken.
pub fn is_due(goal: &Goal, now: chrono::DateTime<chrono::Utc>) -> bool {
    if !goal.status.ticks() {
        return false;
    }
    let Some(last) = goal.counters.last_tick_at.as_deref() else {
        return true;
    };
    let Ok(last) = chrono::DateTime::parse_from_rfc3339(last) else {
        // An unparseable bookmark must not wedge a goal forever.
        return true;
    };
    let interval = chrono::Duration::minutes(goal.tick_interval_minutes() as i64);
    now >= last.with_timezone(&chrono::Utc) + interval
}

/// Every goal owed a tick, oldest bookmark first so a backlog drains fairly.
pub fn due(now: chrono::DateTime<chrono::Utc>) -> Vec<Goal> {
    let mut due: Vec<Goal> = goals::list().into_iter().filter(|g| is_due(g, now)).collect();
    due.sort_by(|a, b| a.counters.last_tick_at.cmp(&b.counters.last_tick_at));
    due
}

// ── rails ────────────────────────────────────────────────────────────────────

/// Why a goal was stopped by a rail, if it was.
///
/// Every one of these blocks rather than ends the goal. Running out of rope is a
/// reason to ask the person who set it whether to extend, not to disappear —
/// and a goal that quietly gave up looks exactly like one still working.
pub fn rail_tripped(goal: &Goal, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let r = &goal.rails;
    if goal.counters.ticks >= r.max_ticks {
        return Some(format!(
            "Out of ticks: {} of {} used. Raise max_ticks to carry on.",
            goal.counters.ticks, r.max_ticks
        ));
    }
    if goal.counters.no_progress_streak >= r.max_consecutive_no_progress {
        return Some(format!(
            "{} ticks in a row changed nothing. Something is stuck that I cannot see; \
             read the scratchpad before letting this run on.",
            goal.counters.no_progress_streak
        ));
    }
    if let Some(budget) = r.compute_minutes_budget
        && goal.counters.compute_minutes_used >= budget
    {
        return Some(format!(
            "Compute budget spent: {} of {budget} workspace minutes.",
            goal.counters.compute_minutes_used
        ));
    }
    if let Some(deadline) = r.deadline.as_deref()
        && let Ok(d) = chrono::DateTime::parse_from_rfc3339(deadline)
        && now >= d.with_timezone(&chrono::Utc)
    {
        return Some(format!("Past its deadline ({deadline})."));
    }
    None
}


// ── the pre-flight ───────────────────────────────────────────────────────────

/// How long a pending run may hold a goal in the waiting state.
///
/// buildr's own reaper settles a run whose follower died after 15 minutes, and
/// `build`/`test` are capped at 10, so anything still running past this is not
/// coming back. Waiting forever on it would be a goal that costs nothing and
/// does nothing, which is the quietest way for this design to fail.
const PENDING_RUN_PATIENCE_MINS: i64 = 30;

/// What the pre-flight decided, before any model was involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreFlight {
    /// Run a tick. `note` is something the pre-flight learned that belongs in
    /// the scratchpad first — a finished build, a workspace that vanished.
    Spend { note: Option<String> },
    /// Spend nothing. The thing this goal is waiting for has not happened yet,
    /// and asking a language model to observe that costs more than the answer.
    Wait { why: String },
}

/// The decision about a pending run, split out from the HTTP so it can be
/// tested without a network.
///
/// `age_mins` is how long ago the run was started, which is the only thing that
/// distinguishes "still going" from "never coming back".
pub fn decide_pending(
    result: &Result<crate::buildr::Run, crate::buildr::Error>,
    age_mins: i64,
    what: &str,
) -> PreFlight {
    match result {
        Ok(run) if !run.finished() => {
            if age_mins >= PENDING_RUN_PATIENCE_MINS {
                PreFlight::Spend {
                    note: Some(format!(
                        "`{what}` (run {}) has been running for {age_mins} minutes and is not \
                         coming back. Treat it as failed, and check the workspace \
                         before starting another.",
                        run.id
                    )),
                }
            } else {
                PreFlight::Wait {
                    why: format!("`{what}` is still running"),
                }
            }
        }
        Ok(run) => {
            // The output is what the next tick actually needs — a build that
            // failed is only useful with the reason attached.
            let tail: String = run
                .output
                .as_deref()
                .unwrap_or("")
                .chars()
                .rev()
                .take(2000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            PreFlight::Spend {
                note: Some(format!(
                    "`{what}` finished: **{}**{}.{}",
                    run.status,
                    run.exit_code
                        .map(|c| format!(" (exit {c})"))
                        .unwrap_or_default(),
                    if tail.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n\n```\n{}\n```", tail.trim())
                    }
                )),
            }
        }
        // Gone means gone: the run record was reaped or never existed, and
        // waiting for it is waiting for nothing.
        Err(crate::buildr::Error::Gone) => PreFlight::Spend {
            note: Some(format!(
                "The run for `{what}` no longer exists. Start it again if it still matters."
            )),
        },
        // No credential: nothing here will ever resolve, so stop waiting and let
        // the tick say so properly.
        Err(crate::buildr::Error::NotConfigured) => PreFlight::Spend {
            note: Some(format!(
                "Could not check `{what}`: this pod has no buildr.space credential."
            )),
        },
        // A transient — buildr down, a network blip. Cheap to try again shortly,
        // but not forever: past the patience window the tick runs and reports.
        Err(e) => {
            if age_mins >= PENDING_RUN_PATIENCE_MINS {
                PreFlight::Spend {
                    note: Some(format!("Could not check `{what}` for {age_mins} minutes: {e}")),
                }
            } else {
                PreFlight::Wait {
                    why: format!("could not reach buildr.space ({e})"),
                }
            }
        }
    }
}

/// Answer everything that can be answered without a model, and decide whether
/// this wake-up is worth a turn.
///
/// Mutates `goal` in place — clearing a pending run that has landed, forgetting
/// a workspace that has been reaped, recording compute spent — so the tick that
/// follows starts from what is true rather than from what was true last time.
async fn preflight(goal: &mut Goal) -> PreFlight {
    let mut notes: Vec<String> = Vec::new();

    if let Some(pending) = goal.pending_run.clone() {
        let age = chrono::DateTime::parse_from_rfc3339(&pending.started_at)
            .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_minutes())
            .unwrap_or(0);
        let result = crate::buildr::get_run(&pending.workspace_id, &pending.run_id).await;
        match decide_pending(&result, age, &pending.what) {
            PreFlight::Wait { why } => {
                // Deliberately nothing else: no journal line, no counters. A
                // wait is not a tick, and one line every five minutes would bury
                // the ticks that did something.
                log::info!("goal {} waiting: {why}", goal.id);
                return PreFlight::Wait { why };
            }
            PreFlight::Spend { note } => {
                goal.pending_run = None;
                notes.extend(note);
            }
        }
    }

    // A workspace that has been reaped is the normal end of a free-plan week,
    // not an error — but a tick that does not know costs a whole turn finding
    // out, and usually finds out by failing.
    if let Some(ws_id) = goal.workspace.id.clone() {
        match crate::buildr::get_workspace(&ws_id).await {
            Ok(ws) if ws.is_ready() || ws.is_hibernated() => {}
            Ok(ws) => notes.push(format!(
                "Workspace `{ws_id}` is `{}`{}. It is not usable yet.",
                ws.status,
                ws.error.map(|e| format!(" ({e})")).unwrap_or_default()
            )),
            Err(crate::buildr::Error::Gone) => {
                goal.workspace.id = None;
                notes.push(format!(
                    "Workspace `{ws_id}` is gone. Create a new one and clone the repo at the \
                     goal's branch before doing anything else — the branch and this \
                     scratchpad are the only things that survived."
                ));
            }
            Err(e) => log::debug!("goal {}: could not read workspace: {e}", goal.id),
        }
    }

    if crate::buildr::configured()
        && let Ok(compute) = crate::buildr::compute().await
    {
        goal.counters.compute_minutes_used = compute.used();
    }

    PreFlight::Spend {
        note: if notes.is_empty() {
            None
        } else {
            Some(notes.join("\n\n"))
        },
    }
}

/// Tell the person who set the goal that it has stopped and needs them.
///
/// The whole design leans on someone noticing a blocked goal — it stops the
/// heartbeat, so nothing else will ever mention it again. Before this, a goal
/// that blocked at 2am sat there until somebody happened to open the right
/// screen, which is exactly the overnight silence `goal_block` is supposed to be
/// rare enough to justify.
///
/// Delivered into the chat the goal was created from, as a turn in that
/// conversation, so it arrives wherever that chat already reaches — the
/// workshop, a phone, a text message. A goal with no binding logs and moves on:
/// there is nowhere to say it.
async fn announce(context: &AgentRuntimeContext, goal: &Goal, message: &str) {
    use crate::scheduled_tasks::IoBinding;
    match &goal.io {
        IoBinding::WorkshopChat { chat_id } => {
            match crate::workshop_api::deliver_followup_to_chat(context, chat_id, message).await {
                crate::workshop_api::FollowupDelivery::Delivered => {}
                // Not retried: the goal is already stopped and the message is in
                // its journal and its scratchpad either way. Waking a busy chat
                // twice to say the same thing is worse than saying it once.
                other => log::warn!("goal {}: could not announce ({other:?})", goal.id),
            }
        }
        other => log::info!(
            "goal {} has nowhere to announce to ({other:?}): {message}",
            goal.id
        ),
    }
}

/// Put the workspace back to sleep, whatever the tick did or forgot.
///
/// Not an optimisation and not the prompt's job. buildr bills awake minutes and
/// hibernates on its own only after 10–30 idle minutes, so a tick that leaves
/// the box running bills most of the gap to the goal's owner — every half hour,
/// all week. Failures are logged and swallowed: a workspace that refuses to
/// hibernate (mid-provision, or serving) is buildr's problem to reap, and it is
/// not worth failing a tick that otherwise went fine.
async fn hibernate_workspace(goal: &Goal) {
    let Some(ws_id) = goal.workspace.id.as_deref() else {
        return;
    };
    match crate::buildr::hibernate(ws_id).await {
        Ok(_) => log::info!("goal {}: hibernated workspace {ws_id}", goal.id),
        Err(e) => log::debug!("goal {}: could not hibernate {ws_id}: {e}", goal.id),
    }
}

// ── the frame ────────────────────────────────────────────────────────────────

/// The instruction a tick opens with, before its scratchpad.
///
/// It is long because it is the whole contract: every tick is a stranger to
/// every other one, and this is the only place the rules can be stated. The
/// three that matter most — verify before checking, commit before finishing,
/// rewrite the scratchpad last — are the three whose absence produces a goal
/// that looks like it is working and is not.
pub fn tick_frame(goal: &Goal, kind: TickKind, tick_number: u32) -> String {
    let common = format!(
        "You are working towards a long-running goal. This is tick {tick_number}.\n\n\
         You remember nothing of previous ticks. Everything you know is in the scratchpad \
         below — and everything the *next* tick will know is what you leave there.\n\n\
         Rules that hold on every tick:\n\
         - **Do one slice of work, not the whole goal.** A tick is minutes, not hours.\n\
         - **Verify before you claim.** Never check a plan box you did not see pass — a build \
         that compiles is not a feature that works.\n\
         - **Uncommitted work does not exist.** If you changed code, commit and push it before \
         the tick ends; the workspace can be reaped between ticks.\n\
         - **Decide, don't stall.** For an ordinary choice, pick the reasonable option, record \
         the decision and why in the scratchpad's State, and keep moving. Use `goal_block` only \
         when the call is irreversible, spends money, or changes what the goal means.\n\
         - **Finish by rewriting the scratchpad** with `goal_scratchpad_write`, so a stranger \
         could pick this up. That is the last thing you do.\n\n"
    );

    let specific = match kind {
        TickKind::Plan => {
            "**This tick is for planning.** There is no usable plan yet. Look at the goal and at \
             whatever the repo or the workspace tells you, then write a plan of 3–8 concrete \
             steps as markdown checkboxes under `## Plan`. Each step should be one tick's worth \
             of work and concrete enough that you could tell whether it happened. Do not start \
             the work this tick — the plan is the work."
        }
        TickKind::Work => {
            "**This tick is for work.** Take the first unchecked step in the plan. Do it, verify \
             it, and check it off only if the verification passed. If the step turns out to be \
             bigger than one tick, split it in the plan and do the first part."
        }
        TickKind::Review => {
            "**This tick is for review and grooming.** Do no new work. Instead:\n\
             1. Re-derive the plan from reality — what is actually on the branch, what the tests \
             and CI actually say — not from what earlier ticks claimed. Uncheck anything not \
             genuinely done, and add anything that was missed.\n\
             2. Fold the older half of the Log into State: turn twenty lines of what-I-did into \
             three lines of what-is-true-now.\n\
             3. Retire what is resolved — answered questions, cleared blockers.\n\
             4. Keep every decision you took, so a later tick does not re-litigate it.\n\
             5. Remember anything that will outlive this goal (how this repo builds, what its \
             tests need) with `mem_remember`.\n\
             Never drop an unchecked step, an open blocker or an unresolved question."
        }
    };

    let scratchpad = goals::read_scratchpad(&goal.id)
        .map(|s| goals::trim_for_injection(&s))
        .unwrap_or_else(|| goals::seed_scratchpad(goal));

    let pending = goal
        .pending_run
        .as_ref()
        .map(|r| {
            format!(
                "\n\n**First, before anything else:** the previous tick started `{}` (run `{}` in \
                 workspace `{}`) and did not wait for it. Read its result and act on it.",
                r.what, r.run_id, r.workspace_id
            )
        })
        .unwrap_or_default();

    format!("{common}{specific}{pending}\n\n---\n\n{scratchpad}")
}

// ── running one ──────────────────────────────────────────────────────────────

/// What a tick did, for the caller that has to log it.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    pub kind: TickKind,
    pub progressed: bool,
    pub status: GoalStatus,
    pub summary: String,
    /// True when the wake-up spent nothing: the pre-flight found the goal still
    /// waiting on something, and no model ran.
    pub waited: bool,
}

/// Run one tick of one goal, and record everything it left behind.
///
/// Never returns an error: a tick that failed is a fact about the goal, not
/// about the daemon, and the daemon has other goals to run. Failures land in the
/// journal and count towards the no-progress rail like any other tick that
/// achieved nothing.
pub async fn run_tick(
    context: &AgentRuntimeContext,
    goal: &Goal,
    cwd: &str,
    approval_mode: &ApprovalMode,
) -> TickOutcome {
    let started = std::time::Instant::now();
    let now = chrono::Utc::now();

    // A rail that has already tripped blocks before spending anything.
    if let Some(reason) = rail_tripped(goal, now) {
        let mut goal = goal.clone();
        goal.status = GoalStatus::Blocked;
        goal.blocked_reason = Some(reason.clone());
        let _ = goals::save(&goal);
        record(&goal, TickKind::Work, "—", &reason, false, started);
        return TickOutcome {
            kind: TickKind::Work,
            progressed: false,
            status: GoalStatus::Blocked,
            summary: reason,
            waited: false,
        };
    }

    // Everything answerable without a model, answered without one.
    let mut goal = goal.clone();
    let preflight_note = match preflight(&mut goal).await {
        PreFlight::Wait { why } => {
            // The bookmark still moves, so the short fuse applies and this does
            // not spin: the goal looks again in five minutes, having spent
            // nothing but one HTTP GET.
            goal.counters.last_tick_at = Some(now.to_rfc3339());
            let _ = goals::save(&goal);
            return TickOutcome {
                kind: TickKind::Work,
                progressed: false,
                status: goal.status,
                summary: why.clone(),
                waited: true,
            };
        }
        PreFlight::Spend { note } => note,
    };
    // Save what the pre-flight learned before the turn, so a tick that crashes
    // does not lose the fact that its build finished.
    let _ = goals::save(&goal);
    let goal = &goal;

    let before = goals::read_scratchpad(&goal.id).unwrap_or_else(|| {
        // First tick: give the goal the document it will spend its life editing.
        let seeded = goals::seed_scratchpad(goal);
        let _ = goals::write_scratchpad(&goal.id, &seeded);
        seeded
    });
    // What the pre-flight found goes into the document, not just into the
    // prompt: a tick that crashes before writing must not take the only record
    // of its finished build with it.
    let before = match &preflight_note {
        Some(note) => {
            let updated = goals::append_to_section(&before, "Log", &format!("- {note}"));
            let _ = goals::write_scratchpad(&goal.id, &updated);
            updated
        }
        None => before,
    };
    let kind = tick_kind(goal, &before);
    let tick_number = goal.counters.ticks + 1;
    let model = model_for(goal, kind);
    let persona = persona_for(goal);

    log::info!(
        "goal {} tick {tick_number} [{kind:?}] as {persona} on {model}",
        goal.id
    );

    let logger = DiagnosticsLogger::new().ok().map(Arc::new);
    let outcome = runtime::run_one_shot_task(
        context,
        RunOneShotRequest {
            persona_slug: &persona,
            cwd,
            model_name: &model,
            task: &tick_frame(goal, kind, tick_number),
            approval_mode: approval_mode.clone(),
            diagnostics: logger,
            instance_id: Some(goal.instance_id.clone()),
            preset_personas: None,
            goal_id: Some(goal.id.clone()),
        },
    )
    .await;

    let summary = match outcome {
        Ok(metalcraft::RunOutcome::Completed(state)) => state
            .final_answer()
            .unwrap_or("(the tick ended without saying anything)")
            .to_string(),
        Ok(metalcraft::RunOutcome::Interrupted { reason, .. }) => {
            format!("Tick stopped early: {reason}")
        }
        Ok(metalcraft::RunOutcome::Failed { node, error, .. }) => {
            format!("Tick failed in {node}: {error}")
        }
        Err(e) => format!("Tick could not run: {e}"),
    };

    let status_before = goal.status;
    // Re-read: the goal_* tools may have moved the status or the scratchpad
    // out from under the copy we started with, and theirs is the newer one.
    let mut goal = goals::get(&goal.id).unwrap_or_else(|| goal.clone());
    let after = goals::read_scratchpad(&goal.id).unwrap_or_default();
    let progressed = after != before;

    goal.counters.ticks = tick_number;
    goal.counters.last_tick_at = Some(now.to_rfc3339());
    goal.counters.no_progress_streak = if progressed {
        0
    } else {
        goal.counters.no_progress_streak + 1
    };

    // Check the rails again with the tick's own result folded in, so a streak
    // that just reached its limit blocks now rather than after one more.
    if goal.status == GoalStatus::Active
        && let Some(reason) = rail_tripped(&goal, now)
    {
        goal.status = GoalStatus::Blocked;
        goal.blocked_reason = Some(reason);
    }
    let _ = goals::save(&goal);

    // Last thing, always — see `hibernate_workspace`.
    hibernate_workspace(&goal).await;

    // A goal that has stopped has to say so, because nothing else will: the
    // heartbeat that would have mentioned it again is the thing that stopped.
    match goal.status {
        GoalStatus::Blocked if status_before == GoalStatus::Active => {
            let question = goal
                .blocked_reason
                .clone()
                .unwrap_or_else(|| "It stopped and did not say why.".into());
            announce(
                context,
                &goal,
                &format!(
                    "Your goal **{}** has stopped and needs you.\n\n{question}\n\nReply here \
                     to answer it and start it again.",
                    goal.title
                ),
            )
            .await;
        }
        GoalStatus::Done if status_before == GoalStatus::Active => {
            announce(
                context,
                &goal,
                &format!("Your goal **{}** is done.\n\n{summary}", goal.title),
            )
            .await;
        }
        _ => {}
    }

    record(&goal, kind, &model, &summary, progressed, started);

    TickOutcome {
        kind,
        progressed,
        status: goal.status,
        summary,
        waited: false,
    }
}

/// The persona a goal's ticks run as, falling back when its pack is not
/// installed.
///
/// The `goal-builder` / `goal-auditor` personas ship with the goal-agents pack.
/// A pod without it still runs goals — as the pod's default orchestrator, which
/// is worse at the job but not broken — rather than failing every tick with a
/// missing-persona error that says nothing about goals.
fn persona_for(goal: &Goal) -> String {
    let wanted = goal.kind.persona();
    if crate::persona::Persona::load(wanted, &crate::paths::personas_dir()).is_ok() {
        return wanted.to_string();
    }
    let fallback = runtime::configured_default_persona();
    log::warn!(
        "goal {} wants persona '{wanted}' (install the goal-agents pack); running as '{fallback}'",
        goal.id
    );
    fallback
}

fn record(
    goal: &Goal,
    kind: TickKind,
    model: &str,
    summary: &str,
    progressed: bool,
    started: std::time::Instant,
) {
    let progress = goal.progress();
    append_journal(
        &goal.id,
        &JournalEntry {
            at: chrono::Utc::now().to_rfc3339(),
            tick: goal.counters.ticks,
            kind,
            model: model.to_string(),
            summary: summary.chars().take(2000).collect(),
            status: goal.status,
            plan_done: progress.done,
            plan_total: progress.total,
            progressed,
            duration_secs: started.elapsed().as_secs(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::{Counters, GoalKind, Heartbeat, ModelTiers, Rails, Workspace};

    fn goal() -> Goal {
        Goal {
            id: "goal_test".into(),
            title: "t".into(),
            goal: "do the thing".into(),
            kind: GoalKind::Build,
            instance_id: "inst".into(),
            agent_preset: "general-agent".into(),
            workspace: Workspace::default(),
            status: GoalStatus::Active,
            blocked_reason: None,
            heartbeat: Heartbeat::default(),
            io: crate::scheduled_tasks::IoBinding::Unbound,
            journal_chat_id: None,
            rails: Rails::default(),
            counters: Counters::default(),
            pending_run: None,
            models: ModelTiers::default(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn a_goal_with_no_plan_plans() {
        assert_eq!(tick_kind(&goal(), "## Plan\n_none yet_\n"), TickKind::Plan);
    }

    #[test]
    fn a_goal_with_a_plan_works() {
        let doc = "## Plan\n- [ ] one\n";
        assert_eq!(tick_kind(&goal(), doc), TickKind::Work);
    }

    #[test]
    fn every_fifth_tick_reviews() {
        let mut g = goal();
        let doc = "## Plan\n- [ ] one\n";
        g.counters.ticks = REVIEW_EVERY - 1;
        assert_eq!(tick_kind(&g, doc), TickKind::Review);
    }

    #[test]
    fn a_bloated_scratchpad_forces_a_review_off_cadence() {
        let g = goal();
        let doc = format!("## Plan\n- [ ] one\n\n## Log\n{}", "- a line\n".repeat(200));
        assert_eq!(tick_kind(&g, &doc), TickKind::Review);
    }

    #[test]
    fn a_goal_that_never_ticked_is_due_now() {
        assert!(is_due(&goal(), chrono::Utc::now()));
    }

    #[test]
    fn a_goal_that_just_ticked_is_not() {
        let mut g = goal();
        g.counters.last_tick_at = Some(chrono::Utc::now().to_rfc3339());
        assert!(!is_due(&g, chrono::Utc::now()));
    }

    #[test]
    fn a_goal_becomes_due_after_its_interval() {
        let mut g = goal();
        let now = chrono::Utc::now();
        g.counters.last_tick_at =
            Some((now - chrono::Duration::minutes(31)).to_rfc3339());
        assert!(is_due(&g, now));
    }

    #[test]
    fn a_blocked_goal_never_ticks() {
        let mut g = goal();
        g.status = GoalStatus::Blocked;
        assert!(!is_due(&g, chrono::Utc::now()));
        g.status = GoalStatus::Done;
        assert!(!is_due(&g, chrono::Utc::now()));
        g.status = GoalStatus::Paused;
        assert!(!is_due(&g, chrono::Utc::now()));
    }

    #[test]
    fn an_unreadable_bookmark_does_not_wedge_a_goal() {
        let mut g = goal();
        g.counters.last_tick_at = Some("not a date".into());
        assert!(is_due(&g, chrono::Utc::now()));
    }

    #[test]
    fn rails_trip_on_ticks_streak_and_deadline() {
        let now = chrono::Utc::now();
        let mut g = goal();
        assert!(rail_tripped(&g, now).is_none());

        g.counters.ticks = g.rails.max_ticks;
        assert!(rail_tripped(&g, now).unwrap().contains("Out of ticks"));

        let mut g = goal();
        g.counters.no_progress_streak = g.rails.max_consecutive_no_progress;
        assert!(rail_tripped(&g, now).unwrap().contains("changed nothing"));

        let mut g = goal();
        g.rails.deadline = Some((now - chrono::Duration::hours(1)).to_rfc3339());
        assert!(rail_tripped(&g, now).unwrap().contains("deadline"));

        let mut g = goal();
        g.rails.compute_minutes_budget = Some(10);
        g.counters.compute_minutes_used = 10;
        assert!(rail_tripped(&g, now).unwrap().contains("Compute budget"));
    }

    #[test]
    fn a_future_deadline_does_not_trip() {
        let now = chrono::Utc::now();
        let mut g = goal();
        g.rails.deadline = Some((now + chrono::Duration::hours(1)).to_rfc3339());
        assert!(rail_tripped(&g, now).is_none());
    }

    #[test]
    fn tiers_map_onto_the_ladder() {
        assert_eq!(resolve_tier("mini"), runtime::AVAILABLE_MODELS[0]);
        assert_eq!(
            resolve_tier("strong"),
            runtime::AVAILABLE_MODELS[runtime::AVAILABLE_MODELS.len() - 1]
        );
    }

    #[test]
    fn a_goal_may_override_a_tier() {
        let mut g = goal();
        g.models.work = Some("mini".into());
        assert_eq!(model_for(&g, TickKind::Work), runtime::AVAILABLE_MODELS[0]);
        // and the ones it did not override keep the kind's tier
        assert_eq!(
            model_for(&g, TickKind::Plan),
            resolve_tier(TickKind::Plan.tier())
        );
    }

    #[test]
    fn a_stuck_goal_escalates_a_tier_by_itself() {
        let mut g = goal();
        assert_eq!(tier_for(&g, TickKind::Work), "standard");
        g.counters.no_progress_streak = 1;
        assert_eq!(
            tier_for(&g, TickKind::Work),
            "strong",
            "one tick that changed nothing buys the next one a better model"
        );
    }

    #[test]
    fn escalation_respects_the_top_of_the_ladder() {
        let mut g = goal();
        g.counters.no_progress_streak = 3;
        assert_eq!(tier_for(&g, TickKind::Review), "strong");
        // and an explicitly cheap goal still climbs, just from lower down
        g.models.work = Some("mini".into());
        assert_eq!(tier_for(&g, TickKind::Work), "standard");
    }

    #[test]
    fn the_frame_carries_the_scratchpad_and_the_pending_run() {
        let mut g = goal();
        g.pending_run = Some(crate::goals::PendingRun {
            workspace_id: "ws_1".into(),
            run_id: "run_9".into(),
            what: "cargo test".into(),
            started_at: String::new(),
        });
        let frame = tick_frame(&g, TickKind::Work, 3);
        assert!(frame.contains("tick 3"));
        assert!(frame.contains("run_9"), "the pending run must be named first");
        assert!(frame.contains("do the thing"), "the goal itself must be in the frame");
        assert!(frame.contains("first unchecked step"));
    }

    fn run(status: &str) -> crate::buildr::Run {
        crate::buildr::Run {
            id: "run_1".into(),
            status: status.into(),
            exit_code: None,
            cmd: None,
            output: None,
        }
    }

    #[test]
    fn a_running_build_is_waited_for_rather_than_watched() {
        let d = decide_pending(&Ok(run("running")), 4, "cargo test");
        assert!(matches!(d, PreFlight::Wait { .. }), "{d:?}");
    }

    #[test]
    fn a_build_that_will_never_land_stops_being_waited_for() {
        // Past the patience window nothing is coming back, and a goal that waits
        // forever costs nothing and does nothing — the quietest failure here.
        let d = decide_pending(&Ok(run("running")), PENDING_RUN_PATIENCE_MINS, "cargo test");
        match d {
            PreFlight::Spend { note } => assert!(note.unwrap().contains("not coming back")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_finished_build_carries_its_output_into_the_tick() {
        let mut r = run("failed");
        r.exit_code = Some(101);
        r.output = Some("error[E0308]: mismatched types".into());
        match decide_pending(&Ok(r), 3, "cargo test") {
            PreFlight::Spend { note } => {
                let note = note.unwrap();
                assert!(note.contains("failed"), "{note}");
                assert!(note.contains("exit 101"), "{note}");
                // the reason, not just the verdict — a failure without it is
                // one the next tick has to reproduce to learn anything
                assert!(note.contains("E0308"), "{note}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_vanished_run_is_not_waited_for() {
        let d = decide_pending(&Err(crate::buildr::Error::Gone), 1, "cargo build");
        match d {
            PreFlight::Spend { note } => assert!(note.unwrap().contains("no longer exists")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_pod_with_no_buildr_credential_stops_waiting_immediately() {
        let d = decide_pending(&Err(crate::buildr::Error::NotConfigured), 1, "cargo build");
        assert!(matches!(d, PreFlight::Spend { .. }), "{d:?}");
    }

    #[test]
    fn buildr_being_briefly_unreachable_is_waited_out_but_not_forever() {
        let blip = Err(crate::buildr::Error::Http("502".into()));
        assert!(matches!(decide_pending(&blip, 2, "x"), PreFlight::Wait { .. }));
        assert!(matches!(
            decide_pending(&blip, PENDING_RUN_PATIENCE_MINS + 1, "x"),
            PreFlight::Spend { .. }
        ));
    }

    #[test]
    fn the_review_frame_forbids_new_work() {
        let frame = tick_frame(&goal(), TickKind::Review, 5);
        assert!(frame.contains("Do no new work"));
        assert!(frame.contains("Never drop an unchecked step"));
    }
}
