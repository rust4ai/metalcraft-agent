//! The conductor: the small agent that *is* the project.
//!
//! A project runs two agents, and the split between them is the design. The
//! **worker** does the work in a fresh context every tick and knows only what it
//! is handed. The **conductor** is the one thing that persists: it holds the
//! plan, decides whether the project is done, and writes the briefing that tells
//! the worker what this tick is for. It never does the work itself.
//!
//! That split exists to fix one thing in particular. Until now a single agent
//! planned the work, did the work, and then judged whether its own work met its
//! own goal — which is the weakest possible arrangement for the decision that
//! matters most. The thing that says "done" should not be the thing that wants
//! to be finished.
//!
//! # Two kinds of memory, deliberately
//!
//! The conductor has both, and they are not the same thing:
//!
//! | | this ledger | `mem_*` instance memory |
//! | --- | --- | --- |
//! | shape | one document, verbatim, injected in full every tick | ranked recall, injected by relevance |
//! | written | rewritten by the conductor every tick | appended when something durable is learned; distilled by the nightly dream |
//! | holds | its running thesis about **this** project — bearing, what has been tried, what it is watching | what outlives this project — how this repo builds, which delegate is reliable at what |
//! | if it is wrong | this project stalls | the pod forgets something |
//!
//! The ledger is to the conductor what the scratchpad is to the worker, and the
//! reason both exist is the same: recall is fuzzy and ranked on purpose, and a
//! thing that must be true and complete *every* tick cannot be left to it.
//!
//! The ledger is written twice per tick, from two directions:
//!
//! 1. **The conductor rewrites it** at the end of its own turn — its judgement,
//!    in its own words.
//! 2. **The runner appends what the worker did** when the worker's turn returns
//!    ([`record_worker_return`]). That half is written by code from structured
//!    facts, not by a model, so it cannot be forgotten by one — the same
//!    division as everywhere else here: generated judgement, constant structure.
//!
//! There is deliberately no second conductor turn after the worker returns. It
//! would double the conductor's cost per tick to reflect on something the next
//! tick reads anyway fifteen minutes later, and nothing is lost in between: the
//! appended record is derived from the dispatch reports and the task deltas, not
//! from anyone's recollection.

use crate::approval::ApprovalMode;
use crate::projects::{self, Project};
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest};

/// The conductor's persona. Small on purpose: it reads state and writes
/// judgement, so it has no file tools, no shell and no delegation.
pub const CONDUCTOR_PERSONA: &str = "project-conductor";

/// The ledger's sections, in order. Fixed for the same reason the scratchpad's
/// are: the frame refers to them by name, and a document whose shape drifts is
/// one nothing can act on.
pub const SECTIONS: &[&str] = &["Bearing", "Learned", "Tried", "Watching"];

/// Cap on the ledger when injected. Past this the oldest `Tried` entries are the
/// first to go — they are the part the conductor would have folded into
/// `Learned` anyway.
const MAX_INJECT_BYTES: usize = 8 * 1024;

fn path(project_id: &str) -> std::path::PathBuf {
    crate::paths::project_dir(project_id).join("conductor.md")
}

pub fn read(project_id: &str) -> Option<String> {
    std::fs::read_to_string(path(project_id)).ok()
}

