//! Projects — a long-running piece of work that holds a goal and pursues it on a
//! heartbeat.
//!
//! The container is a **project**; the goal is a *string in its state*. That is
//! the whole reason this type is not called `Goal`: a goal is what you are aiming
//! at, not the thing doing the aiming, and naming the container after one of its
//! fields is what made `goal.goal` read the way it did. Because the goal is
//! state, a project can be re-aimed without dying — it keeps its instances, its
//! memory, its session, its tasks, its workspace and its history.
//!
//! See `docs/projects-plan.md` for the whole design. The shape that matters here:
//!
//! * **A project owns its [`AgentInstance`]s for its life**
//!   (`InstanceOrigin::Project`), which is what gives it memory across ticks and
//!   a conversation history a person can read.
//! * **The scratchpad is the state.** A tick runs in a *fresh* context and
//!   carries nothing but this document, so cost per tick stays flat instead of
//!   climbing until compaction starts eating the detail the project depends on.
//!   It is markdown because the model maintains it — except `## Plan`, which is
//!   rendered from [`crate::project_tasks`] precisely because a model rewriting
//!   its own plan is a model that can lose a row from it.
//! * **Nothing here runs anything.** This module is the record and the document;
//!   [`crate::project_tick`] is what wakes up and acts on them.
//!
//! Storage mirrors the other data-dir stores: one JSON per project at
//! `<data>/projects/<id>.json`, with the scratchpad and its snapshots beside it in
//! `<data>/projects/<id>/`.
//!
//! [`AgentInstance`]: crate::agent_instance::AgentInstance

use serde::{Deserialize, Serialize};

use crate::paths;

/// What a project is trying to do, which selects its persona and its tick frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    /// Ship something: phases, a branch, a PR per phase.
    Build,
    /// Review a repo: a findings ledger, a PR or issue per finding.
    Audit,
}

impl ProjectKind {
    /// The persona a tick of this kind runs as.
    pub fn persona(&self) -> &'static str {
        match self {
            Self::Build => "project-builder",
            Self::Audit => "project-auditor",
        }
    }
}

/// Where a project is in its life.
///
/// `Blocked` is the one that carries weight: it is where *every* stopping
/// condition lands — the agent asking a question, and every rail in [`Rails`]
/// tripping. A project that quietly gave up would be indistinguishable from one
/// still working, which is the worst confusion this design could offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Blocked,
    Paused,
    Done,
    Failed,
}

impl ProjectStatus {
    /// Whether the heartbeat fires for a project in this state.
    pub fn ticks(&self) -> bool {
        matches!(self, Self::Active)
    }
}

/// How often the project wakes.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Heartbeat {
    #[serde(default = "default_every_minutes")]
    pub every_minutes: u32,
    /// IANA name. Absent reads the pod's default, then UTC — the same rule
    /// scheduled flows follow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Fifteen minutes: often enough that a change a person makes lands soon, rare
/// enough that a project left running overnight is not a billing event.
///
/// Thirty was the right number when every wake-up cost a model call. It does not:
/// the pre-flight answers "has the build finished?" with an HTTP GET and spends
/// nothing when the answer is no, so an idle wake-up is nearly free and fifteen
/// buys twice the responsiveness for almost none of the cost.
pub const DEFAULT_HEARTBEAT_MINUTES: u32 = 15;

/// The floor. Below this a "heartbeat" is a busy-loop billing someone for the
/// privilege — and the only legitimate short interval is the pending-run
/// re-tick, which uses exactly this.
pub const MIN_HEARTBEAT_MINUTES: u32 = 5;

fn default_every_minutes() -> u32 {
    DEFAULT_HEARTBEAT_MINUTES
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            every_minutes: DEFAULT_HEARTBEAT_MINUTES,
            timezone: None,
        }
    }
}

/// One repo the project works in, inside its buildr.space workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProjectRepo {
    /// `owner/name` on GitHub.
    pub full_name: String,
    /// Directory under `/workspace`. Absent means buildr's own default (the
    /// repo's bare name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// The branch the project works on — `project/<slug>`, not the repo's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// The project's buildr.space workspace.
