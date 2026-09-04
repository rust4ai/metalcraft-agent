//! What this process is holding, and the ceilings that keep it bounded.
//!
//! The agent is a long-lived daemon that keeps conversations, diagnostics, and
//! live event buses in memory for the lifetime of a pod. Everything here exists
//! because none of those had a *number* attached: a pod that died of memory
//! pressure left no record of which of the three had grown, so the first
//! question after a restart — "grown how much, since when?" — had no answer at
//! all.
//!
//! Two halves, deliberately in one module:
//!
//!   * **Limits** — the byte ceilings enforced elsewhere (tool results, request
//!     bodies, diagnostics files). Each reads an env override once and caches
//!     it, so an operator can raise one on a pod without a rebuild.
//!   * **Counters** — process-lifetime totals the metrics endpoint reports.
//!     Plain atomics, never reset, so two samples a minute apart give a rate.
//!
//! Nothing here is on a hot path in a way that matters: the counters are
//! relaxed atomics, and the limits are `OnceLock` reads.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

// ── Limits ──────────────────────────────────────────────────────────────

/// Default ceiling on a single tool result, in bytes of serialized JSON.
///
/// Generous on purpose. This is not a "keep the context small" knob — the tools
/// that produce big output already truncate themselves to sensible sizes, and
/// compaction handles the rest. This is the backstop for the ones that *can't*
/// know: an HTTP tool pointed at a 300 MB export, a pack tool that returns a
/// whole file listing. Those results are useless to a model anyway, and the
/// harm they do is permanent — a tool result enters `AgentState`, is persisted
/// with the chat, and is replayed into every subsequent request in the turn.
const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;

/// Default ceiling on an inbound HTTP request body.
///
/// Above axum's 2 MB default because chats, flows, and pack installs legitimately
/// `PUT` large JSON; far below "whatever fits in RAM", which is what an unset
/// limit means on the pack-install and webhook routes.
const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Default ceiling on any one file the diagnostics logger writes.
///
/// A diagnostics file is written *per executor step*, so its size is multiplied
/// by every step of every turn. The cap is what stops one pathological turn from
/// filling a pod's disk, and — because the writer streams and aborts at the
/// ceiling rather than serializing first — what stops it from building a
/// hundred-megabyte `String` on the way there.
const DEFAULT_MAX_DIAGNOSTIC_FILE_BYTES: usize = 512 * 1024;

/// Ceiling on a single tool result. Override with `MAX_TOOL_RESULT_BYTES`.
pub fn max_tool_result_bytes() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_bytes("MAX_TOOL_RESULT_BYTES", DEFAULT_MAX_TOOL_RESULT_BYTES))
}

/// Ceiling on an inbound request body. Override with `MAX_REQUEST_BODY_BYTES`.
pub fn max_request_body_bytes() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_bytes("MAX_REQUEST_BODY_BYTES", DEFAULT_MAX_REQUEST_BODY_BYTES))
}

/// Ceiling on one diagnostics file. Override with `MAX_DIAGNOSTIC_FILE_BYTES`.
pub fn max_diagnostic_file_bytes() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        env_bytes(
            "MAX_DIAGNOSTIC_FILE_BYTES",
            DEFAULT_MAX_DIAGNOSTIC_FILE_BYTES,
        )
    })
}

/// Read a byte-count env var, falling back to `default`.
///
/// A `0` disables the limit by making it effectively unbounded rather than
/// zero — a literal zero ceiling would truncate every payload to nothing, which
/// is never what an operator typing `0` means.
fn env_bytes(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(0) => usize::MAX,
            Ok(n) => n,
            Err(_) => {
                log::warn!("{name}={raw:?} is not a byte count; using the default {default}");
                default
            }
        },
        Err(_) => default,
    }
}

// ── Counters ────────────────────────────────────────────────────────────

static DIAGNOSTIC_BYTES: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_FILES: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_FILES_TRUNCATED: AtomicU64 = AtomicU64::new(0);
static TOOL_RESULTS_TRUNCATED: AtomicU64 = AtomicU64::new(0);
static TOOL_BYTES_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Record a diagnostics file that was written in full.
pub fn record_diagnostic_write(bytes: u64) {
    DIAGNOSTIC_BYTES.fetch_add(bytes, Ordering::Relaxed);
    DIAGNOSTIC_FILES.fetch_add(1, Ordering::Relaxed);
}

/// Record a diagnostics file that hit [`max_diagnostic_file_bytes`] and was
/// replaced by a stub.
pub fn record_diagnostic_truncated(bytes: u64) {
    DIAGNOSTIC_BYTES.fetch_add(bytes, Ordering::Relaxed);
    DIAGNOSTIC_FILES.fetch_add(1, Ordering::Relaxed);
    DIAGNOSTIC_FILES_TRUNCATED.fetch_add(1, Ordering::Relaxed);
}

/// Record a tool result that exceeded [`max_tool_result_bytes`].
pub fn record_tool_result_truncated(dropped_bytes: u64) {
    TOOL_RESULTS_TRUNCATED.fetch_add(1, Ordering::Relaxed);
    TOOL_BYTES_DROPPED.fetch_add(dropped_bytes, Ordering::Relaxed);
}

/// Totals since process start. Never reset — two samples give a rate.
#[derive(Debug, Clone, Copy, serde::Serialize, utoipa::ToSchema)]
pub struct Counters {
    pub diagnostic_bytes_written: u64,
    pub diagnostic_files_written: u64,
    pub diagnostic_files_truncated: u64,
    pub tool_results_truncated: u64,
    pub tool_bytes_dropped: u64,
}

pub fn counters() -> Counters {
    Counters {
        diagnostic_bytes_written: DIAGNOSTIC_BYTES.load(Ordering::Relaxed),
        diagnostic_files_written: DIAGNOSTIC_FILES.load(Ordering::Relaxed),
        diagnostic_files_truncated: DIAGNOSTIC_FILES_TRUNCATED.load(Ordering::Relaxed),
        tool_results_truncated: TOOL_RESULTS_TRUNCATED.load(Ordering::Relaxed),
        tool_bytes_dropped: TOOL_BYTES_DROPPED.load(Ordering::Relaxed),
    }
}

// ── Process memory ──────────────────────────────────────────────────────

/// Resident set size of this process, in bytes.
///
/// `None` where it can't be read without a dependency or a subprocess. Linux —
/// which is every pod this actually runs on — reads it straight out of
/// `/proc/self/statm`; macOS would need a `mach` FFI call for a number only a
/// developer would look at, so it goes unanswered rather than guessed.
pub fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        // Fields are page counts; the second is the resident set.
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // 4 KiB everywhere this runs. Reading it properly means `sysconf`, which
        // means libc; the constant is right on x86-64 and aarch64 Linux.
        Some(resident_pages * 4096)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_override_means_unbounded_not_empty() {
        // SAFETY: single-threaded test, and the var is read back immediately.
        unsafe { std::env::set_var("MC_TEST_ZERO_LIMIT", "0") };
        assert_eq!(env_bytes("MC_TEST_ZERO_LIMIT", 10), usize::MAX);
        unsafe { std::env::remove_var("MC_TEST_ZERO_LIMIT") };
    }

    #[test]
    fn an_unparseable_override_falls_back() {
        unsafe { std::env::set_var("MC_TEST_BAD_LIMIT", "lots") };
        assert_eq!(env_bytes("MC_TEST_BAD_LIMIT", 10), 10);
        unsafe { std::env::remove_var("MC_TEST_BAD_LIMIT") };
    }

}
