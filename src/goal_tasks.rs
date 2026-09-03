//! A goal's task list: the structured half of what a goal knows.
//!
//! A goal's scratchpad already carried a plan — as markdown checkboxes under
//! `## Plan`, parsed back out by [`crate::goals::progress_of`] to draw a
//! progress bar. That is a task table stored as prose, maintained by a model
//! the tick frame has to *beg* not to lose rows from ("never drop an unchecked
//! step"). A store cannot drop a row, so the plan moves here and the scratchpad
//! keeps what markdown is actually good at: State, Log, decisions, blockers.
//!
//! What that buys, in order of how much it matters:
//!
//! * **Nothing is silently lost.** The model manipulates tasks with tools; it
//!   never rewrites the list, so it can never drop half of it while rewriting.
//! * **Done means evidence.** [`complete`] requires an [`Evidence`] item — a
//!   commit, a run and its exit code, a finding id. A box cannot be checked by
//!   assertion.
//! * **Dependencies, and therefore parallelism.** `deps` says what must land
//!   first; everything else is [`ready`] at the same time, which is what a tick
//!   fans out over.
//! * **Long work in flight, per task.** Each task owns its own
//!   [`PendingRun`](crate::goals::PendingRun), so a goal can have three builds
//!   running at once and poll all of them for free — the pre-flight is HTTP, not
//!   a model.
//!
//! One file per goal at `<data>/goals/<id>/tasks.json`, whole-file rewrite with
//! a tmp+rename, exactly like the findings ledger next to it: a plan is tens of
//! rows, not thousands, and an atomic replace cannot leave half a list behind.
//! There is one writer — the tick — so there is nothing to lock.
//!
//! A goal created before this existed has no `tasks.json`, and every caller
//! falls back to the checkbox parser. Nothing has to be migrated.

use serde::{Deserialize, Serialize};

use crate::goals::{PendingRun, Progress};
use crate::paths;

/// How many tasks one goal may hold.
///
/// Not a storage limit — a limit on what a plan can be. A goal that has decomposed
/// itself into eighty tasks has not planned; it has listed, and the review tick
/// will spend its whole budget grooming the list instead of the work.
pub const MAX_TASKS: usize = 60;

/// Where a task has got to.
///
/// There is no `Ready` variant: readiness is *derived* from the dependency
/// graph ([`is_ready`]) rather than stored, so it can never disagree with the
/// deps it is supposed to summarise. Storing it would mean a promote pass, and
/// a promote pass means two sources of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not started. Ready when every dependency has landed.
    Todo,
    /// In flight across ticks: this task started something long (a build, a
    /// test run) and is owed its result. The pre-flight polls it without
    /// spending a model.
    Waiting,
    /// Stopped on something this goal cannot resolve by itself. Unlike
    /// [`crate::goals::GoalStatus::Blocked`] this stops **one task**, not the
    /// goal — its siblings keep going, which is most of the point of having
    /// tasks at all.
    Blocked,
    Done,
    /// Reality moved and this task no longer makes sense. Kept rather than
    /// deleted so a later tick does not re-derive it from the same reasoning
    /// that produced it the first time.
    Dropped,
}

impl TaskStatus {
    /// Whether a dependent may start. `Dropped` satisfies a dependency for the
    /// same reason `archived` does on a Hermes board: work that will never
    /// happen must not hold its dependents hostage forever.
    pub fn satisfies_dependency(&self) -> bool {
        matches!(self, Self::Done | Self::Dropped)
    }

    /// Whether this task still wants somebody's attention.
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Todo | Self::Waiting | Self::Blocked)
    }
}

/// What kind of proof a task finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A commit sha on the goal's branch.
    Commit,
    /// A buildr run id, ideally with its exit code — `r_88 exit 0`.
    Run,
    /// A findings-ledger id (`f3`), for audit goals.
    Finding,
    /// A path the task produced or changed.
    File,
    /// Everything else. Deliberately last, and deliberately not the default:
    /// prose is what the scratchpad is for, and a task whose only proof is a
    /// sentence has not really been verified.
    Note,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub value: String,
    pub at: String,
}