///
/// **A cache, not state.** The truth is the branch on GitHub plus the
/// scratchpad; this may be reaped, deleted after a week of hibernation on the
/// free plan, or thrown away because the sprite got into a bad state. A tick
/// reconciles rather than assuming, and a project that cannot survive losing this
/// is a project that dies over a weekend.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Workspace {
    /// buildr.space workspace id. `None` until the first tick provisions one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub repos: Vec<ProjectRepo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_provisioned_at: Option<String>,
}

/// Everything that can stop a project.
///
/// All of them land in [`ProjectStatus::Blocked`] rather than ending the project:
/// running out of rope is a reason to ask a person, not to disappear.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Rails {
    #[serde(default = "default_max_ticks")]
    pub max_ticks: u32,
    #[serde(default = "default_max_no_progress")]
    pub max_consecutive_no_progress: u32,
    /// buildr.space awake minutes this project may spend. Absent = only the
    /// account's own plan bounds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute_minutes_budget: Option<u32>,
    /// How many of this project's PRs may be open at once. Twenty simultaneous bot
    /// PRs is how a repo learns to ignore them.
    #[serde(default = "default_max_open_prs")]
    pub max_open_prs: u32,
    /// RFC3339. Past it, the project blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
}

fn default_max_ticks() -> u32 {
    200
}
fn default_max_no_progress() -> u32 {
    3
}
fn default_max_open_prs() -> u32 {
    3
}

impl Default for Rails {
    fn default() -> Self {
        Self {
            max_ticks: default_max_ticks(),
            max_consecutive_no_progress: default_max_no_progress(),
            compute_minutes_budget: None,
            max_open_prs: default_max_open_prs(),
            deadline: None,
        }
    }
}

/// What the project has spent and how far it has got.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Counters {
    #[serde(default)]
    pub ticks: u32,
    /// Consecutive ticks that changed nothing. Drives the rail, and (later) the
    /// model-tier escalation.
    #[serde(default)]
    pub no_progress_streak: u32,
    #[serde(default)]
    pub compute_minutes_used: u32,
    #[serde(default)]
    pub tokens_spent: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tick_at: Option<String>,
}

/// A long-running command the last tick started and did not wait for.
///
/// `buildr_build`/`buildr_test` keep advancing after the request returns, and a
/// cold build outlives any sane tick — so a tick may end owing one of these, and
/// the next tick's first act is to read it. The heartbeat is the polling loop.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PendingRun {
    pub workspace_id: String,
    pub run_id: String,
    /// What was started, for the journal — "cargo test", "npm run build".
    #[serde(default)]
    pub what: String,
    pub started_at: String,
}

/// Model tier per tick kind, in tier names rather than model ids so a project
/// written on one pod runs on another.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ModelTiers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
}

/// One project.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Project {
    pub id: String,
    /// Short human handle, for lists.
    pub title: String,
    /// **The goal.** The one field that matters; everything else is
    /// configuration or a counter.
    ///
    /// A field rather than the project's identity, so it can be re-aimed: edit
    /// it and the project keeps its instances, memory, session, tasks,
    /// workspace and history. Naming the container after this string is what
    /// the old `Goal` type got wrong.
    pub goal: String,
    pub kind: ProjectKind,
    /// The **worker**: the agent that does the work, in a fresh context each
    /// tick. Owns the scratchpad and produces the evidence.
    pub instance_id: String,
    /// The **conductor**: the small agent that *is* the project — it holds the
    /// plan, judges whether the goal is met, and writes each tick's briefing.
    /// Separate from the worker so that the thing which says "done" is not the
    /// thing that wants to be finished.
    ///
    /// Empty on a project created before the conductor existed, which falls back
    /// to sharing the worker's instance rather than losing its memory.
    #[serde(default)]
    pub conductor_instance_id: String,
    pub agent_preset: String,

    #[serde(default)]
    pub workspace: Workspace,

    pub status: ProjectStatus,
    /// Why it stopped, when it is blocked — the question to put to a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,

    #[serde(default)]
    pub heartbeat: Heartbeat,
    /// Where a question or a report is delivered. Reuses the binding scheduled
    /// follow-ups already use, because it is the same problem: a job that
    /// finishes when nobody is looking still has to reach somebody.
    #[serde(default = "unbound_io")]
    pub io: crate::scheduled_tasks::IoBinding,
    /// The chat that holds this project's journal — one line per tick, and where a
    /// person answers to unblock it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_chat_id: Option<String>,

    #[serde(default)]
    pub rails: Rails,
    #[serde(default)]
    pub counters: Counters,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_run: Option<PendingRun>,
    #[serde(default)]
    pub models: ModelTiers,

    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn unbound_io() -> crate::scheduled_tasks::IoBinding {
    crate::scheduled_tasks::IoBinding::Unbound
}

