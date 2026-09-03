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

/// The model for one tick, honouring the goal's own overrides.
fn model_for(goal: &Goal, kind: TickKind) -> String {
    let override_tier = match kind {
        TickKind::Plan => goal.models.plan.as_deref(),
        TickKind::Work => goal.models.work.as_deref(),
        TickKind::Review => goal.models.review.as_deref(),
    };
    resolve_tier(override_tier.unwrap_or_else(|| kind.tier()))
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
        };
    }

    let before = goals::read_scratchpad(&goal.id).unwrap_or_else(|| {
        // First tick: give the goal the document it will spend its life editing.
        let seeded = goals::seed_scratchpad(goal);
        let _ = goals::write_scratchpad(&goal.id, &seeded);
        seeded
    });
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
    record(&goal, kind, &model, &summary, progressed, started);

    TickOutcome {
        kind,
        progressed,
        status: goal.status,
        summary,
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

    #[test]
    fn the_review_frame_forbids_new_work() {
        let frame = tick_frame(&goal(), TickKind::Review, 5);
        assert!(frame.contains("Do no new work"));
        assert!(frame.contains("Never drop an unchecked step"));
    }
}
