//! The audit archetype's ledger: what a sweep found, and what happened to it.
//!
//! An audit goal's whole failure mode is repetition. It sweeps a repo across
//! many ticks, each one knowing nothing of the others, so without a durable
//! record tick 9 reports what tick 4 already opened a PR for — and a repo that
//! receives the same PR twice learns to ignore the sender.
//!
//! So findings live here rather than in prose. They are **structured** and not
//! part of the scratchpad for two reasons: a client can render them (and a
//! reviewer can see what is outstanding without reading a markdown document),
//! and the dedupe key is a field rather than a phrase somebody has to match by
//! eye. A compact rendering is injected into every audit tick's prompt, which is
//! how the agent sees the ledger without the scratchpad having to carry it.
//!
//! One file per goal at `<data>/goals/<id>/findings.json`, whole-file rewrite:
//! a ledger is tens of entries, not thousands, and an atomic replace cannot
//! leave half a list behind.

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Something is wrong and will bite: a bug, a security hole, data loss.
    High,
    /// Worth fixing: a latent trap, a missing test on a real path.
    Medium,
    /// Tidying. Real, but nobody is harmed by it surviving another month.
    Low,
}

impl Severity {
    /// Ordering for "what to fix next" — highest first.
    pub fn rank(&self) -> u8 {
        match self {
            Self::High => 0,
            Self::Medium => 1,
            Self::Low => 2,
        }
    }
}

/// Where a finding has got to.
///
/// `Rejected` is the state that keeps the ledger honest. Without it a finding
/// somebody decided against would be re-found on the next sweep, forever — the
/// ledger has to remember the noes as well as the yeses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingState {
    Open,
    PrOpen,
    IssueOpen,
    Merged,
    Rejected,
}