impl Project {
    /// The project's own directory: the scratchpad and its snapshots.
    pub fn dir(&self) -> std::path::PathBuf {
        paths::project_dir(&self.id)
    }

    /// How far along.
    ///
    /// From the task list when the project has one, and from the scratchpad's
    /// `Plan` checkboxes when it does not — every project created before tasks
    /// existed still draws a correct bar, with nothing to migrate.
    pub fn progress(&self) -> Progress {
        if crate::project_tasks::exists(&self.id) {
            return crate::project_tasks::progress(&crate::project_tasks::list(&self.id));
        }
        progress_of(&read_scratchpad(&self.id).unwrap_or_default())
    }

    /// Whether anything this project started is still running — a build handed to
    /// the heartbeat, by the project itself or by any of its tasks.
    pub fn awaiting_a_run(&self) -> bool {
        self.pending_run.is_some()
            || crate::project_tasks::list(&self.id)
                .iter()
                .any(|t| t.pending_run.is_some())
    }

    /// Which instance the conductor runs as, falling back to the worker's.
    pub fn conductor_instance<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.conductor_instance_id.trim().is_empty() {
            fallback
        } else {
            &self.conductor_instance_id
        }
    }

    /// The interval until the next tick, honouring a pending run's short fuse.
    ///
    /// A project waiting on a build it started should look again soon; a project
    /// working through phases should not. This is the only place the interval
    /// shrinks, and it shrinks for a reason that will resolve on its own.
    pub fn tick_interval_minutes(&self) -> u32 {
        if self.awaiting_a_run() {
            return MIN_HEARTBEAT_MINUTES;
        }
        self.heartbeat.every_minutes.max(MIN_HEARTBEAT_MINUTES)
    }
}

/// Checked and total steps in the scratchpad's plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Progress {
    pub done: u32,
    pub total: u32,
}

