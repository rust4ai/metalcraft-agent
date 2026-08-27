//! The turn's plan — the artifact that makes multi-step work checkable.
//!
//! An orchestrator that narrates a four-step plan and then delegates once has
//! not broken any rule, because until now the plan existed only as prose inside
//! one assistant message. Nothing held it. The cheapest next move after a single
//! `sub_agent` returned something plausible was always `say_to_user`, which is a
//! terminal tool: the turn ended there, three steps short, and looked finished.
//!
//! This module is the missing commitment device. [`update_plan`] writes the
//! steps down here, [`sub_agent`] records what a delegation reports it did *not*
//! finish, and [`say_to_user`] asks [`TurnPlan::blocking_reason`] before it is
//! allowed to close the turn. A plan with open steps, or an unacknowledged
//! handoff, means the reply tool returns an error instead of delivering — and
//! since metalcraft 0.10 a *failed* terminal tool no longer ends the turn, that
//! error hands control back to the model with the list of what it still owes.
//!
//! The store is per-turn: [`crate::runtime::build_agent_runtime`] creates one
//! and shares it with the three tools in that runtime's registry. A delegated
//! sub-agent gets `None` — it runs its own turn with its own obligations, and
//! must not be able to satisfy or block its parent's plan.
//!
//! [`update_plan`]: crate::tools::update_plan
//! [`sub_agent`]: crate::tools::sub_agent
//! [`say_to_user`]: crate::tools::say_to_user

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// How many times the reply tool may be turned away in one turn before the gate
/// gives up and lets the turn close.
///
/// A rail that can deadlock a turn is worse than the behaviour it corrects: a
/// model convinced it is finished, refused forever, would burn every one of the
/// 90 steps and then fail with nothing delivered. Two refusals is enough to
/// convert "I'll stop after one delegation" into "fine, I'll do the rest" while
/// leaving a model that genuinely cannot proceed a way out — it answers, and the
/// unfinished steps are visible in its own summary.
const MAX_GATE_REFUSALS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    InProgress,
    Done,
    /// Deliberately abandoned — an earlier step's result made it unnecessary.
    /// Counts as closed, so it does not block the turn.
    Skipped,
}

impl StepStatus {
    /// Does this step still owe work?
    fn is_open(self) -> bool {
        matches!(self, StepStatus::Pending | StepStatus::InProgress)
    }

    fn marker(self) -> &'static str {
        match self {
            StepStatus::Pending => "[ ]",
            StepStatus::InProgress => "[~]",
            StepStatus::Done => "[x]",
            StepStatus::Skipped => "[-]",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlanStep {
    /// What this step does, in one line, concrete enough to tell whether it happened.
    pub step: String,
    /// The persona this step is meant for. Advisory — the gate never checks that
    /// the delegation actually went there, because a plan that reroutes mid-turn
    /// is a plan working as intended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default)]
    pub status: StepStatus,
}

/// What a delegation reported it did not finish.
///
/// The sub-agent that just read the code is better placed than the orchestrator
/// to say what is left, so it says so and this is where that lands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    /// The persona (or tool set) the delegation ran as, for the message the gate
    /// shows the model.
    pub from: String,
    pub not_done: Vec<String>,
    /// Which persona the sub-agent thinks should pick the work up.
    pub suggest_persona: Option<String>,
}

/// Told whenever the plan changes, so a client can watch it being worked.
///
/// The plan is the one piece of an agent's reasoning that is already structured
/// — the model wrote it down as a list — and a list the person can watch being
/// crossed off is the difference between "it is doing something" and "it is on
/// step 3 of 5". Called synchronously from the tool that changed it, so it must
/// not block: send on a channel, do not wait on one.
pub type PlanSink = Arc<dyn Fn(&[PlanStep]) + Send + Sync>;

#[derive(Default)]
pub struct TurnPlan {
    steps: Vec<PlanStep>,
    /// Handoffs recorded since the last `set_steps`. Writing a new plan is the
    /// acknowledgement — the orchestrator has read them and decided what to do —
    /// so `set_steps` clears these.
    handoffs: Vec<Handoff>,
    refusals: usize,
    /// Where to announce changes. `None` for a run nobody is watching.
    on_change: Option<PlanSink>,
}

impl std::fmt::Debug for TurnPlan {
    // Hand-written because a sink is a closure, which has no Debug.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnPlan")
            .field("steps", &self.steps)
            .field("handoffs", &self.handoffs)
            .field("refusals", &self.refusals)
            .field("watched", &self.on_change.is_some())
            .finish()
    }
}

