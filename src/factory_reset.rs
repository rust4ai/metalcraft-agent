//! Returning a pod to the state it booted in the first time.
//!
//! This exists for one job that nothing else does: **re-testing the new-user
//! experience on a real pod**. Onboarding is a sequence of claims about an empty
//! world — *you have no agents*, *no source is bound*, *nothing is installed* —
//! and every one of them stops being testable the moment you use the pod once.
//! Deleting chats one by one does not get it back, because what onboarding reads
//! is not the chat list: it is the absence of `agent_instances/`, of `keys.json`,
//! of the ecosystem-packs marker. So the reset is defined against the data
//! directory rather than against any feature's idea of "my content".
//!
//! ## Why it restarts the process
//!
//! Wiping files under a live process does not wipe the process. The chat store
//! ([`crate::workshop_api`]), the memory bases and registry
//! ([`crate::memory::instance`]), the inbound dedup ring
//! ([`crate::inbound_dedup`]) and the hub-auth owner cache
//! ([`crate::hub_auth`]) are all `OnceLock` process globals holding state
//! rehydrated from those files at first touch. Delete `chats/` alone and the
//! next write re-persists every chat from RAM — the reset visibly undoes itself.
//!
//! We could add a `clear()` to each cache, but that is a list that has to be
//! kept in step with every future cache by everyone who adds one, and the
//! failure mode when someone forgets is silent and looks exactly like this bug.
//! Exiting is the version that cannot rot: the supervisor cold-boots the binary,
//! [`crate::seed::ensure_defaults`] runs on the fresh directory, and every cache
//! is empty because the process is new. That is also, precisely, what a
//! newly-provisioned pod is.
//!
//! The cost is honest and worth naming to the caller: a pod with no supervisor
//! (a local `metalcraft-daemon`, not a managed k3 pod) stays down until someone
//! starts it again. [`ResetReport::restart`] says which world the pod is in as
//! far as it can tell, and the clients surface it before anyone presses the
//! button.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// The phrase a caller has to type back before this runs.
///
/// Not a boolean, because the whole point is that no accidental call — a
/// replayed request, a fat-fingered `curl`, a client bug looping over POST
/// endpoints — can reach it. A constant string is the cheapest thing that a
/// machine cannot produce by mistake and a person cannot produce without
/// having read what it says.
pub const CONFIRM_PHRASE: &str = "FACTORY RESET";

/// How much of the pod to take away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResetScope {
    /// Everything. The data directory is emptied, including `keys.json`, so the
    /// pod comes back with no bound inference source and no service
    /// connections. This is the one that actually replays onboarding from step
    /// zero, and it is the default for that reason.
    #[default]
    Full,
    /// Everything except the key store.
    ///
    /// For the second and tenth run of the same test, where re-binding a source
    /// key each time is friction with nothing to teach. The trade is explicit:
    /// any onboarding step gated on *no key is bound* will not appear, so this
    /// is not the scope to check a first-run flow with.
    KeepKeys,
}

impl ResetScope {
    /// Entries at the root of the data dir this scope preserves.
    fn preserved(self) -> &'static [&'static str] {
        match self {
            ResetScope::Full => &[],
            // Only the secrets themselves. `integrations.json` (which
            // integrations are enabled) and `channels.json` are configuration
            // the reset is *supposed* to take, and keeping them would leave a
            // pod that is neither fresh nor the one you had.
            ResetScope::KeepKeys => &["keys.json"],
        }
    }
}

/// What the pod expects to happen to it after the wipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RestartExpectation {
    /// The process is supervised and should come back on its own.
    Supervised,
    /// Nothing is known to be watching this process. It will exit, and someone
    /// has to start it again.
    Manual,
}

/// A single entry the wipe could not remove.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResetFailure {
    /// Entry name, relative to the data dir.
    pub name: String,
    pub error: String,
}

/// What the reset actually did — reported rather than assumed.
///
/// A partial wipe is a real outcome (a busy file, a read-only mount, a
/// permission the container lost), and it is the outcome where "success" is the
/// most dangerous thing to say: the operator would go on to test onboarding
/// against a pod that still has half its state.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResetReport {
    pub scope: ResetScope,
    /// The data directory that was emptied.
    pub data_dir: String,
    /// Entry names removed, sorted. Directory entries count once.
    pub removed: Vec<String>,
    /// Entry names deliberately left in place by the scope.
    pub kept: Vec<String>,
    /// Entries that could not be removed. Non-empty means the pod is *not*
    /// factory-fresh, whatever the HTTP status said.
    pub failed: Vec<ResetFailure>,
    /// Whether the process expects to be restarted for it, or left down.
    pub restart: RestartExpectation,
}