impl Evidence {
    pub fn new(kind: EvidenceKind, value: &str) -> Self {
        Self {
            kind,
            value: value.trim().to_string(),
            at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// One slice of a goal's plan.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Task {
    /// Short and typeable (`t1`, `t2`) — it is quoted in the scratchpad, in
    /// handoffs and in the journal, so it has to survive being retyped.
    pub id: String,
    /// One line. This is what the rendered plan shows and what the UI lists.
    pub title: String,
    /// What a delegate is handed when this task is dispatched. A delegate has
    /// none of the goal's context, so this carries every decision it depends on
    /// — the same contract a sub-agent's `task` argument has always had.
    #[serde(default)]
    pub detail: String,
    pub status: TaskStatus,
    /// Task ids **within this goal** that must land first.
    #[serde(default)]
    pub deps: Vec<String>,
    /// Persona to delegate this to. `None` means the tick does it itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Whether running this task writes to the goal's workspace.
    ///
    /// **Defaults to `true`**, and the asymmetry is deliberate: a task wrongly
    /// marked read-only can be dispatched alongside another that is editing the
    /// same sprite, and two agents writing one workspace corrupt each other
    /// silently. A task wrongly marked as mutating merely runs on its own. The
    /// cheap mistake is the safe one.
    #[serde(default = "yes")]
    pub mutates_workspace: bool,
    /// A long-running command this task started and did not wait for. The
    /// pre-flight reads it on the next tick, spending nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_run: Option<PendingRun>,
    /// A command that must exit 0 before this task may be `Done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// How many times this task has been attempted and come back unfinished.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn yes() -> bool {
    true
}

/// What a caller asks for when adding a task. `deps` may name task ids that do
/// not exist yet **within the same batch** — see [`add_many`].
#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub title: String,
    pub detail: String,
    pub deps: Vec<String>,
    pub assignee: Option<String>,
    pub mutates_workspace: Option<bool>,
    pub gate: Option<String>,
}

fn path(goal_id: &str) -> std::path::PathBuf {
    paths::goal_dir(goal_id).join("tasks.json")
}

/// Every task, in the order they were added.
///
/// Insertion order, not sorted: a plan reads top to bottom, and re-ordering it
/// under the reader would make "the first unchecked step" mean something
/// different on every tick.
pub fn list(goal_id: &str) -> Vec<Task> {
    std::fs::read_to_string(path(goal_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Whether this goal has a task list at all.
///
/// False for every goal created before tasks existed, which is what the
/// checkbox fallbacks key off.
pub fn exists(goal_id: &str) -> bool {
    path(goal_id).exists()
}

pub fn save(goal_id: &str, tasks: &[Task]) -> Result<(), String> {
    let dir = paths::goal_dir(goal_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(tasks).map_err(|e| e.to_string())?;
    let p = path(goal_id);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

pub fn get(tasks: &[Task], id: &str) -> Option<Task> {
    tasks.iter().find(|t| t.id == id).cloned()
}

/// Whether this task can be started now: not finished, and every dependency has
/// landed.
pub fn is_ready(task: &Task, all: &[Task]) -> bool {
    if task.status != TaskStatus::Todo {
        return false;
    }
    task.deps.iter().all(|d| {
        all.iter()
            .find(|t| &t.id == d)
            // A dep that no longer exists cannot block: it was dropped from the
            // list by a groom, and holding its dependents forever on a row
            // nobody can see is the worst of both.
            .is_none_or(|t| t.status.satisfies_dependency())
    })
}

/// Everything startable right now — what a tick fans out over.
pub fn ready(tasks: &[Task]) -> Vec<Task> {
    tasks
        .iter()
        .filter(|t| is_ready(t, tasks))
        .cloned()
        .collect()
}

/// Checked and total, for the progress bar.
///
/// `Dropped` counts as neither: a plan that shed three steps should read as
/// "2 of 4", not "2 of 7 with three that will never move".
pub fn progress(tasks: &[Task]) -> Progress {
    let mut p = Progress::default();
    for t in tasks {
        if t.status == TaskStatus::Dropped {
            continue;
        }
        p.total += 1;
        if t.status == TaskStatus::Done {
            p.done += 1;
        }
    }
    p
}

/// The next id, from the high-water mark rather than the length: a dropped task
/// must not hand its id to a different one, because the old id is already
/// quoted in a scratchpad somewhere.
fn next_id(tasks: &[Task]) -> String {
    let n = tasks
        .iter()
        .filter_map(|t| t.id.trim_start_matches('t').parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    format!("t{n}")
}

/// Add a batch of tasks, resolving intra-batch dependencies.
///
/// Batch rather than one at a time because a plan is written all at once, and a
/// planning tick that had to add five tasks in five calls would spend most of
/// its turn on bookkeeping. Within a batch, `deps` may name either an existing
/// task id or a **0-based index into this batch** (as a bare number, e.g. `"0"`)
/// — a plan describes its own shape before its rows have ids.
///
/// Rejects, rather than silently repairing: an unknown dependency, a
/// self-dependency, a cycle, or a batch that would take the goal past
/// [`MAX_TASKS`]. A plan with a cycle is a plan the goal would never finish,
/// and discovering that on tick 40 is far worse than being told now.
pub fn add_many(goal_id: &str, new: &[NewTask]) -> Result<Vec<Task>, String> {
    if new.is_empty() {
        return Err("no tasks given".into());
    }
    let mut tasks = list(goal_id);
    if tasks.len() + new.len() > MAX_TASKS {
        return Err(format!(
            "that would be {} tasks; a goal holds at most {MAX_TASKS}. Drop what is done or \
             finished with, or make the plan coarser.",
            tasks.len() + new.len()
        ));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let existing_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();

    // Mint every id first, so a batch can refer to its own rows by index.
    let mut minted: Vec<String> = Vec::with_capacity(new.len());
    let mut scratch = tasks.clone();
    for _ in new {
        let id = next_id(&scratch);
        scratch.push(Task {
            id: id.clone(),
            title: String::new(),
            detail: String::new(),
            status: TaskStatus::Todo,
            deps: Vec::new(),
            assignee: None,
            mutates_workspace: true,
            pending_run: None,
            gate: None,
            attempts: 0,
            evidence: Vec::new(),
            blocked_reason: None,
            created_at: now.clone(),
            updated_at: now.clone(),
        });
        minted.push(id);
    }

    let mut added: Vec<Task> = Vec::with_capacity(new.len());
    for (i, n) in new.iter().enumerate() {
        let title = n.title.trim();
        if title.is_empty() {
            return Err(format!("tasks[{i}] has no title"));
        }
        let mut deps = Vec::new();
        for d in &n.deps {
            let d = d.trim();
            let resolved = if let Ok(idx) = d.parse::<usize>() {
                // A bare number is an index into this batch.
                minted.get(idx).cloned().ok_or_else(|| {
                    format!("tasks[{i}] depends on index {idx}, but the batch has {} tasks", new.len())
                })?
            } else if existing_ids.iter().any(|e| e == d) || minted.iter().any(|m| m == d) {
                d.to_string()
            } else {
                return Err(format!("tasks[{i}] depends on '{d}', which is not a task"));
            };
            if resolved == minted[i] {
                return Err(format!("tasks[{i}] depends on itself"));
            }
            if !deps.contains(&resolved) {
                deps.push(resolved);
            }
        }
        added.push(Task {
            id: minted[i].clone(),
            title: title.to_string(),
            detail: n.detail.trim().to_string(),
            status: TaskStatus::Todo,
            deps,
            assignee: n.assignee.clone().filter(|a| !a.trim().is_empty()),
            mutates_workspace: n.mutates_workspace.unwrap_or(true),
            gate: n.gate.clone().filter(|g| !g.trim().is_empty()),
            pending_run: None,
            attempts: 0,
            evidence: Vec::new(),
            blocked_reason: None,
            created_at: now.clone(),
            updated_at: now.clone(),
        });
    }

    tasks.extend(added.clone());
    if let Some(cycle) = find_cycle(&tasks) {
        return Err(format!(
            "those dependencies form a cycle ({cycle}) — nothing in it could ever start"
        ));
    }
    save(goal_id, &tasks)?;
    Ok(added)
}

/// Whether a whole list is a usable plan — what a hand-edited list is checked
/// against before it replaces the one a goal is living by.
///
/// The tools cannot produce a bad list (they validate as they go), but a person
/// or a client PUTting the whole thing can: duplicate ids, an empty title, a
/// dependency on a row they just deleted, a cycle. All of those would either
/// wedge the goal or make it silently skip work, so they are refused here rather
/// than discovered on tick 40.
pub fn validate(tasks: &[Task]) -> Result<(), String> {
    if tasks.len() > MAX_TASKS {
        return Err(format!("{} tasks; a goal holds at most {MAX_TASKS}", tasks.len()));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(tasks.len());
    for (i, t) in tasks.iter().enumerate() {
        if t.id.trim().is_empty() {
            return Err(format!("tasks[{i}] has no id"));
        }
        if t.title.trim().is_empty() {
            return Err(format!("task '{}' has no title", t.id));
        }
        if seen.contains(&t.id.as_str()) {
            return Err(format!("'{}' appears twice", t.id));
        }
        seen.push(&t.id);
    }
    for t in tasks {
        for d in &t.deps {
            if d == &t.id {
                return Err(format!("'{}' depends on itself", t.id));
            }
            if !tasks.iter().any(|o| &o.id == d) {
                return Err(format!("'{}' depends on '{d}', which is not in the list", t.id));
            }
        }
    }
    if let Some(cycle) = find_cycle(tasks) {
        return Err(format!("those dependencies form a cycle ({cycle})"));
    }
    Ok(())
}

/// The first dependency cycle, rendered as `t1 → t2 → t1`, or `None`.
fn find_cycle(tasks: &[Task]) -> Option<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Unseen,
        InProgress,
        Done,
    }
    let mut marks: Vec<(String, Mark)> =
        tasks.iter().map(|t| (t.id.clone(), Mark::Unseen)).collect();

    fn visit(
        id: &str,
        tasks: &[Task],
        marks: &mut Vec<(String, Mark)>,
        stack: &mut Vec<String>,
    ) -> Option<String> {
        // A dep naming a row that is not in the list cannot be part of a cycle.
        let slot = marks.iter().position(|(m, _)| m == id)?;
        match marks[slot].1 {
            Mark::Done => return None,
            Mark::InProgress => {
                let from = stack.iter().position(|s| s == id).unwrap_or(0);
                let mut path: Vec<String> = stack[from..].to_vec();
                path.push(id.to_string());
                return Some(path.join(" → "));
            }
            Mark::Unseen => {}
        }
        marks[slot].1 = Mark::InProgress;
        stack.push(id.to_string());
        if let Some(task) = tasks.iter().find(|t| t.id == id) {
            for d in &task.deps {
                if let Some(found) = visit(d, tasks, marks, stack) {
                    return Some(found);
                }
            }
        }
        stack.pop();
        marks[slot].1 = Mark::Done;
        None
    }

    for t in tasks {
        let mut stack = Vec::new();
        if let Some(found) = visit(&t.id, tasks, &mut marks, &mut stack) {
            return Some(found);
        }
    }
    None
}

/// Apply a change to one task. Every field is optional; absent means unchanged.
#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub detail: Option<String>,
    pub deps: Option<Vec<String>>,
    pub assignee: Option<Option<String>>,
    pub mutates_workspace: Option<bool>,
    pub gate: Option<Option<String>>,
    pub status: Option<TaskStatus>,
    pub blocked_reason: Option<Option<String>>,
    pub pending_run: Option<Option<PendingRun>>,
    pub bump_attempts: bool,
}

pub fn update(goal_id: &str, id: &str, patch: TaskPatch) -> Result<Task, String> {
    let mut tasks = list(goal_id);
    let slot = tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| format!("no task '{id}'"))?;

    if let Some(deps) = &patch.deps {
        for d in deps {
            if d == id {
                return Err(format!("task '{id}' cannot depend on itself"));
            }
            if !tasks.iter().any(|t| &t.id == d) {
                return Err(format!("'{d}' is not a task"));
            }
        }
    }

    {
        let t = &mut tasks[slot];
        if let Some(v) = patch.title {
            let v = v.trim().to_string();
            if v.is_empty() {
                return Err("a task needs a title".into());
            }
            t.title = v;
        }
        if let Some(v) = patch.detail {
            t.detail = v.trim().to_string();
        }
        if let Some(v) = patch.deps {
            t.deps = v;
        }
        if let Some(v) = patch.assignee {
            t.assignee = v.filter(|a| !a.trim().is_empty());
        }
        if let Some(v) = patch.mutates_workspace {
            t.mutates_workspace = v;
        }
        if let Some(v) = patch.gate {
            t.gate = v.filter(|g| !g.trim().is_empty());
        }
        if let Some(v) = patch.status {
            t.status = v;
        }
        if let Some(v) = patch.blocked_reason {
            t.blocked_reason = v;
        }
        if let Some(v) = patch.pending_run {
            t.pending_run = v;
        }
        if patch.bump_attempts {
            t.attempts += 1;
        }
        t.updated_at = chrono::Utc::now().to_rfc3339();
    }

    if let Some(cycle) = find_cycle(&tasks) {
        return Err(format!("that would make a cycle ({cycle})"));
    }
    let updated = tasks[slot].clone();
    save(goal_id, &tasks)?;
    Ok(updated)
}

/// Finish a task, with proof.
///
/// Evidence is required rather than encouraged. "Verify before you claim" was a
/// rule in the tick frame, and a rule in a frame is something a model on tick 40
/// drops; a parameter it cannot omit is not. A task whose only honest proof is
/// prose can still pass [`EvidenceKind::Note`] — the point is that it had to say
/// so on the record.
pub fn complete(goal_id: &str, id: &str, evidence: Evidence) -> Result<Task, String> {
    let mut tasks = list(goal_id);
    let slot = tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| format!("no task '{id}'"))?;
    if evidence.value.trim().is_empty() {
        return Err("evidence needs a value — a commit sha, a run id, a finding id, a path".into());
    }
    if tasks[slot].gate.is_some() && !gate_is_green(&tasks[slot]) {
        return Err(format!(
            "task '{id}' has a gate (`{}`) that has not passed. Run it, record the run with \
             goal_await_run, and complete this task once it exits 0.",
            tasks[slot].gate.clone().unwrap_or_default()
        ));
    }
    let t = &mut tasks[slot];
    t.status = TaskStatus::Done;
    t.blocked_reason = None;
    t.pending_run = None;
    t.evidence.push(evidence);
    t.updated_at = chrono::Utc::now().to_rfc3339();
    let done = t.clone();
    save(goal_id, &tasks)?;
    Ok(done)
}

/// Whether a gated task has a green run on record.
///
/// Looks for a `run` evidence item mentioning `exit 0`, which is the shape
/// [`crate::goal_tick`] writes when a pending run lands. Deliberately literal:
/// a heuristic that guessed would defeat the point of having a gate.
fn gate_is_green(task: &Task) -> bool {
    task.evidence
        .iter()
        .any(|e| e.kind == EvidenceKind::Run && e.value.contains("exit 0"))
}

/// The task list as the tick sees it, compact enough to inject every time.
///
/// This is the rendered `## Plan`. The model reads a checklist — which it is
/// good at — and never rewrites one, which is where it fails.
pub fn render(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "(no tasks yet)".to_string();
    }
    tasks
        .iter()
        .map(|t| {
            let state = match t.status {
                TaskStatus::Done => "done".to_string(),
                TaskStatus::Dropped => "dropped".to_string(),
                TaskStatus::Blocked => format!(
                    "blocked: {}",
                    t.blocked_reason.as_deref().unwrap_or("no reason recorded")
                ),
                TaskStatus::Waiting => t
                    .pending_run
                    .as_ref()
                    .map(|r| format!("waiting on `{}` (run {})", r.what, r.run_id))
                    .unwrap_or_else(|| "waiting".into()),
                TaskStatus::Todo => {
                    if is_ready(t, tasks) {
                        "ready".to_string()
                    } else {
                        "todo".to_string()
                    }
                }
            };
            let deps = if t.deps.is_empty() {
                String::new()
            } else {
                format!(" (after {})", t.deps.join(", "))
            };
            let who = t
                .assignee
                .as_deref()
                .map(|a| format!(" · delegate: {a}"))
                .unwrap_or_default();
            let gate = t
                .gate
                .as_deref()
                .map(|g| format!(" · gate: `{g}`"))
                .unwrap_or_default();
            let proof = t
                .evidence
                .last()
                .map(|e| format!(" · {:?}: {}", e.kind, e.value).to_lowercase())
                .unwrap_or_default();
            let tries = if t.attempts > 0 {
                format!(" · {} attempt(s)", t.attempts)
            } else {
                String::new()
            };
            format!(
                "- **{}** [{state}] {}{deps}{who}{gate}{proof}{tries}",
                t.id, t.title
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A one-line shape of the list, for the journal and the log.
pub fn summarize(tasks: &[Task]) -> String {
    let p = progress(tasks);
    let ready_now = ready(tasks).len();
    let blocked = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Blocked)
        .count();
    let waiting = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Waiting)
        .count();
    format!(
        "{}/{} done · {ready_now} ready · {waiting} waiting · {blocked} blocked",
        p.done, p.total
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, status: TaskStatus, deps: &[&str]) -> Task {
        Task {
            id: id.into(),
            title: format!("task {id}"),
            detail: String::new(),
            status,
            deps: deps.iter().map(|d| d.to_string()).collect(),
            assignee: None,
            mutates_workspace: true,
            pending_run: None,
            gate: None,
            attempts: 0,
            evidence: Vec::new(),
            blocked_reason: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn a_task_with_no_deps_is_ready() {
        let tasks = vec![t("t1", TaskStatus::Todo, &[])];
        assert!(is_ready(&tasks[0], &tasks));
    }

    #[test]
    fn a_task_waits_for_every_dependency() {
        // The whole point of deps: t3 must not start while either parent is open.
        let tasks = vec![
            t("t1", TaskStatus::Done, &[]),
            t("t2", TaskStatus::Todo, &[]),
            t("t3", TaskStatus::Todo, &["t1", "t2"]),
        ];
        assert!(!is_ready(&tasks[2], &tasks));
        let landed = vec![
            t("t1", TaskStatus::Done, &[]),
            t("t2", TaskStatus::Done, &[]),
            t("t3", TaskStatus::Todo, &["t1", "t2"]),
        ];
        assert!(is_ready(&landed[2], &landed));
    }

    #[test]
    fn a_dropped_dependency_does_not_hold_its_dependents_forever() {
        // Work that will never happen must not block work that still can.
        let tasks = vec![
            t("t1", TaskStatus::Dropped, &[]),
            t("t2", TaskStatus::Todo, &["t1"]),
        ];
        assert!(is_ready(&tasks[1], &tasks));
    }

    #[test]
    fn a_dependency_that_no_longer_exists_does_not_block() {
        let tasks = vec![t("t2", TaskStatus::Todo, &["t_gone"])];
        assert!(is_ready(&tasks[0], &tasks));
    }

    #[test]
    fn everything_independent_is_ready_at_once() {
        // This is the parallelism: three rows, no edges, all startable.
        let tasks = vec![
            t("t1", TaskStatus::Todo, &[]),
            t("t2", TaskStatus::Todo, &[]),
            t("t3", TaskStatus::Todo, &["t1"]),
        ];
        let r = ready(&tasks);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].id, "t1");
        assert_eq!(r[1].id, "t2");
    }

    #[test]
    fn dropped_tasks_leave_the_progress_bar() {
        let tasks = vec![
            t("t1", TaskStatus::Done, &[]),
            t("t2", TaskStatus::Dropped, &[]),
            t("t3", TaskStatus::Todo, &[]),
        ];
        let p = progress(&tasks);
        assert_eq!((p.done, p.total), (1, 2));
    }

    #[test]
    fn a_cycle_is_found_and_named() {
        let tasks = vec![
            t("t1", TaskStatus::Todo, &["t2"]),
            t("t2", TaskStatus::Todo, &["t1"]),
        ];
        let cycle = find_cycle(&tasks).expect("cycle should be found");
        assert!(cycle.contains("t1"), "{cycle}");
        assert!(cycle.contains("t2"), "{cycle}");
    }

    #[test]
    fn a_hand_edited_list_is_checked_before_it_replaces_a_live_plan() {
        // Everything here would either wedge the goal or make it skip work.
        assert!(validate(&[t("t1", TaskStatus::Todo, &[])]).is_ok());
        assert!(
            validate(&[t("t1", TaskStatus::Todo, &[]), t("t1", TaskStatus::Todo, &[])]).is_err(),
            "a duplicate id"
        );
        assert!(
            validate(&[t("t1", TaskStatus::Todo, &["t_gone"])]).is_err(),
            "a dependency on a row that was deleted"
        );
        assert!(
            validate(&[t("t1", TaskStatus::Todo, &["t1"])]).is_err(),
            "a self-dependency"
        );
        let mut untitled = t("t1", TaskStatus::Todo, &[]);
        untitled.title = "  ".into();
        assert!(validate(&[untitled]).is_err(), "an empty title");
    }

    #[test]
    fn a_plain_chain_is_not_a_cycle() {
        let tasks = vec![
            t("t1", TaskStatus::Todo, &[]),
            t("t2", TaskStatus::Todo, &["t1"]),
            t("t3", TaskStatus::Todo, &["t2"]),
        ];
        assert!(find_cycle(&tasks).is_none());
    }

    #[test]
    fn ids_come_from_the_high_water_mark() {
        // t2 was dropped from the list entirely; its id must not be reissued,
        // because the scratchpad may still quote it.
        let tasks = vec![t("t1", TaskStatus::Done, &[]), t("t5", TaskStatus::Todo, &[])];
        assert_eq!(next_id(&tasks), "t6");
    }

    #[test]
    fn a_gate_needs_a_green_run() {
        let mut task = t("t1", TaskStatus::Todo, &[]);
        task.gate = Some("cargo test".into());
        assert!(!gate_is_green(&task));
        task.evidence.push(Evidence::new(EvidenceKind::Run, "r_9 exit 1"));
        assert!(!gate_is_green(&task));
        task.evidence.push(Evidence::new(EvidenceKind::Run, "r_10 exit 0"));
        assert!(gate_is_green(&task));
    }

    #[test]
    fn the_render_says_what_a_tick_needs_to_choose() {
        let mut tasks = vec![
            t("t1", TaskStatus::Done, &[]),
            t("t2", TaskStatus::Todo, &["t1"]),
            t("t3", TaskStatus::Todo, &["t2"]),
        ];
        tasks[0]
            .evidence
            .push(Evidence::new(EvidenceKind::Commit, "a1b2c3d"));
        tasks[1].assignee = Some("coding-agent".into());
        let out = render(&tasks);
        assert!(out.contains("**t1** [done]"), "{out}");
        // t2's dependency has landed, so it reads as ready; t3's has not.
        assert!(out.contains("**t2** [ready]"), "{out}");
        assert!(out.contains("**t3** [todo]"), "{out}");
        assert!(out.contains("delegate: coding-agent"), "{out}");
        assert!(out.contains("a1b2c3d"), "{out}");
    }
}
