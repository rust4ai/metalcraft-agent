//! Persisted **scheduled follow-ups** — deferred subagent jobs the agent arms
//! with the `schedule_followup` tool and the daemon fires when due.
//!
//! A chat turn is a synchronous request/response, so the agent cannot block for
//! minutes waiting to re-check something. Instead it schedules a job here and
//! ends its turn; the daemon poll loop (see [`crate::daemon`]) later runs the
//! job as a subagent and delivers the result back to the originating
//! chat/channel via its [`ReplySink`](crate::tools::ReplySink).
//!
//! State is a JSON array at `<data>/scheduled_tasks.json`, written atomically
//! (tmp + rename) like the other data-dir stores. A process-wide lock serializes
//! the load-modify-save cycle so a turn arming a job and the daemon claiming due
//! jobs can't clobber each other.

use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

/// Longest a follow-up may be deferred. Beyond this the agent should use a flow
/// (cron/interval) instead of a one-shot follow-up.
pub const MAX_DELAY_SECS: i64 = 24 * 60 * 60;
/// Shortest deferral — below this "just do it now" is the right call, and a tiny
/// delay risks firing inside the same poll tick.
pub const MIN_DELAY_SECS: i64 = 10;
/// Cap on pending jobs bound to a single chat, so a misbehaving agent can't
/// flood the store.
pub const MAX_PENDING_PER_CHAT: usize = 20;
/// How many times a follow-up may itself schedule another follow-up. Bounds a
/// task that keeps re-arming itself (e.g. "still not ready, check again").
pub const MAX_RESCHEDULE_DEPTH: u32 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// Where a fired follow-up's reply should be delivered. Captured when the job is
/// armed so the daemon can rebuild the right [`ReplySink`](crate::tools::ReplySink).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IoBinding {
    /// A Workshop chat — deliver by appending to the chat and publishing to its
    /// live event stream.
    WorkshopChat { chat_id: String },
    /// A gateway conversation — deliver out through the bound adapter.
    Gateway { channel_id: String, address: String },
    /// No delivery channel known (e.g. a one-shot CLI run). The result is logged
    /// only.
    Unbound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    /// Originating chat id, if any (mirrors `io_binding` for WorkshopChat).
    #[serde(default)]
    pub chat_id: Option<String>,
    pub io_binding: IoBinding,
    pub run_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// Instruction the subagent runs on wakeup.
    pub task: String,
    /// Persona to run the wakeup as; falls back to the daemon persona if None.
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub tool_set: Option<String>,
    #[serde(default)]
    pub pack: Option<String>,
    pub status: TaskStatus,
    /// Loop guard — incremented when a follow-up schedules another follow-up.
    #[serde(default)]
    pub reschedule_depth: u32,
}

/// Fields a caller supplies to arm a job; the store fills in id/timestamps/status.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub io_binding: IoBinding,
    pub run_at: DateTime<Utc>,
    pub task: String,
    pub persona: Option<String>,
    pub tool_set: Option<String>,
    pub pack: Option<String>,
    pub reschedule_depth: u32,
}

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── pure helpers (unit-tested without disk) ─────────────────────────────────

/// Parse a human delay like `"90s"`, `"3m"`, `"2h"`, or a bare integer (seconds)
/// into a whole-second count. Rejects zero/negative and unknown units.
pub fn parse_delay_secs(raw: &str) -> Result<i64, String> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() {
        return Err("delay is empty".into());
    }
    let (num_part, unit_secs) = match s.chars().last().unwrap() {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        d if d.is_ascii_digit() => (s.as_str(), 1),
        other => return Err(format!("unknown delay unit '{other}' (use s, m, or h)")),
    };
    let n: i64 = num_part
        .trim()
        .parse()
        .map_err(|_| format!("invalid delay number in '{raw}'"))?;
    if n <= 0 {
        return Err("delay must be positive".into());
    }
    Ok(n * unit_secs)
}

/// Resolve a `delay`/`at` pair into an absolute run time, clamped to the allowed
/// window. Exactly one of `delay` or `at` must be given.
pub fn resolve_run_at(
    delay: Option<&str>,
    at: Option<&str>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let run_at = match (delay, at) {
        (Some(d), None) => now + ChronoDuration::seconds(parse_delay_secs(d)?),
        (None, Some(a)) => a
            .parse::<DateTime<Utc>>()
            .map_err(|_| format!("could not parse `at` time '{a}' (use RFC3339, e.g. 2026-07-23T18:04:00Z)"))?,
        (Some(_), Some(_)) => return Err("provide either `delay` or `at`, not both".into()),
        (None, None) => return Err("provide a `delay` (e.g. \"3m\") or an `at` time".into()),
    };
    let secs = (run_at - now).num_seconds();
    if secs < MIN_DELAY_SECS {
        return Err(format!("run time is too soon (min {MIN_DELAY_SECS}s from now)"));
    }
    if secs > MAX_DELAY_SECS {
        return Err(format!(
            "run time is too far out ({}h max — use a scheduled flow for longer)",
            MAX_DELAY_SECS / 3600
        ));
    }
    Ok(run_at)
}

fn new_id() -> String {
    // Time-based, monotonic-ish id without pulling in a uuid dep here. Uniqueness
    // is per-nanosecond, which is ample for user-armed follow-ups.
    format!("sch_{}", Utc::now().timestamp_nanos_opt().unwrap_or(0))
}

// ── persistence ─────────────────────────────────────────────────────────────