impl ResetReport {
    /// True when every entry in scope was removed.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Empty this pod's data directory, preserving whatever `scope` spares.
pub fn wipe(scope: ResetScope) -> std::io::Result<ResetReport> {
    wipe_dir(&paths::data_dir(), &paths::upload_root(), scope)
}

/// Empty `data_dir`, preserving whatever `scope` spares.
///
/// Removes the *children* of the directory rather than the directory itself: on
/// a managed pod that path is a mounted volume, and removing a mount point
/// either fails or detaches the storage the restarted process is about to want.
///
/// Best-effort per entry — one unremovable file does not abandon the rest, it
/// lands in [`ResetReport::failed`]. Callers decide what a partial wipe means;
/// this function's job is to be truthful about it.
///
/// Takes its two paths rather than reading them from [`crate::paths`], which is
/// not a style preference: `paths::data_dir()` memoizes in a process-global
/// `OnceLock`, so a test that tried to redirect it with an env var would be
/// racing every other test in the binary for who calls it first — and the
/// consequence of losing that race, for *this* function, is deleting the
/// developer's real agent data. A parameter cannot lose that race.
fn wipe_dir(
    data_dir: &Path,
    upload_root: &Path,
    scope: ResetScope,
) -> std::io::Result<ResetReport> {
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let mut failed = Vec::new();

    // A missing data dir is a pod that is already factory-fresh, not an error.
    let entries = match std::fs::read_dir(data_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ResetReport {
                scope,
                data_dir: data_dir.display().to_string(),
                removed,
                kept,
                failed,
                restart: expected_restart(),
            });
        }
        Err(e) => return Err(e),
    };

    let preserved = scope.preserved();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if preserved.contains(&name.as_str()) {
            kept.push(name);
            continue;
        }
        match remove_any(&entry.path()) {
            Ok(()) => removed.push(name),
            Err(e) => failed.push(ResetFailure {
                name,
                error: e.to_string(),
            }),
        }
    }

    // The upload root is normally `<data>/uploads` and was just taken with
    // everything else. It can be pointed elsewhere with `METALCRAFT_UPLOAD_ROOT`,
    // and an operator-chosen path outside our directory is not ours to delete —
    // a factory reset should never be the thing that empties someone's
    // `/mnt/shared`. Say so instead of doing it quietly.
    if !upload_root.starts_with(data_dir) && upload_root.exists() {
        log::warn!(
            "factory reset: leaving upload root {} alone — it is outside the data dir",
            upload_root.display()
        );
        kept.push(format!("{} (outside data dir)", upload_root.display()));
    }

    removed.sort();
    kept.sort();

    log::warn!(
        "factory reset ({scope:?}): removed {} entr{} from {}{}",
        removed.len(),
        if removed.len() == 1 { "y" } else { "ies" },
        data_dir.display(),
        if failed.is_empty() {
            String::new()
        } else {
            format!(" ({} failed)", failed.len())
        },
    );

    Ok(ResetReport {
        scope,
        data_dir: data_dir.display().to_string(),
        removed,
        kept,
        failed,
        restart: expected_restart(),
    })
}

