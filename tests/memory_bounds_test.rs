//! A synthetic stress harness for the memory the agent holds while it runs.
//!
//! The bug this exists to catch does not fail a normal test. Everything here
//! worked correctly at every size — it just cost an amount that grew with the
//! square of the conversation, so a chat with a thousand steps in it spent
//! hundreds of megabytes recording a few hundred kilobytes of new material. That
//! is invisible to a correctness test and obvious to a shape test, which is what
//! these are: they assert on *growth*, not on values.
//!
//! **The measurement is the allocator, not the disk.** An earlier version of
//! this file weighed the session directory and called that the memory test,
//! which is a proxy and a poor one: it cannot see a transient `Vec<Value>` that
//! is built and dropped without ever being written, and that transient was most
//! of the cost. [`Counting`] wraps the system allocator so the assertions below
//! are about bytes this process actually requested.
//!
//! Deliberately a **single** test function, for the same reason
//! `memory_test.rs` is: `paths::data_dir()` resolves once per process, so the
//! first test to set `METALCRAFT_DATA_DIR` decides where every other test in the
//! binary writes — and a second test's `TempDir` would then be deleted out from
//! under the first one's still-running phase. One test also means the allocator
//! counters have exactly one writer, which is what makes them readable at all.
//!
//! Two conventions worth keeping if this file grows:
//!
//!   * Assert a ratio or a slope, never an absolute byte count. An absolute
//!     number is a test that fails the next time someone adds a field.
//!   * Keep the sizes small enough to run in CI. Quadratic growth is legible at
//!     a few hundred steps; it does not need a million.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use metalcraft::{AgentMessage, AgentState};
use metalcraft_agent::diagnostics::DiagnosticsLogger;
use metalcraft_agent::tools::capped::cap_result;

// ── Counting allocator ──────────────────────────────────────────────────

static TOTAL_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, plus a tally.
///
/// `Relaxed` throughout: these counters are read from the one thread that runs
/// the test, and paying for ordering on every allocation would distort the thing
/// being measured.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            TOTAL_ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            if new_size > layout.size() {
                let grew = new_size - layout.size();
                TOTAL_ALLOCATED.fetch_add(grew, Ordering::Relaxed);
                let live = LIVE.fetch_add(grew, Ordering::Relaxed) + grew;
                PEAK.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        out
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn total_allocated() -> usize {
    TOTAL_ALLOCATED.load(Ordering::Relaxed)
}

/// Arm the peak watermark at the current live total, so the next reading is the
/// peak *of the work in between* rather than of the whole process so far.
fn arm_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// How far live bytes rose above where [`arm_peak`] left them.
fn peak_above_armed() -> usize {
    PEAK.load(Ordering::Relaxed)
        .saturating_sub(LIVE.load(Ordering::Relaxed))
}

// ── Fixtures ────────────────────────────────────────────────────────────