pub fn write(project_id: &str, markdown: &str) -> Result<(), String> {
    let dir = crate::paths::project_dir(project_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let p = path(project_id);
    let tmp = p.with_extension("md.tmp");
    std::fs::write(&tmp, markdown).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// Append one line under a section, creating the section if the document drifted.
pub fn append(project_id: &str, section: &str, line: &str) -> Result<(), String> {
    let current = read(project_id).unwrap_or_else(|| seed(project_id));
    let updated = projects::append_to_section(&current, section, line);
    write(project_id, &updated)
}

/// The document a project's conductor starts with.
pub fn seed(_project_id: &str) -> String {
    "## Bearing\n\
     _First tick: read the goal, decide what this project has to do first, and say so here._\n\n\
     ## Learned\n(nothing yet)\n\n\
     ## Tried\n(nothing yet)\n\n\
     ## Watching\n(none)\n"
        .to_string()
}

/// The ledger as the conductor is shown it, bounded.
pub fn for_injection(project_id: &str) -> String {
    let doc = read(project_id).unwrap_or_else(|| seed(project_id));
    if doc.len() <= MAX_INJECT_BYTES {
        return doc;
    }
    // Same trade the scratchpad makes: drop the oldest history rather than
    // truncate mid-document, because history is what a groom folds away anyway.
    projects::trim_for_injection(&doc)
}

/// What the runner writes into `Tried` when a worker turn comes back.
///
/// Deliberately factual and code-written: what ran, what moved, what is still
/// owed. The conductor's next turn is where that becomes judgement.
pub fn record_worker_return(
    project_id: &str,
    tick: u32,
    summary: &str,
    tasks_before: &[crate::project_tasks::Task],
    tasks_after: &[crate::project_tasks::Task],
) {
    let closed: Vec<&str> = tasks_after
        .iter()
        .filter(|a| {
            a.status == crate::project_tasks::TaskStatus::Done
                && tasks_before
                    .iter()
                    .find(|b| b.id == a.id)
                    .is_none_or(|b| b.status != crate::project_tasks::TaskStatus::Done)
        })
        .map(|t| t.id.as_str())
        .collect();
    let blocked: Vec<&str> = tasks_after
        .iter()
        .filter(|a| {
            a.status == crate::project_tasks::TaskStatus::Blocked
                && tasks_before
                    .iter()
                    .find(|b| b.id == a.id)
                    .is_none_or(|b| b.status != crate::project_tasks::TaskStatus::Blocked)
        })
        .map(|t| t.id.as_str())
        .collect();
    let retried: Vec<String> = tasks_after
        .iter()
        .filter(|a| {
            tasks_before
                .iter()
                .find(|b| b.id == a.id)
                .is_some_and(|b| a.attempts > b.attempts)
        })
        .map(|t| format!("{} (attempt {})", t.id, t.attempts))
        .collect();

    let mut parts: Vec<String> = Vec::new();
    if !closed.is_empty() {
        parts.push(format!("closed {}", closed.join(", ")));
    }
    if !blocked.is_empty() {
        parts.push(format!("blocked {}", blocked.join(", ")));
    }
    if !retried.is_empty() {
        parts.push(format!("came back unfinished: {}", retried.join(", ")));
    }
    if parts.is_empty() {
        parts.push("nothing moved".to_string());
    }

    // "tick 7", never "t7": the rest of this document is full of task ids, and a
    // tick rendered the same way is one more thing to disambiguate while reading
    // the one section that exists to be read quickly.
    let line = format!(
        "- **tick {tick}** — {}. Worker said: {}",
        parts.join("; "),
        summary.replace('\n', " ").chars().take(400).collect::<String>()
    );
    if let Err(e) = append(project_id, "Tried", &line) {
        log::warn!("project {project_id}: could not record the worker's return ({e})");
    }
}

// ── the worker's brief ───────────────────────────────────────────────────────

/// Ask the conductor to write the worker's standing instructions, from the goal.
///
/// Called once, on a project's first tick. A project aimed at a Rust service and
/// one aimed at a docs site should not get the same worker, and a generic persona
/// cannot say what a generated one can: which repo, which stack, what "verified"
/// looks like here, which conventions matter.
///
/// Once, not every tick, and the distinction is load-bearing. This is what the
/// project *is*; the briefing is what this tick is *for*. Re-deriving the first
/// from a model every fifteen minutes would cost more and drift — the worker
/// would find its own standing instructions quietly rewritten under it, which is
/// the same instability the fresh-per-tick design exists to avoid.
///
/// Returns `None` when it cannot be written. The worker then runs on its persona
/// alone, exactly as it did before briefs existed.
pub async fn compose_worker_brief(
    context: &AgentRuntimeContext,
    project: &Project,
    cwd: &str,
    approval_mode: &ApprovalMode,
) -> Option<String> {
    if crate::persona::Persona::load(CONDUCTOR_PERSONA, &crate::paths::personas_dir()).is_err() {
        return None;
    }
    let repos = project
        .workspace
        .repos
        .iter()
        .map(|r| {
            format!(
                "{}{}",
                r.full_name,
                r.branch.as_deref().map(|b| format!(" (branch {b})")).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    let task = format!(
        "Write the standing instructions for the worker agent on this project.\n\n\
         The goal:\n{}\n\n\
         Repository: {}\n\n\
         These instructions go in the worker's system prompt and are written ONCE — they are what \
         this project *is*, not what any one tick is *for*. You will write the per-tick briefing \
         separately, every tick, so do not put anything here that changes.\n\n\
         Say what a competent stranger would need to know to work on this and nothing else:\n\
         - what the project is, in a sentence\n\
         - the stack and where things live, as far as the goal implies them\n\
         - what 'done' and 'verified' mean for work of this kind\n\
         - conventions worth holding to, and anything that is explicitly out of scope\n\n\
         Do not restate the kanban-style protocol, the task tools, or the standing rules — the \
         worker is given all of those anyway, and repeating them wastes the room. Do not invent \
         facts about the codebase you have not been told; say what to check instead. Under 300 \
         words. Output the instructions only — no preamble, no heading, no code fence.",
        project.goal.trim(),
        if repos.is_empty() { "(none named yet)" } else { &repos },
    );

    let outcome = runtime::run_one_shot_task(
        context,
        RunOneShotRequest {
            persona_slug: CONDUCTOR_PERSONA,
            cwd,
            model_name: &crate::project_tick::resolve_tier("strong"),
            task: &task,
            approval_mode: approval_mode.clone(),
            diagnostics: None,
            instance_id: Some(project.conductor_instance(&project.instance_id).to_string()),
            preset_personas: None,
            project_brief: None,
            project_id: None,
        },
    )
    .await;

    match outcome {
        Ok(metalcraft::RunOutcome::Completed(state)) => state
            .final_answer()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        other => {
            log::warn!(
                "project {}: could not compose the worker brief ({other:?}); \
                 the worker will run on its persona alone",
                project.id
            );
            None
        }
    }
}

// ── the turn ─────────────────────────────────────────────────────────────────

/// What the conductor decided this tick.
#[derive(Debug, Clone)]
pub struct Briefing {
    /// What to tell the worker. The situational half of its frame; the runner
    /// supplies the invariant half.
    pub text: String,
    /// False when the conductor could not run and this is the fallback. The
    /// journal records it, because "the conductor was down for six ticks" is
    /// the sort of thing that explains a project that went nowhere.
    pub from_conductor: bool,
}

/// The briefing used when the conductor cannot run.
///
/// Fail-open, like every other model in this system: a composer that is down
/// must never wedge a project. The fallback is deliberately plain — it says what
/// is ready and nothing else, because inventing situational advice without a
/// model is how a fallback starts lying.
pub fn fallback(tasks: &[crate::project_tasks::Task]) -> Briefing {
    let ready = crate::project_tasks::ready(tasks);
    let text = if ready.is_empty() {
        "No task is ready. Work out why from the plan below — something is blocked, waiting, or \
         missing — and either unblock it or say what is needed."
            .to_string()
    } else {
        format!(
            "Ready now: {}. Take what you can finish this tick.",
            ready
                .iter()
                .map(|t| format!("{} ({})", t.id, t.title))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Briefing {
        text,
        from_conductor: false,
    }
}

/// The prompt the conductor is given each tick.
pub fn conductor_frame(
    project: &Project,
    tick_number: u32,
    tasks: &[crate::project_tasks::Task],
    preflight_note: Option<&str>,
    recent_journal: &str,
) -> String {
    format!(
        "You are the conductor of a long-running project. This is tick {tick_number}.\n\n\
         You do not do the work. A worker agent does, in a fresh context that knows only what \
         you write for it. Your job this tick, in order:\n\n\
         1. **Groom the plan.** `task_add` what is missing, `task_update` what has been \
         re-scoped, `task_drop` what reality has made pointless. Tasks with no `deps` run at the \
         same time, so leave deps empty wherever two tasks are genuinely independent.\n\
         2. **Decide whether this project is still going.** `project_complete` when the goal is \
         met — you are the one who judges that, not the worker. `project_block` only when a \
         person is genuinely needed.\n\
         3. **Update your ledger** with `conductor_write`: your bearing, what you have learned \
         about working this project, what you are watching. Use `mem_remember` for anything that \
         outlives this project — how this repo builds, which delegate is reliable at what.\n\
         4. **Write the briefing.** Your final answer IS the briefing, and it is the only thing \
         the worker hears from you. Say what this tick is for and why, name the tasks to take, \
         and name any decision already made that it must not re-litigate. Do not repeat the \
         standing rules — the worker is given those anyway. A few sentences.\n\n\
         ---\n\n\
         ## The goal\n{}\n\n\
         ## Your ledger\n\n{}\n\n\
         ## The plan\n\n{}\n\n\
         ## The worker's scratchpad\n\n{}\n\n\
         ## Recent ticks\n{}\n{}",
        project.goal.trim(),
        for_injection(&project.id),
        crate::project_tasks::render(tasks),
        projects::read_scratchpad(&project.id).unwrap_or_else(|| "(empty)".into()),
        if recent_journal.is_empty() {
            "(none yet)"
        } else {
            recent_journal
        },
        preflight_note
            .map(|n| format!("\n## What landed since the last tick\n{n}\n"))
            .unwrap_or_default(),
    )
}

/// Run the conductor's turn and return its briefing.
///
/// Never fails: a conductor that cannot run yields the fallback briefing and the
/// tick goes ahead. The alternative — no tick because the composer was down — is
/// strictly worse than a tick with a plainer frame.
#[allow(clippy::too_many_arguments)]
pub async fn conduct(
    context: &AgentRuntimeContext,
    project: &Project,
    cwd: &str,
    approval_mode: &ApprovalMode,
    tick_number: u32,
    tasks: &[crate::project_tasks::Task],
    preflight_note: Option<&str>,
    recent_journal: &str,
) -> Briefing {
    if crate::persona::Persona::load(CONDUCTOR_PERSONA, &crate::paths::personas_dir()).is_err() {
        log::warn!(
            "project {}: no `{CONDUCTOR_PERSONA}` persona (install the project-agents pack); \
             running on the fallback briefing",
            project.id
        );
        return fallback(tasks);
    }

    let model = crate::project_tick::resolve_tier(
        project.models.plan.as_deref().unwrap_or("strong"),
    );
    let outcome = runtime::run_one_shot_task(
        context,
        RunOneShotRequest {
            persona_slug: CONDUCTOR_PERSONA,
            cwd,
            model_name: &model,
            task: &conductor_frame(project, tick_number, tasks, preflight_note, recent_journal),
            approval_mode: approval_mode.clone(),
            diagnostics: None,
            instance_id: Some(project.conductor_instance(&project.instance_id).to_string()),
            preset_personas: None,
            project_brief: None,
            project_id: Some(project.id.clone()),
        },
    )
    .await;

    match outcome {
        Ok(metalcraft::RunOutcome::Completed(state)) => match state.final_answer() {
            Some(text) if !text.trim().is_empty() => Briefing {
                text: text.trim().to_string(),
                from_conductor: true,
            },
            // It ran, groomed the plan, and said nothing. The plan changes are
            // already durable; only the briefing is missing.
            _ => fallback(tasks),
        },
        other => {
            log::warn!("project {}: conductor turn did not complete ({other:?})", project.id);
            fallback(tasks)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_tasks::{Task, TaskStatus};

    fn t(id: &str, status: TaskStatus, attempts: u32) -> Task {
        Task {
            id: id.into(),
            title: format!("task {id}"),
            detail: String::new(),
            status,
            deps: Vec::new(),
            assignee: None,
            mutates_workspace: true,
            pending_run: None,
            gate: None,
            attempts,
            evidence: Vec::new(),
            blocked_reason: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn the_seed_has_every_section_the_frame_names() {
        let doc = seed("proj_1");
        for s in SECTIONS {
            assert!(
                projects::section_body(&doc, s).is_some(),
                "the seeded ledger is missing `{s}`, which the frame refers to by name"
            );
        }
    }

    #[test]
    fn the_fallback_says_what_is_ready_and_nothing_more() {
        // A fallback that invented situational advice without a model would be
        // a fallback that lies.
        let tasks = vec![t("t1", TaskStatus::Todo, 0), t("t2", TaskStatus::Done, 0)];
        let b = fallback(&tasks);
        assert!(!b.from_conductor, "the journal has to be able to tell");
        assert!(b.text.contains("t1"), "{}", b.text);
        assert!(!b.text.contains("t2"), "a finished task is not ready: {}", b.text);
    }

    #[test]
    fn an_empty_plan_gets_a_fallback_that_says_so() {
        let b = fallback(&[t("t1", TaskStatus::Blocked, 0)]);
        assert!(b.text.contains("No task is ready"), "{}", b.text);
    }
}