fn load_unlocked() -> Vec<ScheduledTask> {
    let path = crate::paths::scheduled_tasks_file();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            log::warn!("scheduled_tasks.json is corrupt ({e}); starting empty");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

fn save_unlocked(tasks: &[ScheduledTask]) -> std::io::Result<()> {
    let path = crate::paths::scheduled_tasks_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(tasks).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// All tasks, newest-scheduled first.
pub fn list() -> Vec<ScheduledTask> {
    let _g = lock().lock().unwrap();
    let mut all = load_unlocked();
    all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    all
}

/// Arm a new follow-up. Validates the per-chat pending cap and reschedule depth.
pub fn add(new: NewTask) -> Result<ScheduledTask, String> {
    if new.reschedule_depth > MAX_RESCHEDULE_DEPTH {
        return Err(format!(
            "follow-up chain too deep (max {MAX_RESCHEDULE_DEPTH} self-reschedules)"
        ));
    }
    let _g = lock().lock().unwrap();
    let mut all = load_unlocked();

    let chat_id = match &new.io_binding {
        IoBinding::WorkshopChat { chat_id } => Some(chat_id.clone()),
        _ => None,
    };
    if let Some(cid) = &chat_id {
        let pending = all
            .iter()
            .filter(|t| t.status == TaskStatus::Pending && t.chat_id.as_deref() == Some(cid))
            .count();
        if pending >= MAX_PENDING_PER_CHAT {
            return Err(format!(
                "too many pending follow-ups for this chat (max {MAX_PENDING_PER_CHAT})"
            ));
        }
        // Dedup an identical pending job (same chat + same task text).
        if all.iter().any(|t| {
            t.status == TaskStatus::Pending
                && t.chat_id.as_deref() == Some(cid)
                && t.task == new.task
        }) {
            return Err("an identical follow-up is already scheduled for this chat".into());
        }
    }

    let task = ScheduledTask {
        id: new_id(),
        chat_id,
        io_binding: new.io_binding,
        run_at: new.run_at,
        created_at: Utc::now(),
        task: new.task,
        persona: new.persona,
        tool_set: new.tool_set,
        pack: new.pack,
        status: TaskStatus::Pending,
        reschedule_depth: new.reschedule_depth,
    };
    all.push(task.clone());
    save_unlocked(&all).map_err(|e| format!("failed to persist scheduled task: {e}"))?;
    Ok(task)
}

/// Cancel a pending job. Returns false if it doesn't exist or already ran.
pub fn cancel(id: &str) -> Result<bool, String> {
    let _g = lock().lock().unwrap();
    let mut all = load_unlocked();
    let Some(t) = all.iter_mut().find(|t| t.id == id) else {
        return Ok(false);
    };
    if t.status != TaskStatus::Pending {
        return Ok(false);
    }
    t.status = TaskStatus::Cancelled;
    save_unlocked(&all).map_err(|e| format!("failed to persist cancel: {e}"))?;
    Ok(true)
}

/// Set a task's status (used by the daemon as a job moves running → done/failed).
pub fn mark(id: &str, status: TaskStatus) {
    let _g = lock().lock().unwrap();
    let mut all = load_unlocked();
    if let Some(t) = all.iter_mut().find(|t| t.id == id) {
        t.status = status;
        if let Err(e) = save_unlocked(&all) {
            log::warn!("failed to persist status for {id}: {e}");
        }
    }
}

/// Atomically claim all pending jobs whose `run_at` has passed: flips them to
/// `Running` and returns them. Marking-while-claiming prevents the next poll
/// tick from firing the same job twice.
pub fn claim_due(now: DateTime<Utc>) -> Vec<ScheduledTask> {
    let _g = lock().lock().unwrap();
    let mut all = load_unlocked();
    let mut due = Vec::new();
    for t in all.iter_mut() {
        if t.status == TaskStatus::Pending && t.run_at <= now {
            t.status = TaskStatus::Running;
            due.push(t.clone());
        }
    }
    if !due.is_empty() {
        if let Err(e) = save_unlocked(&all) {
            log::warn!("failed to persist claimed due tasks: {e}");
        }
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_delay_units() {
        assert_eq!(parse_delay_secs("90s").unwrap(), 90);
        assert_eq!(parse_delay_secs("3m").unwrap(), 180);
        assert_eq!(parse_delay_secs("2h").unwrap(), 7200);
        assert_eq!(parse_delay_secs("45").unwrap(), 45);
        assert_eq!(parse_delay_secs(" 5M ").unwrap(), 300);
    }

    #[test]
    fn parse_delay_rejects_bad() {
        assert!(parse_delay_secs("").is_err());
        assert!(parse_delay_secs("0s").is_err());
        assert!(parse_delay_secs("-3m").is_err());
        assert!(parse_delay_secs("10d").is_err());
        assert!(parse_delay_secs("abc").is_err());
    }

    #[test]
    fn resolve_run_at_window() {
        let now = Utc::now();
        // too soon
        assert!(resolve_run_at(Some("5s"), None, now).is_err());
        // ok
        let r = resolve_run_at(Some("3m"), None, now).unwrap();
        assert_eq!((r - now).num_seconds(), 180);
        // too far
        assert!(resolve_run_at(Some("48h"), None, now).is_err());
        // both / neither
        assert!(resolve_run_at(Some("3m"), Some("x"), now).is_err());
        assert!(resolve_run_at(None, None, now).is_err());
    }

    #[test]
    fn reschedule_depth_capped() {
        let now = Utc::now();
        let over = NewTask {
            io_binding: IoBinding::Unbound,
            run_at: now + ChronoDuration::minutes(3),
            task: "x".into(),
            persona: None,
            tool_set: None,
            pack: None,
            reschedule_depth: MAX_RESCHEDULE_DEPTH + 1,
        };
        assert!(add(over).is_err());
    }
}