impl TurnPlan {
    /// Replace the plan wholesale and acknowledge any outstanding handoffs.
    ///
    /// Whole-plan replacement rather than per-step patching is deliberate: the
    /// model re-states the full list every time, so a step it has quietly
    /// dropped is visible in the diff instead of lingering as a stale entry
    /// nobody will ever close.
    pub fn set_steps(&mut self, steps: Vec<PlanStep>) {
        self.steps = steps;
        self.handoffs.clear();
        self.announce();
    }

    /// Tell the watcher, if there is one, what the plan looks like now.
    fn announce(&self) {
        if let Some(sink) = &self.on_change {
            sink(&self.steps);
        }
    }

    pub fn record_handoff(&mut self, handoff: Handoff) {
        self.handoffs.push(handoff);
    }

    /// Clear everything for a fresh turn.
    ///
    /// The plan is per-*turn*, but the runtime that owns it is not: the CLI
    /// builds one runtime and reuses it for the whole session. Without this, a
    /// plan left open at the end of one turn would block the next turn's first
    /// reply — the gate firing on work the user has already moved on from.
    pub fn reset(&mut self) {
        self.steps.clear();
        self.handoffs.clear();
        self.refusals = 0;
        // Announced too: a new turn starts with no plan, and a client still
        // showing the last one would be showing history as if it were live.
        self.announce();
    }

    pub fn steps(&self) -> &[PlanStep] {
        &self.steps
    }

    fn open_steps(&self) -> impl Iterator<Item = &PlanStep> {
        self.steps.iter().filter(|s| s.status.is_open())
    }