impl FindingState {
    /// Whether this finding is still consuming one of the goal's PR slots.
    pub fn holds_a_pr_slot(&self) -> bool {
        matches!(self, Self::PrOpen)
    }
    /// Whether it is still waiting to be acted on.
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Finding {
    /// Short and stable (`f1`, `f2`) — it is quoted in PR bodies and in the
    /// scratchpad, so it has to be typeable.
    pub id: String,
    pub title: String,
    /// `path/to/file.rs:42`, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub severity: Severity,
    #[serde(default)]
    pub detail: String,
    pub state: FindingState,
    /// The PR or issue it turned into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn path(goal_id: &str) -> std::path::PathBuf {
    paths::goal_dir(goal_id).join("findings.json")
}

/// Every finding, highest severity first and oldest first within a severity.
pub fn list(goal_id: &str) -> Vec<Finding> {
    let mut findings: Vec<Finding> = std::fs::read_to_string(path(goal_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    findings.sort_by(|a, b| {
        a.severity
            .rank()
            .cmp(&b.severity.rank())
            .then_with(|| a.created_at.cmp(&b.created_at))
    });
    findings
}

fn save(goal_id: &str, findings: &[Finding]) -> Result<(), String> {
    let dir = paths::goal_dir(goal_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(findings).map_err(|e| e.to_string())?;
    let p = path(goal_id);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// Two findings are the same when they name the same place and say the same
/// thing.
///
/// Deliberately loose on the title (case and punctuation) and exact on the file:
/// a sweep re-run over the same directory phrases things slightly differently
/// each time, and the whole point of the ledger is that "unwrap on a None" and
/// "Unwrap on a None." do not become two PRs.
fn same(a: &Finding, title: &str, file: Option<&str>) -> bool {
    fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }
    a.file.as_deref() == file && normalize(&a.title) == normalize(title)
}

/// Record a finding, or return the existing one it duplicates.
///
/// Duplicates are answered rather than refused: the agent asking to add one has
/// just spent a tick finding it, and telling it "you already know this, it is
/// f3, and f3 is already a PR" is more useful than an error.
pub fn add(
    goal_id: &str,
    title: &str,
    file: Option<&str>,
    severity: Severity,
    detail: &str,
) -> Result<(Finding, bool), String> {
    let mut findings = list(goal_id);
    if let Some(existing) = findings.iter().find(|f| same(f, title, file)) {
        return Ok((existing.clone(), true));
    }
    let now = chrono::Utc::now().to_rfc3339();
    // Ids count from the high-water mark, not from the length: a deleted finding
    // must not hand its id to a different one, because the old id is already
    // quoted in a PR body somewhere.
    let next = findings
        .iter()
        .filter_map(|f| f.id.trim_start_matches('f').parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let finding = Finding {
        id: format!("f{next}"),
        title: title.trim().to_string(),
        file: file.map(str::to_string),
        severity,
        detail: detail.trim().to_string(),
        state: FindingState::Open,
        link: None,
        created_at: now.clone(),
        updated_at: now,
    };
    findings.push(finding.clone());
    save(goal_id, &findings)?;
    Ok((finding, false))
}

/// Move a finding along. `link` is the PR or issue it became.
pub fn set_state(
    goal_id: &str,
    id: &str,
    state: FindingState,
    link: Option<&str>,
) -> Result<Finding, String> {
    let mut findings = list(goal_id);
    let found = findings
        .iter_mut()
        .find(|f| f.id == id)
        .ok_or_else(|| format!("no finding '{id}'"))?;
    found.state = state;
    if let Some(link) = link {
        found.link = Some(link.to_string());
    }
    found.updated_at = chrono::Utc::now().to_rfc3339();
    let updated = found.clone();
    save(goal_id, &findings)?;
    Ok(updated)
}

/// How many of this goal's PRs are open — what `max_open_prs` is counted
/// against.
pub fn open_prs(goal_id: &str) -> usize {
    list(goal_id)
        .iter()
        .filter(|f| f.state.holds_a_pr_slot())
        .count()
}

/// The ledger as the agent sees it, compact enough to inject every tick.
///
/// Everything but `Rejected` detail: a tick needs to know a finding was turned
/// down so it does not raise it again, but not why — that argument is over.
pub fn render(goal_id: &str) -> String {
    let findings = list(goal_id);
    if findings.is_empty() {
        return "(nothing found yet)".to_string();
    }
    findings
        .iter()
        .map(|f| {
            format!(
                "- **{}** [{:?}/{:?}] {}{}{}",
                f.id,
                f.severity,
                f.state,
                f.title,
                f.file
                    .as_deref()
                    .map(|p| format!(" (`{p}`)"))
                    .unwrap_or_default(),
                f.link
                    .as_deref()
                    .map(|l| format!(" → {l}"))
                    .unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(title: &str, file: Option<&str>) -> Finding {
        Finding {
            id: "f1".into(),
            title: title.into(),
            file: file.map(str::to_string),
            severity: Severity::Medium,
            detail: String::new(),
            state: FindingState::Open,
            link: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn the_same_finding_phrased_differently_is_the_same_finding() {
        // The case the ledger exists for: two sweeps over one file, worded
        // slightly differently, must not become two PRs.
        let a = f("Unwrap on a None", Some("src/lib.rs:42"));
        assert!(same(&a, "unwrap on a none.", Some("src/lib.rs:42")));
        assert!(same(&a, "  Unwrap  on a None  ", Some("src/lib.rs:42")));
    }

    #[test]
    fn the_same_words_about_a_different_file_are_a_different_finding() {
        let a = f("Unwrap on a None", Some("src/lib.rs:42"));
        assert!(!same(&a, "Unwrap on a None", Some("src/other.rs:9")));
        assert!(!same(&a, "Unwrap on a None", None));
    }

    #[test]
    fn severity_orders_what_to_fix_next() {
        assert!(Severity::High.rank() < Severity::Medium.rank());
        assert!(Severity::Medium.rank() < Severity::Low.rank());
    }

    #[test]
    fn only_an_open_pr_holds_a_slot() {
        assert!(FindingState::PrOpen.holds_a_pr_slot());
        // A merged PR has stopped costing anyone attention, and an issue is not
        // a PR — neither should keep the goal from opening the next one.
        assert!(!FindingState::Merged.holds_a_pr_slot());
        assert!(!FindingState::IssueOpen.holds_a_pr_slot());
        assert!(!FindingState::Rejected.holds_a_pr_slot());
        assert!(!FindingState::Open.holds_a_pr_slot());
    }
}
