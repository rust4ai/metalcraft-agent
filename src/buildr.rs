//! A small buildr.space client — only the calls a [project](crate::projects) has to
//! make *without* an agent in the loop.
//!
//! Projects do their real work in a buildr.space workspace through the
//! `buildr-space` agent pack's HTTP tools, driven by the model. This module is
//! deliberately not that. It exists for the handful of questions the tick runner
//! has to answer **before** deciding whether to spend a turn at all:
//!
//! * is the workspace still there, and awake?
//! * has the build the last tick started finished?
//! * how many minutes has this account spent?
//!
//! Those are HTTP GETs. Asking a language model to make them — as a whole
//! ReAct turn, with a system prompt and a scratchpad and a summary — is the most
//! expensive way to poll a URL that has ever been devised, and a goal waiting on
//! a cold `cargo build` would do it every thirty minutes. See
//! `docs/projects-plan.md` §3: the cheapest tick is the one that never calls
//! a model.
//!
//! The other half is [`hibernate`], which is not a question but an obligation.
//! buildr bills awake minutes and hibernates on a 10–30 minute idle timer, so a
//! tick that leaves its workspace running bills the whole gap to the goal's
//! owner. The runner calls it rather than trusting a prompt to remember.
//!
//! Auth is `BUILDR_API_KEY` (a `bsk_` PAT) from the key store, or a linked
//! Metalcraft token — buildr accepts `mck_` for a linked account, which is what
//! keeps a per-pod PAT from having to exist.

use serde::Deserialize;

/// Where buildr.space lives. Overridable for a self-hosted or test deployment;
/// the pack's tools hard-code the public host, so this only moves for the
/// out-of-band calls made here.
pub fn base_url() -> String {
    crate::key_store::lookup("BUILDR_BASE_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://buildr.space".to_string())
}

/// The credential, preferring the PAT the pack already documents.
///
/// `None` is not an error here — a pod with no buildr account simply has no
/// workspaces, and every caller in this module treats that as "nothing to say"
/// rather than as a failure. A goal that needs one will find out from the agent
/// tools, which can explain it properly.
pub fn token() -> Option<String> {
    for key in ["BUILDR_API_KEY", "METALCRAFT_TOKEN"] {
        if let Some(v) = crate::key_store::lookup(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Whether this pod can talk to buildr at all.
pub fn configured() -> bool {
    token().is_some()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `queued` | `provisioning` | `ready` | `hibernated` | `failed`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub repo_full_name: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Workspace {
    /// Awake and usable.
    pub fn is_ready(&self) -> bool {
        self.status == "ready"
    }
    /// Asleep, but recoverable with a wake.
    pub fn is_hibernated(&self) -> bool {
        self.status == "hibernated"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub id: String,
    /// `running` | `succeeded` | `failed`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
}

impl Run {
    pub fn finished(&self) -> bool {
        self.status != "running"
    }
    pub fn succeeded(&self) -> bool {
        self.status == "succeeded"
    }
}

/// What the account has spent this period.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Compute {
    #[serde(default)]
    pub compute_used_minutes: Option<i64>,
    /// Doubly optional in the wire format: absent for a signed-out caller, and
    /// `null` for an unmetered plan, which has no remaining minutes because it
    /// has no ceiling. Flattened here to the only distinction a goal acts on.
    #[serde(default)]
    pub compute_remaining_minutes: Option<Option<i64>>,
}

impl Compute {
    /// Minutes left, or `None` when this account is unmetered.
    pub fn remaining(&self) -> Option<i64> {
        self.compute_remaining_minutes.flatten()
    }
    pub fn used(&self) -> u32 {
        self.compute_used_minutes.unwrap_or(0).max(0) as u32
    }
}

/// Anything that stopped a call from answering.
///
/// The distinction that earns its keep is [`Self::Gone`]: a 404 means the
/// workspace was reaped, deleted, or never existed, and the right response is to
/// re-provision — not to retry, and not to block the goal. Everything else is a
/// transient the caller should shrug at, because a goal must survive buildr
/// being briefly unreachable.
#[derive(Debug)]
pub enum Error {
    /// No credential on this pod.
    NotConfigured,
    /// The workspace or run is not there any more.
    Gone,
    Http(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "no BUILDR_API_KEY on this pod"),
            Self::Gone => write!(f, "not found"),
            Self::Http(e) => write!(f, "{e}"),
        }
    }
}

async fn request<T: for<'de> Deserialize<'de>>(
    method: reqwest::Method,
    path: &str,
) -> Result<T, Error> {
    let token = token().ok_or(Error::NotConfigured)?;
    let url = format!("{}{path}", base_url());
    let resp = reqwest::Client::new()
        .request(method, &url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        // Short on purpose: every caller here is a pre-flight check standing
        // between a goal and its work, and one that hangs has cost more than the
        // answer was worth.
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| Error::Http(e.to_string()))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::Gone);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Http(format!("{status}: {}", body.chars().take(300).collect::<String>())));
    }
    resp.json::<T>().await.map_err(|e| Error::Http(e.to_string()))
}