/// Total bytes of every file a logger has written into its session dir.
fn session_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .expect("session dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// One step's worth of new material: an assistant line and a tool exchange.
fn append_a_step(state: &mut AgentState, step: usize) {
    state
        .messages
        .push(AgentMessage::Assistant(format!("step {step}: thinking")));
    state.messages.push(AgentMessage::ToolCall {
        id: format!("call_{step}"),
        call_id: None,
        name: "read_file".into(),
        args: serde_json::json!({ "path": format!("src/file_{step}.rs") }),
    });
    state.messages.push(AgentMessage::ToolResult {
        id: format!("call_{step}"),
        call_id: None,
        name: "read_file".into(),
        // Big enough that re-serializing the whole history would dominate, which
        // is precisely the cost being measured.
        result: "x".repeat(2_000),
    });
}

/// Run a session of `steps` steps, returning what the *logging* cost.
///
/// Only the `log_turn` calls are measured. Appending the messages allocates too,
/// and unavoidably grows with the session — including that would bury the number
/// this is actually about.
struct SessionCost {
    /// Bytes allocated across every `log_turn` call.
    allocated: usize,
    /// The largest transient any single `log_turn` call held at once.
    worst_step_peak: usize,
    /// Bytes the session left on disk.
    on_disk: u64,
}

fn cost_of(steps: usize) -> SessionCost {
    let logger = DiagnosticsLogger::new().expect("logger");
    let mut state = AgentState::new("start".to_string());
    let mut allocated = 0usize;
    let mut worst_step_peak = 0usize;

    for step in 0..steps {
        append_a_step(&mut state, step);

        let before = total_allocated();
        arm_peak();
        logger.log_turn(&state);
        allocated += total_allocated() - before;
        worst_step_peak = worst_step_peak.max(peak_above_armed());
    }

    SessionCost {
        allocated,
        worst_step_peak,
        on_disk: session_bytes(logger.session_dir()),
    }
}

#[test]
fn what_a_long_session_costs_stays_bounded() {
    let root = tempfile::tempdir().expect("tempdir");
    // SAFETY: set before any other thread exists in this test binary, and before
    // the first `data_dir()` call, which caches the value in a `OnceLock`.
    unsafe { std::env::set_var("METALCRAFT_DATA_DIR", root.path()) };

    // ── Logging costs what a step adds, not what the session contains ───
    //
    // Run the same per-step logging over a short session and one four times
    // longer. Linear writing makes the longer one cost about 4×; the quadratic
    // version this replaced cost about 16×, and that gap is the whole test.
    let short = cost_of(50);
    let long = cost_of(200);

    let alloc_ratio = long.allocated as f64 / short.allocated as f64;
    assert!(
        alloc_ratio < 6.0,
        "4× the steps allocated {alloc_ratio:.1}× the bytes ({} → {}). Linear is \
         ~4×; anything near 16× means log_turn went back to serializing the whole \
         message list on every step.",
        short.allocated,
        long.allocated
    );

    let disk_ratio = long.on_disk as f64 / short.on_disk as f64;
    assert!(
        disk_ratio < 6.0,
        "4× the steps wrote {disk_ratio:.1}× the bytes ({} → {})",
        short.on_disk,
        long.on_disk
    );

    // ── The transient peak does not grow with the session ───────────────
    //
    // The sharper claim, and the one the ratios above cannot make: logging step
    // 200 of a session must not hold more at once than logging step 50 did. This
    // is what stops a long turn from ratcheting the process's memory high-water
    // mark up and leaving it there.
    // Measured at the time of writing: 8 498 bytes against 8 504, a difference of
    // six. It is flat because what a step holds is the output buffer and one
    // message being written through it, neither of which knows how long the
    // session is. `2×` is the loosest bound that still fails the moment that
    // stops being true.
    assert!(
        long.worst_step_peak < short.worst_step_peak * 2,
        "the worst single step held {} bytes in a 200-step session against {} in a \
         50-step one — the per-step transient is growing with the history, which \
         is the quadratic bug in its other form",
        long.worst_step_peak,
        short.worst_step_peak
    );

    // ── The delta files still add up to the whole conversation ──────────
    //
    // Bounding the cost is only worth anything if the record survives it: a
    // reader that concatenates `turn_NNN.json` in order has to end up with every
    // message, or this traded an OOM for a diagnostics tool that lies.
    let logger = DiagnosticsLogger::new().expect("logger");
    let mut state = AgentState::new("start".to_string());
    for step in 0..25 {
        append_a_step(&mut state, step);
        logger.log_turn(&state);
    }
    let rebuilt = replay(logger.session_dir());
    assert_eq!(
        rebuilt.len(),
        state.messages.len(),
        "the concatenated deltas lost messages"
    );
    assert_eq!(
        rebuilt.last().unwrap()["result"].as_str().unwrap().len(),
        2_000,
        "the last message did not survive the round trip intact"
    );

    // ── Compaction: a list that got shorter replaces, not extends ───────
    //
    // The one case a delta cannot describe, and the branch most likely to be got
    // wrong, because it is the only one where `first_index` goes backwards. A
    // reader that appended here instead of discarding would show the pre-
    // compaction history twice and the post-compaction context as a continuation
    // of it — a transcript that never happened.
    let logger = DiagnosticsLogger::new().expect("logger");
    let mut state = AgentState::new("start".to_string());
    for step in 0..10 {
        append_a_step(&mut state, step);
        logger.log_turn(&state);
    }
    // What `context::compact_if_needed` does: replace the list wholesale with a
    // summary plus the recent tail.
    state.messages = vec![
        AgentMessage::Assistant("[summary of the first ten steps]".into()),
        AgentMessage::User("carry on".into()),
    ];
    logger.log_turn(&state);
    append_a_step(&mut state, 99);
    logger.log_turn(&state);

    let files = turn_files(logger.session_dir());
    let compaction = std::fs::read_to_string(logger.session_dir().join(&files[10])).unwrap();
    let compaction: serde_json::Value = serde_json::from_str(&compaction).unwrap();
    assert_eq!(
        compaction["rewritten"],
        serde_json::json!(true),
        "a shorter message list must be marked as a rewrite"
    );
    assert_eq!(
        compaction["first_index"], serde_json::json!(0),
        "a rewrite starts the history over"
    );

    let rebuilt = replay(logger.session_dir());
    assert_eq!(
        rebuilt.len(),
        state.messages.len(),
        "replaying across a compaction did not land on the post-compaction context"
    );
    assert_eq!(
        rebuilt[0]["content"], "[summary of the first ten steps]",
        "the replayed history should begin at the summary, not before it"
    );

    // ── A pathological step is capped, and leaves nothing behind ────────
    let limit = metalcraft_agent::resources::max_diagnostic_file_bytes();
    let logger = DiagnosticsLogger::new().expect("logger");
    let mut state = AgentState::new("start".to_string());
    state
        .messages
        .push(AgentMessage::Assistant("y".repeat(limit * 3)));

    arm_peak();
    logger.log_turn(&state);
    let capped_step_peak = peak_above_armed();

    let written = session_bytes(logger.session_dir());
    assert!(
        written <= limit as u64,
        "a step over the ceiling wrote {written} bytes against a {limit}-byte limit"
    );
    // The ceiling has to bound the *heap* as well as the file. Serializing first
    // and measuring afterwards would pass the assertion above and fail this one.
    // A quarter of the ceiling against a message three times the ceiling: the
    // record is streamed from the borrowed message, so its size never reaches the
    // heap at all. Measured at ~8.6 KB, so this has room to spare — but it fails
    // immediately if anyone reintroduces a `Vec<Value>` or a `to_string` here,
    // which is exactly the mistake this caught the first time.
    assert!(
        capped_step_peak < limit / 4,
        "writing an over-limit record held {capped_step_peak} bytes at once against \
         a {}-byte message, which means it was built in full before being measured \
         rather than streamed and abandoned at the ceiling",
        limit * 3
    );
    // The streaming writer works through a temp file; a failed write must not
    // leave one lying in the session dir for a reader to trip over.
    let leftovers: Vec<_> = std::fs::read_dir(logger.session_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".partial"))
        .collect();
    assert!(leftovers.is_empty(), "left partial files behind: {leftovers:?}");

    // ── Two sessions in the same second get their own directories ───────
    //
    // The session dir is named for the second it started, so this used to hand
    // both runs the same directory and let them overwrite each other's turns.
    let a = DiagnosticsLogger::new().expect("logger a");
    let b = DiagnosticsLogger::new().expect("logger b");
    assert_ne!(
        a.session_dir(),
        b.session_dir(),
        "two sessions opened back to back shared a directory"
    );

    // ── No tool result enters a conversation unbounded ──────────────────
    //
    // The size matters more than it looks: a tool result is appended to the
    // agent state, persisted with the chat, and replayed into every later
    // request in the turn — so one unbounded result is carried for the rest of
    // the conversation.
    let limit = metalcraft_agent::resources::max_tool_result_bytes();
    let runaway = serde_json::json!({ "rows": vec!["a very long row"; limit / 4] });
    let raw_bytes = serde_json::to_string(&runaway).unwrap().len();
    assert!(raw_bytes > limit, "the fixture needs to exceed the limit");

    let capped = cap_result("http_api", runaway);
    let capped_bytes = serde_json::to_string(&capped).unwrap().len();
    assert_eq!(capped["truncated"], serde_json::json!(true));
    // The note and preview add their own bytes on top of the elided payload, so
    // the guarantee is a bounded multiple of the limit rather than the limit
    // itself. What matters is that it is a function of the limit and not of the
    // tool's output.
    assert!(
        capped_bytes < limit * 2,
        "a capped result should stay near the {limit}-byte limit, got {capped_bytes} \
         (was {raw_bytes})"
    );

    // A result under the limit is passed through untouched — capping must not
    // cost anything in the ordinary case.
    let ordinary = serde_json::json!({ "ok": true });
    assert_eq!(cap_result("http_api", ordinary.clone()), ordinary);
}

/// The `turn_NNN.json` files of a session, in order.
fn turn_files(dir: &std::path::Path) -> Vec<String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("session dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("turn_"))
        .collect();
    files.sort();
    files
}

/// Rebuild a session's message history the way a reader of the delta files must:
/// extend on an ordinary file, start over on one marked `rewritten`.
fn replay(dir: &std::path::Path) -> Vec<serde_json::Value> {
    let mut rebuilt: Vec<serde_json::Value> = Vec::new();
    for name in turn_files(dir) {
        let raw = std::fs::read_to_string(dir.join(&name)).expect("read turn");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse turn");

        if doc["rewritten"] == serde_json::json!(true) {
            rebuilt.clear();
        }
        assert_eq!(
            doc["first_index"].as_u64().unwrap() as usize,
            rebuilt.len(),
            "{name} does not pick up where the previous file left off"
        );
        rebuilt.extend(doc["messages"].as_array().expect("messages").iter().cloned());
    }
    rebuilt
}