    /// The plan as the model should see it echoed back.
    pub fn render(&self) -> String {
        if self.steps.is_empty() {
            return "(no plan)".to_string();
        }
        self.steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let who = s
                    .persona
                    .as_deref()
                    .map(|p| format!(" → {p}"))
                    .unwrap_or_default();
                format!("{} {}. {}{}", s.status.marker(), i + 1, s.step, who)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Why the turn may not close yet, or `None` if it may.
    ///
    /// Two reasons, in the order that matters: an unacknowledged handoff (a
    /// delegation said outright that work remains) outranks open plan steps,
    /// because it carries the more specific instruction.
    pub fn blocking_reason(&self) -> Option<String> {
        if !self.handoffs.is_empty() {
            let detail = self
                .handoffs
                .iter()
                .map(|h| {
                    let next = h
                        .suggest_persona
                        .as_deref()
                        .map(|p| format!(" It suggests delegating the rest to `{p}`."))
                        .unwrap_or_default();
                    format!(
                        "- `{}` reported it did NOT finish: {}.{}",
                        h.from,
                        h.not_done.join("; "),
                        next
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Some(format!(
                "Not delivered — the last delegation came back unfinished:\n{detail}\n\n\
                 Do NOT answer yet. Either delegate the remaining work (a persona that only \
                 investigated cannot also have made the changes), or call `update_plan` to \
                 record what you are doing about it. If you genuinely cannot proceed without \
                 the user, use `ask_user` instead of `say_to_user`."
            ));
        }

        let open: Vec<&PlanStep> = self.open_steps().collect();
        if open.is_empty() {
            return None;
        }
        let detail = open
            .iter()
            .map(|s| {
                let who = s
                    .persona
                    .as_deref()
                    .map(|p| format!(" (→ {p})"))
                    .unwrap_or_default();
                format!("- {}{}", s.step, who)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "Not delivered — your plan still has {} open step(s):\n{detail}\n\n\
             Carry on: delegate the next one. If a step turned out to be unnecessary, call \
             `update_plan` and mark it `skipped`; if you need the user before you can continue, \
             use `ask_user` instead of `say_to_user`.",
            open.len()
        ))
    }

    /// Record that the gate is about to turn the reply tool away, and say whether
    /// it still should. `false` once [`MAX_GATE_REFUSALS`] is spent — the turn is
    /// allowed to close rather than spin.
    pub fn note_refusal(&mut self) -> bool {
        if self.refusals >= MAX_GATE_REFUSALS {
            return false;
        }
        self.refusals += 1;
        true
    }
}

pub type SharedTurnPlan = Arc<Mutex<TurnPlan>>;

pub fn new_shared() -> SharedTurnPlan {
    new_shared_watched(None)
}

/// A plan whose every change is announced to `sink`.
pub fn new_shared_watched(sink: Option<PlanSink>) -> SharedTurnPlan {
    Arc::new(Mutex::new(TurnPlan {
        on_change: sink,
        ..TurnPlan::default()
    }))
}

/// Take the plan lock, recovering from a poisoned mutex.
///
/// A panic in one tool must not turn the plan into a permanent error for the
/// rest of the turn. This is advisory bookkeeping: the worst case of reading it
/// after a panic elsewhere is a slightly stale step list, which is strictly
/// better than every subsequent reply failing.
pub fn lock(plan: &SharedTurnPlan) -> std::sync::MutexGuard<'_, TurnPlan> {
    plan.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, status: StepStatus) -> PlanStep {
        PlanStep {
            step: name.to_string(),
            persona: None,
            status,
        }
    }

    #[test]
    fn an_empty_plan_never_blocks() {
        let plan = TurnPlan::default();
        assert!(plan.blocking_reason().is_none());
    }

    #[test]
    fn open_steps_block_and_closed_ones_do_not() {
        let mut plan = TurnPlan::default();
        plan.set_steps(vec![
            step("research the repo", StepStatus::Done),
            step("edit the landing page", StepStatus::Pending),
        ]);
        let reason = plan.blocking_reason().expect("should block");
        assert!(reason.contains("edit the landing page"));
        assert!(!reason.contains("research the repo"));

        plan.set_steps(vec![
            step("research the repo", StepStatus::Done),
            step("edit the landing page", StepStatus::Skipped),
        ]);
        assert!(plan.blocking_reason().is_none());
    }

    #[test]
    fn an_unfinished_delegation_blocks_even_with_no_plan() {
        let mut plan = TurnPlan::default();
        plan.record_handoff(Handoff {
            from: "research-agent".into(),
            not_done: vec!["4 stale feature claims still need editing".into()],
            suggest_persona: Some("coding-agent".into()),
        });
        let reason = plan.blocking_reason().expect("should block");
        assert!(reason.contains("4 stale feature claims"));
        assert!(reason.contains("coding-agent"));
    }

    /// Writing a plan is how the orchestrator acknowledges a handoff — otherwise
    /// it would be blocked forever by a report it has already acted on.
    #[test]
    fn setting_a_plan_acknowledges_handoffs() {
        let mut plan = TurnPlan::default();
        plan.record_handoff(Handoff {
            from: "research-agent".into(),
            not_done: vec!["edits".into()],
            suggest_persona: None,
        });
        plan.set_steps(vec![step("apply the edits", StepStatus::Done)]);
        assert!(plan.blocking_reason().is_none());
    }

    /// The rail must not be able to deadlock a turn.
    #[test]
    fn the_gate_gives_up_after_two_refusals() {
        let mut plan = TurnPlan::default();
        plan.set_steps(vec![step("still open", StepStatus::Pending)]);
        assert!(plan.note_refusal());
        assert!(plan.note_refusal());
        assert!(!plan.note_refusal(), "third attempt must be let through");
    }

    /// The runtime outlives the turn in the CLI, so the plan must not.
    #[test]
    fn reset_clears_steps_handoffs_and_refusals() {
        let mut plan = TurnPlan::default();
        plan.set_steps(vec![step("open", StepStatus::Pending)]);
        plan.record_handoff(Handoff {
            from: "x".into(),
            not_done: vec!["y".into()],
            suggest_persona: None,
        });
        assert!(plan.note_refusal());
        plan.reset();
        assert!(plan.blocking_reason().is_none());
        assert!(plan.steps().is_empty());
        // Refusals are spent per turn too, so the next turn gets a full budget.
        plan.set_steps(vec![step("open again", StepStatus::Pending)]);
        assert!(plan.note_refusal());
        assert!(plan.note_refusal());
        assert!(!plan.note_refusal());
    }

    #[test]
    fn render_marks_each_status() {
        let mut plan = TurnPlan::default();
        plan.set_steps(vec![
            PlanStep {
                step: "read the repo".into(),
                persona: Some("research-agent".into()),
                status: StepStatus::Done,
            },
            step("edit", StepStatus::InProgress),
        ]);
        let out = plan.render();
        assert!(out.contains("[x] 1. read the repo → research-agent"));
        assert!(out.contains("[~] 2. edit"));
    }
}