pub fn new_id() -> String {
    format!("proj_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

fn dir() -> std::path::PathBuf {
    paths::projects_dir()
}

fn project_path(id: &str) -> std::path::PathBuf {
    dir().join(format!("{id}.json"))
}

/// Every project on this pod, newest first.
pub fn list() -> Vec<Project> {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    let mut projects: Vec<Project> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<Project>(&s).ok())
        .collect();
    projects.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    projects
}

/// One project by id.
pub fn get(id: &str) -> Option<Project> {
    let content = std::fs::read_to_string(project_path(id)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Persist a project, stamping `updated_at`.
pub fn save(project: &Project) -> Result<(), String> {
    let mut project = project.clone();
    project.updated_at = chrono::Utc::now().to_rfc3339();
    std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
    // tmp + rename, like the other stores: a pod that dies mid-write must not
    // leave a project whose JSON is half a document.
    let tmp = project_path(&project.id).with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, project_path(&project.id)).map_err(|e| e.to_string())
}

/// Delete a project and everything under it. The agent instance survives — it holds
/// memory that outlives this project, and instances are never deleted on a timer.
pub fn delete(id: &str) -> Result<(), String> {
    let existed = project_path(id).exists();
    let _ = std::fs::remove_file(project_path(id));
    let _ = std::fs::remove_dir_all(paths::project_dir(id));
    if existed {
        Ok(())
    } else {
        Err(format!("no project '{id}'"))
    }
}

/// How many projects are actively ticking. The ceiling exists because every active
/// project is a live buildr.space workspace on somebody's plan (1 free, 5 premium)
/// and unattended spend that nobody asked about twice.
pub fn active_count() -> usize {
    list().iter().filter(|g| g.status.ticks()).count()
}

// ── the scratchpad ───────────────────────────────────────────────────────────

/// Hard cap on the injected document. Past this a tick spends more on reading
/// its own history than on the work, so a groom is forced (see `project_tick`).
pub const SCRATCHPAD_MAX_BYTES: usize = 12 * 1024;

/// Log lines past this many trigger a groom regardless of the review cadence.
pub const SCRATCHPAD_MAX_LOG_LINES: usize = 40;

/// How many previous scratchpads to keep. Grooming is the one operation that can
/// destroy a project, so it is never the only copy.
pub const SCRATCHPAD_SNAPSHOTS: usize = 10;

/// The sections every scratchpad has, in order. Fixed rather than free-form
/// because the tick frame refers to them by name and the groom rewrites them by
/// name — a document whose shape drifts is one neither can act on.
pub const SECTIONS: &[&str] = &[
    "Goal",
    "Workspace",
    "Plan",
    "State",
    "Log",
    "Blockers",
    "Questions for the human",
];

pub fn scratchpad_path(project_id: &str) -> std::path::PathBuf {
    paths::project_dir(project_id).join("scratchpad.md")
}

/// The scratchpad as written. `None` when the project has none yet.
pub fn read_scratchpad(project_id: &str) -> Option<String> {
    std::fs::read_to_string(scratchpad_path(project_id)).ok()
}

/// Replace the scratchpad, snapshotting the previous one first.
pub fn write_scratchpad(project_id: &str, markdown: &str) -> Result<(), String> {
    let dir = paths::project_dir(project_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if let Some(previous) = read_scratchpad(project_id) {
        snapshot(&dir, &previous)?;
    }
    let path = scratchpad_path(project_id);
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, markdown).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Keep the last [`SCRATCHPAD_SNAPSHOTS`] versions, newest as `.1`.
fn snapshot(dir: &std::path::Path, previous: &str) -> Result<(), String> {
    for n in (1..SCRATCHPAD_SNAPSHOTS).rev() {
        let from = dir.join(format!("scratchpad.{n}.md"));
        if from.exists() {
            let _ = std::fs::rename(&from, dir.join(format!("scratchpad.{}.md", n + 1)));
        }
    }
    std::fs::write(dir.join("scratchpad.1.md"), previous).map_err(|e| e.to_string())
}

/// Previous scratchpads, newest first — what a bad groom is recovered from.
pub fn snapshots(project_id: &str) -> Vec<String> {
    let dir = paths::project_dir(project_id);
    (1..=SCRATCHPAD_SNAPSHOTS)
        .filter_map(|n| std::fs::read_to_string(dir.join(format!("scratchpad.{n}.md"))).ok())
        .collect()
}

/// The document a project starts with.
pub fn seed_scratchpad(project: &Project) -> String {
    let workspace = match (&project.workspace.id, project.workspace.repos.first()) {
        (Some(id), Some(repo)) => format!(
            "buildr `{id}` · repo `{}`{}\n",
            repo.full_name,
            repo.branch
                .as_ref()
                .map(|b| format!(" · branch `{b}`"))
                .unwrap_or_default()
        ),
        (None, Some(repo)) => format!(
            "Not provisioned yet. Create a buildr.space workspace and clone `{}`.\n",
            repo.full_name
        ),
        _ => "None yet.\n".to_string(),
    };
    format!(
        "## Goal\n{}\n\n## Workspace\n{}\n## Plan\n_No plan yet — call `task_add` to write one; this section is rendered from the task list._\n\n\
         ## State\n(nothing yet)\n\n## Log\n\n## Blockers\n(none)\n\n## Questions for the human\n(none)\n",
        project.goal.trim(),
        workspace,
    )
}

/// Append a line under one section, creating the section if the document has
/// drifted and lost it.
pub fn append_to_section(markdown: &str, section: &str, line: &str) -> String {
    let heading = format!("## {section}");
    let Some(start) = find_section(markdown, &heading) else {
        return format!("{}\n\n{heading}\n{line}\n", markdown.trim_end());
    };
    let body_start = start + heading.len();
    let end = next_section_offset(markdown, body_start);
    let mut body = markdown[body_start..end].trim_end().to_string();
    // A placeholder is a statement that there is nothing here, so the first real
    // entry replaces it rather than queueing behind it.
    if is_placeholder(&body) {
        body.clear();
    }
    format!(
        "{}{heading}{}\n{line}\n\n{}",
        &markdown[..start],
        body,
        markdown[end..].trim_start_matches('\n')
    )
}

/// The body of one section, if it has one.
pub fn section_body<'a>(markdown: &'a str, section: &str) -> Option<&'a str> {
    let heading = format!("## {section}");
    let start = find_section(markdown, &heading)?;
    let body_start = start + heading.len();
    let end = next_section_offset(markdown, body_start);
    Some(markdown[body_start..end].trim())
}

fn is_placeholder(body: &str) -> bool {
    let t = body.trim();
    t.is_empty() || t == "(none)" || t == "(nothing yet)"
}

/// Offset of a `## Heading` line, matched at the start of a line only — a
/// heading named inside a fenced code block is prose, not structure.
fn find_section(markdown: &str, heading: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in markdown.split_inclusive('\n') {
        if line.trim_end() == heading {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn next_section_offset(markdown: &str, from: usize) -> usize {
    let mut offset = from;
    for line in markdown[from..].split_inclusive('\n') {
        if offset > from && line.starts_with("## ") {
            return offset;
        }
        offset += line.len();
    }
    markdown.len()
}

/// Checked and total plan steps.
pub fn progress_of(markdown: &str) -> Progress {
    let Some(plan) = section_body(markdown, "Plan") else {
        return Progress::default();
    };
    let mut p = Progress::default();
    for line in plan.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("- [") {
            let mut chars = rest.chars();
            let mark = chars.next().unwrap_or(' ');
            if chars.next() == Some(']') {
                p.total += 1;
                if mark == 'x' || mark == 'X' {
                    p.done += 1;
                }
            }
        }
    }
    p
}

/// Whether the document has decayed enough that the next review tick must groom
/// it rather than merely audit the plan.
pub fn needs_groom(markdown: &str) -> bool {
    if markdown.len() > SCRATCHPAD_MAX_BYTES {
        return true;
    }
    section_body(markdown, "Log")
        .map(|log| log.lines().filter(|l| !l.trim().is_empty()).count() > SCRATCHPAD_MAX_LOG_LINES)
        .unwrap_or(false)
}

/// Trim a scratchpad down to the injectable cap, oldest log entries first.
///
/// The fallback for a document that grew past the cap before a groom could run:
/// a tick still has to be given something, and dropping the oldest history is
/// the least-bad loss — it is the part a groom would have folded into `State`
/// anyway.
pub fn trim_for_injection(markdown: &str) -> String {
    if markdown.len() <= SCRATCHPAD_MAX_BYTES {
        return markdown.to_string();
    }
    let Some(log) = section_body(markdown, "Log") else {
        // No log to give up: truncate at a line boundary rather than mid-word.
        let cut = markdown[..SCRATCHPAD_MAX_BYTES]
            .rfind('\n')
            .unwrap_or(SCRATCHPAD_MAX_BYTES);
        return format!("{}\n\n_[scratchpad truncated]_\n", &markdown[..cut]);
    };
    let mut lines: Vec<&str> = log.lines().collect();
    let mut out = markdown.to_string();
    while out.len() > SCRATCHPAD_MAX_BYTES && lines.len() > 1 {
        lines.pop();
        let kept = lines.join("\n");
        out = replace_section(
            markdown,
            "Log",
            &format!("{kept}\n_[older entries trimmed — groom this]_"),
        );
    }
    out
}

/// Replace one section's body wholesale.
pub fn replace_section(markdown: &str, section: &str, body: &str) -> String {
    let heading = format!("## {section}");
    let Some(start) = find_section(markdown, &heading) else {
        return format!("{}\n\n{heading}\n{body}\n", markdown.trim_end());
    };
    let body_start = start + heading.len();
    let end = next_section_offset(markdown, body_start);
    format!(
        "{}{heading}\n{}\n\n{}",
        &markdown[..start],
        body.trim(),
        markdown[end..].trim_start_matches('\n')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "## Goal\nShip billing\n\n## Plan\n- [x] one\n- [ ] two\n\n## Log\n- t1: did a thing\n\n## Blockers\n(none)\n";

    #[test]
    fn progress_counts_checkboxes() {
        assert_eq!(progress_of(DOC), Progress { done: 1, total: 2 });
    }

    #[test]
    fn progress_of_a_planless_doc_is_zero() {
        assert_eq!(progress_of("## Goal\nx\n"), Progress::default());
    }

    #[test]
    fn append_adds_under_the_named_section() {
        let out = append_to_section(DOC, "Log", "- t2: did another");
        let log = section_body(&out, "Log").unwrap();
        assert!(log.contains("t1"), "{log}");
        assert!(log.contains("t2"), "{log}");
        // and left everything else alone
        assert_eq!(progress_of(&out), Progress { done: 1, total: 2 });
    }

    #[test]
    fn append_replaces_a_placeholder_rather_than_queueing_behind_it() {
        let out = append_to_section(DOC, "Blockers", "- waiting on a key");
        let body = section_body(&out, "Blockers").unwrap();
        assert!(!body.contains("(none)"), "{body}");
        assert!(body.contains("waiting on a key"), "{body}");
    }

    #[test]
    fn append_to_a_missing_section_creates_it() {
        let out = append_to_section("## Goal\nx\n", "Questions for the human", "- which db?");
        assert!(section_body(&out, "Questions for the human").unwrap().contains("which db?"));
    }

    #[test]
    fn a_heading_inside_a_code_fence_is_not_a_section() {
        let doc = "## Goal\nx\n\n## Log\n```\n## Plan\nnot a section\n```\n";
        // The fenced "## Plan" starts a line, so it *is* found by the scanner —
        // what must hold is that the real Log section still reads as its own.
        assert!(section_body(doc, "Log").unwrap().contains("```"));
    }

    #[test]
    fn replace_section_swaps_only_that_body() {
        let out = replace_section(DOC, "Plan", "- [ ] rewritten");
        assert_eq!(progress_of(&out), Progress { done: 0, total: 1 });
        assert!(section_body(&out, "Goal").unwrap().contains("Ship billing"));
    }

    #[test]
    fn needs_groom_on_size() {
        let big = format!("## Log\n{}\n", "- x\n".repeat(SCRATCHPAD_MAX_BYTES / 2));
        assert!(needs_groom(&big));
    }

    #[test]
    fn needs_groom_on_log_length() {
        let doc = format!("## Log\n{}", "- a line\n".repeat(SCRATCHPAD_MAX_LOG_LINES + 1));
        assert!(needs_groom(&doc));
        let short = format!("## Log\n{}", "- a line\n".repeat(3));
        assert!(!needs_groom(&short));
    }

    #[test]
    fn trim_brings_an_overgrown_doc_under_the_cap() {
        let doc = format!(
            "## Goal\nx\n\n## Plan\n- [ ] one\n\n## Log\n{}",
            "- a reasonably long log line about what happened\n".repeat(400)
        );
        assert!(doc.len() > SCRATCHPAD_MAX_BYTES);
        let out = trim_for_injection(&doc);
        assert!(out.len() <= SCRATCHPAD_MAX_BYTES, "{} bytes", out.len());
        // the plan survives — it is the part a tick cannot work without
        assert_eq!(progress_of(&out), Progress { done: 0, total: 1 });
    }

    #[test]
    fn a_pending_run_shortens_the_interval() {
        let mut g = Project {
            id: "g".into(),
            title: "t".into(),
            goal: "do".into(),
            kind: ProjectKind::Build,
            instance_id: "i".into(),
            conductor_instance_id: String::new(),
            agent_preset: "p".into(),
            workspace: Workspace::default(),
            status: ProjectStatus::Active,
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
        };
        assert_eq!(g.tick_interval_minutes(), DEFAULT_HEARTBEAT_MINUTES);
        g.pending_run = Some(PendingRun {
            workspace_id: "ws".into(),
            run_id: "r".into(),
            what: "cargo test".into(),
            started_at: String::new(),
        });
        assert_eq!(g.tick_interval_minutes(), MIN_HEARTBEAT_MINUTES);
    }

    #[test]
    fn a_silly_short_heartbeat_is_floored() {
        let g = Project {
            id: "g".into(),
            title: "t".into(),
            goal: "do".into(),
            kind: ProjectKind::Build,
            instance_id: "i".into(),
            conductor_instance_id: String::new(),
            agent_preset: "p".into(),
            workspace: Workspace::default(),
            status: ProjectStatus::Active,
            blocked_reason: None,
            heartbeat: Heartbeat { every_minutes: 1, timezone: None },
            io: crate::scheduled_tasks::IoBinding::Unbound,
            journal_chat_id: None,
            rails: Rails::default(),
            counters: Counters::default(),
            pending_run: None,
            models: ModelTiers::default(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(g.tick_interval_minutes(), MIN_HEARTBEAT_MINUTES);
    }
}