/// Remove a file, directory, or symlink at `path`.
///
/// `remove_dir_all` follows the entry type, and a symlink *to* a directory must
/// be unlinked rather than walked — otherwise a link in the data dir becomes a
/// path out of it, and the reset deletes whatever it points at.
fn remove_any(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Whether something is likely to restart this process after it exits.
///
/// A guess, and labelled as one in the type: we can tell we are in a container
/// under an orchestrator, which is the case where restart is automatic. Anything
/// we cannot recognise is reported as [`RestartExpectation::Manual`], because
/// the honest failure is telling someone their pod will come back when it will
/// not — not the reverse.
fn expected_restart() -> RestartExpectation {
    // Set by kubelet in every container of a pod; the marker file is the
    // container-runtime fallback for plain Docker.
    let orchestrated = std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
        || Path::new("/var/run/secrets/kubernetes.io").exists();
    if orchestrated {
        RestartExpectation::Supervised
    } else {
        RestartExpectation::Manual
    }
}

/// Re-seed and exit, after giving the caller's response time to reach them.
///
/// The delay is the whole reason this is a separate task: exiting inside the
/// handler drops the connection before the report is written, and the client
/// gets a transport error for an operation that entirely succeeded — which reads
/// as "the reset failed" and invites a second press.
///
/// Seeding before exit is belt-and-braces: boot re-seeds anyway, so this changes
/// nothing on a supervised pod. It matters on an unsupervised one, where the pod
/// may sit wiped for a while before anyone starts it — a data dir holding the
/// default personas and skills is a better thing to leave behind than an empty
/// one, and the operator can inspect it.
pub fn seed_and_exit(delay: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        // Blocking, and deliberately not spawn_blocking: nothing else in this
        // process is going to matter in a moment.
        crate::seed::ensure_defaults();
        log::warn!("factory reset complete — exiting so the pod boots clean");
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A private temp directory per test.
    ///
    /// No env var and no shared state: [`wipe_dir`] takes its path, so these
    /// tests cannot touch the process-wide data dir even if something else in
    /// the binary has already memoized it.
    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mc-reset-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Lay down a plausible used pod: nested dirs and top-level state files.
    fn populate(dir: &Path) {
        std::fs::create_dir_all(dir.join("chats")).unwrap();
        std::fs::write(dir.join("chats/abc.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("memory/instances/i1")).unwrap();
        std::fs::write(dir.join("memory/wal.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(dir.join("agent_instances")).unwrap();
        std::fs::write(dir.join("keys.json"), r#"{"OPENAI_API_KEY":"sk-x"}"#).unwrap();
        std::fs::write(dir.join("channels.json"), "[]").unwrap();
        std::fs::write(dir.join(".metalcraft_packs_seeded"), "").unwrap();
    }

    /// Wipe `dir` with the uploads root inside it, which is the default layout.
    fn wipe_in(dir: &Path, scope: ResetScope) -> ResetReport {
        wipe_dir(dir, &dir.join("uploads"), scope).unwrap()
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn full_scope_empties_everything() {
        let dir = temp_dir("full");
        populate(&dir);

        let report = wipe_in(&dir, ResetScope::Full);

        assert!(
            report.is_clean(),
            "unexpected failures: {:?}",
            report.failed
        );
        assert!(report.kept.is_empty(), "full scope kept {:?}", report.kept);
        assert!(
            entries(&dir).is_empty(),
            "data dir not empty: {:?}",
            entries(&dir)
        );
        // Nested trees, not just top-level files.
        assert!(!dir.join("memory/instances/i1").exists());
    }

    #[test]
    fn keep_keys_spares_only_the_key_store() {
        let dir = temp_dir("keepkeys");
        populate(&dir);

        let report = wipe_in(&dir, ResetScope::KeepKeys);

        assert!(
            report.is_clean(),
            "unexpected failures: {:?}",
            report.failed
        );
        assert_eq!(report.kept, vec!["keys.json".to_string()]);
        assert_eq!(entries(&dir), vec!["keys.json".to_string()]);
        // Config files go even though the secrets stay — the distinction this
        // scope is easiest to get wrong on.
        assert!(!dir.join("channels.json").exists());
    }

    /// The marker is what makes a managed pod re-run its ecosystem-pack seed. If
    /// a reset leaves it behind, the pod comes back without the packs a real new
    /// pod would have — a silently *wrong* new-user experience, which is worse
    /// than none.
    ///
    /// The name is taken from [`paths`] rather than retyped, so renaming the
    /// marker there cannot quietly make this test assert about nothing.
    #[test]
    fn removes_the_ecosystem_packs_marker() {
        let marker = paths::ecosystem_packs_seeded_marker();
        let name = marker.file_name().expect("marker has a file name");
        let dir = temp_dir("marker");
        populate(&dir);
        std::fs::write(dir.join(name), "").unwrap();

        wipe_in(&dir, ResetScope::Full);

        assert!(!dir.join(name).exists());
    }

    /// A symlink in the data dir must be unlinked, never walked — otherwise the
    /// reset reaches outside the directory it is scoped to.
    #[cfg(unix)]
    #[test]
    fn does_not_delete_through_symlinks() {
        let dir = temp_dir("symlink");
        let outside = temp_dir("outside");
        let treasure = outside.join("keep-me.txt");
        std::fs::write(&treasure, "precious").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();

        wipe_in(&dir, ResetScope::Full);

        assert!(!dir.join("linked").exists(), "symlink not removed");
        assert!(treasure.exists(), "reset deleted through a symlink");
        std::fs::remove_dir_all(&outside).ok();
    }

    /// An operator-chosen `METALCRAFT_UPLOAD_ROOT` outside the data dir is not
    /// ours to delete. A factory reset must never be the thing that empties
    /// someone's shared mount.
    #[test]
    fn leaves_an_upload_root_outside_the_data_dir_alone() {
        let dir = temp_dir("uploads-out");
        populate(&dir);
        let uploads = temp_dir("uploads-elsewhere");
        std::fs::write(uploads.join("a.png"), "x").unwrap();

        let report = wipe_dir(&dir, &uploads, ResetScope::Full).unwrap();

        assert!(
            uploads.join("a.png").exists(),
            "reset deleted an external upload root"
        );
        // ...and says so, rather than leaving it a silent exception.
        assert!(
            report.kept.iter().any(|k| k.contains("outside data dir")),
            "kept did not mention the skipped upload root: {:?}",
            report.kept
        );
        std::fs::remove_dir_all(&uploads).ok();
    }

    #[test]
    fn missing_data_dir_is_not_an_error() {
        let dir = temp_dir("missing");
        std::fs::remove_dir_all(&dir).unwrap();

        let report = wipe_in(&dir, ResetScope::Full);

        assert!(report.removed.is_empty());
        assert!(report.is_clean());
    }
}
