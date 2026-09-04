//! A synthetic stress harness for the memory the agent holds while it runs.
//!
//! The bug this exists to catch does not fail a normal test. Everything here
//! worked correctly at every size — it just cost an amount that grew with the
//! square of the conversation, so a chat with a thousand steps in it spent
//! hundreds of megabytes recording a few hundred kilobytes of new material. That
//! is invisible to a correctness test and obvious to a shape test, which is what
//! these are: they assert on *growth*, not on values.
//!
//! Deliberately a **single** test function, for the same reason
//! `memory_test.rs` is: `paths::data_dir()` resolves once per process, so the
//! first test to set `METALCRAFT_DATA_DIR` decides where every other test in the
//! binary writes — and the second test's `TempDir` would then be deleted out
//! from under the first one's still-running phase.
//!
//! Two conventions worth keeping if this file grows:
//!
//!   * Assert a ratio or a slope, never an absolute byte count. An absolute
//!     number is a test that fails the next time someone adds a field.
//!   * Keep the sizes small enough to run in CI. Quadratic growth is legible at
//!     a few hundred steps; it does not need a million.

use metalcraft::{AgentMessage, AgentState};
use metalcraft_agent::diagnostics::DiagnosticsLogger;
use metalcraft_agent::tools::capped::cap_result;

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

#[test]
fn what_a_long_session_costs_stays_bounded() {
    let root = tempfile::tempdir().expect("tempdir");
    // SAFETY: set before any other thread exists in this test binary, and before
    // the first `data_dir()` call, which caches the value in a `OnceLock`.
    unsafe { std::env::set_var("METALCRAFT_DATA_DIR", root.path()) };

    // ── Diagnostics cost what a session adds, not what it contains ──────
    //
    // Run the same per-step logging over a short session and one four times
    // longer. Linear writing makes the longer one cost about 4×; the quadratic
    // version this replaced cost about 16×, and that gap is the whole test.
    let cost_of = |steps: usize| -> u64 {
        let logger = DiagnosticsLogger::new().expect("logger");
        let mut state = AgentState::new("start".to_string());
        for step in 0..steps {
            append_a_step(&mut state, step);
            logger.log_turn(&state);
        }
        session_bytes(logger.session_dir())
    };

    let short = cost_of(50);
    let long = cost_of(200);
    let ratio = long as f64 / short as f64;
    assert!(
        ratio < 6.0,
        "4× the steps cost {ratio:.1}× the diagnostics bytes ({short} → {long}). \
         Linear would be ~4×; anything near 16× means log_turn went back to \
         writing the whole message list on every step."
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

    let mut files: Vec<_> = std::fs::read_dir(logger.session_dir())
        .expect("session dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("turn_"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 25, "one file per step");

    let mut rebuilt: Vec<serde_json::Value> = Vec::new();
    for name in &files {
        let raw = std::fs::read_to_string(logger.session_dir().join(name)).expect("read turn");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse turn");

        // A rewritten file replaces the history rather than extending it — the
        // contract compaction relies on.
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

    // ── One pathological step does not write a pathological file ────────
    let limit = metalcraft_agent::resources::max_diagnostic_file_bytes();
    let logger = DiagnosticsLogger::new().expect("logger");
    let mut state = AgentState::new("start".to_string());
    state
        .messages
        .push(AgentMessage::Assistant("y".repeat(limit * 3)));
    logger.log_turn(&state);
    let written = session_bytes(logger.session_dir());
    assert!(
        written <= limit as u64,
        "a step over the ceiling wrote {written} bytes against a {limit}-byte limit"
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