/// One workspace, or [`Error::Gone`] if it is not there any more.
pub async fn get_workspace(id: &str) -> Result<Workspace, Error> {
    request(reqwest::Method::GET, &format!("/api/v1/workspaces/{id}")).await
}

/// One run — the call that lets a tick end owing a build and the next one
/// collect it.
pub async fn get_run(workspace_id: &str, run_id: &str) -> Result<Run, Error> {
    request(
        reqwest::Method::GET,
        &format!("/api/v1/workspaces/{workspace_id}/runs/{run_id}"),
    )
    .await
}

/// Wake a hibernated workspace.
pub async fn wake(id: &str) -> Result<Workspace, Error> {
    request(
        reqwest::Method::POST,
        &format!("/api/v1/workspaces/{id}/wake"),
    )
    .await
}

/// Put a workspace to sleep — the last thing every tick does.
pub async fn hibernate(id: &str) -> Result<Workspace, Error> {
    request(
        reqwest::Method::POST,
        &format!("/api/v1/workspaces/{id}/hibernate"),
    )
    .await
}

/// What this account has spent, for the goal's compute rail.
pub async fn compute() -> Result<Compute, Error> {
    request(reqwest::Method::GET, "/api/v1/billing/plan").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_states_read_the_way_a_tick_asks_about_them() {
        let ready = Workspace {
            id: "ws".into(),
            name: String::new(),
            status: "ready".into(),
            repo_full_name: None,
            branch: None,
            error: None,
        };
        assert!(ready.is_ready() && !ready.is_hibernated());

        let asleep = Workspace {
            status: "hibernated".into(),
            ..ready.clone()
        };
        assert!(asleep.is_hibernated() && !asleep.is_ready());

        // Provisioning is neither: a tick must not treat it as usable, and must
        // not try to wake it either.
        let building = Workspace {
            status: "provisioning".into(),
            ..ready.clone()
        };
        assert!(!building.is_ready() && !building.is_hibernated());
    }

    #[test]
    fn a_run_is_finished_unless_it_is_running() {
        let running = Run {
            id: "r".into(),
            status: "running".into(),
            exit_code: None,
            cmd: None,
            output: None,
        };
        assert!(!running.finished());

        let ok = Run { status: "succeeded".into(), ..running.clone() };
        assert!(ok.finished() && ok.succeeded());

        let bad = Run { status: "failed".into(), ..running.clone() };
        assert!(bad.finished() && !bad.succeeded());
    }

    #[test]
    fn unmetered_and_exhausted_are_different_nulls() {
        // absent entirely — a caller buildr did not recognise
        let unknown = Compute::default();
        assert_eq!(unknown.remaining(), None);
        assert_eq!(unknown.used(), 0);

        // present but null — an unmetered plan, which has no ceiling
        let unmetered = Compute {
            compute_used_minutes: Some(90),
            compute_remaining_minutes: Some(None),
        };
        assert_eq!(unmetered.remaining(), None);
        assert_eq!(unmetered.used(), 90);

        // present with a number — the metered case a rail acts on
        let metered = Compute {
            compute_used_minutes: Some(90),
            compute_remaining_minutes: Some(Some(210)),
        };
        assert_eq!(metered.remaining(), Some(210));
    }

    #[test]
    fn the_base_url_has_no_trailing_slash() {
        // A trailing slash would produce `//api/v1/...`, which buildr's router
        // does not match — a whole class of 404s that reads as "workspace gone".
        assert!(!base_url().ends_with('/'));
    }
}
