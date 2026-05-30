//! Step guard for error-spiral and loop detection.
//!
//! Provides a [`build_agent_guard`] function that returns a [`StepGuard`]
//! suitable for use with `Executor::with_step_guard`. The guard inspects
//! `AgentState` after each step and detects:
//!
//! - **Error spirals**: N consecutive tool turns where every result is an error.
//! - **Loop detection**: The same tool call (name + args) repeated within a window.

use metalcraft::{AgentMessage, AgentState, GuardAction, StepEvent, StepGuard};
use crate::diagnostics::DiagnosticsLogger;
use crate::ui;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Configuration for the agent step guard.
#[derive(Clone)]
pub struct GuardConfig {
    /// Stop after this many consecutive all-error tool turns. 0 = disabled.
    pub max_consecutive_errors: usize,
    /// Stop once an ordinary tool has been called identically (same name + args)
    /// this many times in a row. 0 = disabled. Kept generous so the guard isn't
    /// paranoid — a tool legitimately repeating a few times (e.g. `cargo check`
    /// between edits) is fine; only a tool stuck firing the *same* call with
    /// nothing changing trips it.
    pub max_identical_repeats: usize,
    /// Same idea, but for tools flagged as status **polls** (see
    /// [`crate::tools::http_api`]). Polling an async job means repeating the
    /// identical call on purpose, so this is much higher. 0 = unlimited (only
    /// the executor's max-steps backstop bounds it).
    pub max_poll_repeats: usize,
    /// Names of tools that are status polls — exempt from `max_identical_repeats`
    /// and governed by `max_poll_repeats` instead.
    pub poll_tools: HashSet<String>,
    /// Print tool calls and results as they happen.
    pub verbose: bool,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            max_consecutive_errors: 3,
            max_identical_repeats: 4,
            max_poll_repeats: 60,
            poll_tools: HashSet::new(),
            verbose: true,
        }
    }
}

/// Build a step guard that checks for error spirals and loops.
pub fn build_agent_guard(
    config: GuardConfig,
    diagnostics: Option<Arc<DiagnosticsLogger>>,
) -> StepGuard<AgentState> {
    let state_tracker = Arc::new(Mutex::new(GuardTracker::new(config.clone())));

    Arc::new(move |state: &AgentState, _event: &StepEvent| {
        if let Some(ref logger) = diagnostics {
            logger.log_turn(state);
        }
        let mut tracker = state_tracker.lock().unwrap();
        tracker.check(state)
    })
}

struct GuardTracker {
    config: GuardConfig,
    /// Number of messages we've already inspected.
    seen_up_to: usize,
    consecutive_error_turns: usize,
    /// Recent tool calls for loop detection, as (call id, hash) pairs. The id
    /// lets us retract a call's hash if its result turns out to be a denial.
    recent_calls: Vec<(String, u64)>,
}

impl GuardTracker {
    fn new(config: GuardConfig) -> Self {
        Self {
            config,
            seen_up_to: 0,
            consecutive_error_turns: 0,
            recent_calls: Vec::new(),
        }
    }

    fn check(&mut self, state: &AgentState) -> GuardAction {
        // Clamp in case state was reset with fewer messages than we've seen
        if self.seen_up_to > state.messages.len() {
            self.seen_up_to = 0;
            self.consecutive_error_turns = 0;
            self.recent_calls.clear();
        }
        let new_messages = &state.messages[self.seen_up_to..];
        self.seen_up_to = state.messages.len();

        // Collect new tool results and tool calls from this batch
        let mut batch_results: Vec<bool> = Vec::new(); // true = error
        // (call id, hash, tool name)
        let mut new_tool_calls: Vec<(String, u64, String)> = Vec::new();
        // Ids of calls that were denied / interrupted (never executed).
        let mut denied_ids: Vec<String> = Vec::new();

        for msg in new_messages {
            match msg {
                AgentMessage::ToolCall { id, name, args, .. } => {
                    if self.config.verbose {
                        let args_brief = summarize_args(args);
                        eprintln!("  {}({args_brief})", ui::tool(format!("▶ {name}")));
                    }
                    let hash = call_hash(name, args);
                    new_tool_calls.push((id.clone(), hash, name.clone()));
                }
                AgentMessage::ToolResult { id, name, result, .. } => {
                    let is_error = result.starts_with("ERROR:");
                    if is_denial(result) {
                        denied_ids.push(id.clone());
                    }
                    if self.config.verbose {
                        if is_error {
                            eprintln!("  {}: {}", ui::error(format!("✗ {name}")), truncate(result, 120));
                        } else if name == "bash" {
                            // For bash results, parse the JSON and show full stdout/stderr
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
                                let exit_code = parsed.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
                                let stdout = parsed.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                                let stderr = parsed.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                                if exit_code == 0 {
                                    eprintln!("  {}", ui::success(format!("✓ {name} (exit 0)")));
                                } else {
                                    eprintln!("  {}", ui::warning(format!("✗ {name} (exit {exit_code})")));
                                }
                                if !stdout.trim().is_empty() {
                                    eprintln!("{}", ui::dim("── stdout ──"));
                                    eprintln!("{}", stdout.trim());
                                }
                                if !stderr.trim().is_empty() {
                                    eprintln!("{}", ui::dim("── stderr ──"));
                                    eprintln!("{}", stderr.trim());
                                }
                                eprintln!("{}", ui::dim("────────────"));
                            } else {
                                eprintln!("  {} {}", ui::success(format!("✓ {name}")), truncate(result, 200));
                            }
                        } else {
                            eprintln!("  {} {}", ui::success(format!("✓ {name}")), truncate(result, 80));
                        }
                    }
                    // Denied/interrupted calls were never executed — they are a
                    // user action, not an agent error, so they don't count toward
                    // the error spiral.
                    if !is_denial(result) {
                        batch_results.push(is_error);
                    }
                }
                _ => {}
            }
        }

        // A denied/interrupted call was never run, so retrying it is the correct
        // response — not a loop. Drop those calls from this batch and retract any
        // hash already recorded for them in a previous batch.
        if !denied_ids.is_empty() {
            new_tool_calls.retain(|(id, _, _)| !denied_ids.contains(id));
            self.recent_calls.retain(|(id, _)| !denied_ids.contains(id));
        }

        // --- Error spiral detection ---
        if self.config.max_consecutive_errors > 0 && !batch_results.is_empty() {
            let all_errors = batch_results.iter().all(|&is_err| is_err);
            if all_errors {
                self.consecutive_error_turns += 1;
            } else {
                self.consecutive_error_turns = 0;
            }

            if self.consecutive_error_turns >= self.config.max_consecutive_errors {
                return GuardAction::Stop(format!(
                    "Error spiral: {} consecutive turns with all tool calls failing",
                    self.consecutive_error_turns
                ));
            }
        }

        // --- Loop detection ---
        // Detect true loops: the same tool call (name + args) firing over and
        // over with nothing changing in between. We count how many identical
        // calls immediately precede this one and only stop once that streak
        // exceeds the allowed budget — generous for ordinary tools (a tool like
        // `cargo check` legitimately repeats between edits) and far more so for
        // status polls, which are *designed* to repeat while awaiting async work.
        if !new_tool_calls.is_empty() {
            let (_, new_hash, new_name) = &new_tool_calls[0];
            let is_poll = self.config.poll_tools.contains(new_name);
            let limit = if is_poll {
                self.config.max_poll_repeats
            } else {
                self.config.max_identical_repeats
            };

            if limit > 0 {
                // How many of the most recent calls were identical to this one.
                let streak = self
                    .recent_calls
                    .iter()
                    .rev()
                    .take_while(|(_, h)| h == new_hash)
                    .count();
                if streak >= limit {
                    return GuardAction::Stop(format!(
                        "Loop detected: {new_name} called identically {} times in a row",
                        streak + 1
                    ));
                }
            }

            self.recent_calls
                .extend(new_tool_calls.iter().map(|(id, h, _)| (id.clone(), *h)));
            // Keep enough history to measure the largest streak we care about.
            let keep = self
                .config
                .max_identical_repeats
                .max(self.config.max_poll_repeats)
                .max(1);
            if self.recent_calls.len() > keep {
                let drain = self.recent_calls.len() - keep;
                self.recent_calls.drain(..drain);
            }
        }

        GuardAction::Continue
    }
}

/// True when a tool result represents a denied or interrupted call rather than
/// a genuine execution outcome. These are user actions (or guard interruptions),
/// so retrying the identical call afterwards is expected and must not be flagged
/// as a loop, nor counted toward the error spiral.
///
/// Matches the deny reasons produced in `approval.rs` and metalcraft's synthetic
/// "interrupted by user" results.
fn is_denial(result: &str) -> bool {
    result.contains("User denied tool")
        || result.contains("interrupted by user")
        || result.contains("Failed to read approval input")
}

fn call_hash(name: &str, args: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let args_str = serde_json::to_string(args).unwrap_or_default();
    args_str.hash(&mut hasher);
    hasher.finish()
}

fn summarize_args(args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        obj.iter()
            .map(|(k, v)| {
                let val = match v {
                    serde_json::Value::String(s) => truncate(s, 60),
                    other => truncate(&other.to_string(), 60),
                };
                format!("{k}: {val}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        truncate(&args.to_string(), 80)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a valid char boundary at or before `max`
        let end = s.floor_char_boundary(max);
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft::AgentState;

    fn tracker() -> GuardTracker {
        GuardTracker::new(GuardConfig {
            max_consecutive_errors: 3,
            max_identical_repeats: 4,
            max_poll_repeats: 60,
            poll_tools: HashSet::new(),
            verbose: false,
        })
    }

    fn tracker_with_polls(polls: &[&str]) -> GuardTracker {
        GuardTracker::new(GuardConfig {
            max_consecutive_errors: 3,
            max_identical_repeats: 4,
            max_poll_repeats: 60,
            poll_tools: polls.iter().map(|s| s.to_string()).collect(),
            verbose: false,
        })
    }

    fn is_stop(action: &GuardAction) -> bool {
        matches!(action, GuardAction::Stop(_))
    }

    fn tool_call(id: &str) -> AgentMessage {
        AgentMessage::ToolCall {
            id: id.to_string(),
            call_id: None,
            name: "write_file".to_string(),
            args: serde_json::json!({"path": "App.tsx", "content": "x"}),
        }
    }

    /// A named tool call with stable args (so repeats hash identically).
    fn named_call(id: &str, name: &str) -> AgentMessage {
        AgentMessage::ToolCall {
            id: id.to_string(),
            call_id: None,
            name: name.to_string(),
            args: serde_json::json!({"job_id": "abc"}),
        }
    }

    fn named_result(id: &str, name: &str, result: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            id: id.to_string(),
            call_id: None,
            name: name.to_string(),
            result: result.to_string(),
        }
    }

    fn tool_result(id: &str, result: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            id: id.to_string(),
            call_id: None,
            name: "write_file".to_string(),
            result: result.to_string(),
        }
    }

    #[test]
    fn denied_call_then_identical_retry_is_not_a_loop() {
        let mut t = tracker();
        let mut state = AgentState::new("go");

        // 1. Model emits write_file; it is denied at approval time.
        state.messages.push(tool_call("1"));
        assert!(!is_stop(&t.check(&state)));
        state
            .messages
            .push(tool_result("1", "ERROR: User denied tool 'write_file'"));
        assert!(!is_stop(&t.check(&state)));

        // 2. User says "keep going"; model retries the identical call. This must
        //    NOT be flagged as a loop, since the first attempt never executed.
        state.messages.push(tool_call("2"));
        assert!(
            !is_stop(&t.check(&state)),
            "retry after a denial should not trip loop detection"
        );
    }

    #[test]
    fn a_few_identical_calls_are_allowed_but_a_stuck_loop_stops() {
        let mut t = tracker(); // max_identical_repeats = 4
        let mut state = AgentState::new("go");

        // The same executed call a handful of times is tolerated (not paranoid):
        // calls 1..=4 should all be allowed.
        for i in 1..=4 {
            state.messages.push(tool_call(&i.to_string()));
            assert!(
                !is_stop(&t.check(&state)),
                "identical call #{i} within budget should be allowed"
            );
            state
                .messages
                .push(tool_result(&i.to_string(), "wrote App.tsx"));
            assert!(!is_stop(&t.check(&state)));
        }

        // The 5th identical call in a row exceeds the budget — a real stuck loop.
        state.messages.push(tool_call("5"));
        assert!(
            is_stop(&t.check(&state)),
            "an ordinary tool stuck repeating identically should eventually stop"
        );
    }

    #[test]
    fn poll_tool_may_repeat_well_past_the_ordinary_budget() {
        let mut t = tracker_with_polls(&["starflask_get_job"]);
        let mut state = AgentState::new("go");

        // Poll the same job id many times in a row — far more than
        // max_identical_repeats — without tripping the loop guard.
        for i in 0..20 {
            state.messages.push(named_call(&i.to_string(), "starflask_get_job"));
            assert!(
                !is_stop(&t.check(&state)),
                "poll #{i} should be allowed for a poll tool"
            );
            state.messages.push(named_result(
                &i.to_string(),
                "starflask_get_job",
                r#"{"status":200,"data":{"status":"processing"}}"#,
            ));
            assert!(!is_stop(&t.check(&state)));
        }
    }

    #[test]
    fn non_poll_tool_is_not_exempt_even_when_polls_configured() {
        // A tool NOT in the poll set still uses the ordinary (relaxed) budget.
        let mut t = tracker_with_polls(&["starflask_get_job"]);
        let mut state = AgentState::new("go");
        for i in 1..=4 {
            state.messages.push(tool_call(&i.to_string()));
            assert!(!is_stop(&t.check(&state)));
            state.messages.push(tool_result(&i.to_string(), "ok"));
            assert!(!is_stop(&t.check(&state)));
        }
        state.messages.push(tool_call("5"));
        assert!(
            is_stop(&t.check(&state)),
            "an ordinary tool should still trip the loop guard past its budget"
        );
    }

    #[test]
    fn repeated_denials_do_not_trip_the_error_spiral() {
        let mut t = tracker();
        let mut state = AgentState::new("go");

        // Three deny/retry cycles in a row. Denials are user actions, not agent
        // failures, so they must not accumulate toward the error spiral.
        for i in 0..3 {
            state.messages.push(tool_call(&format!("c{i}")));
            assert!(!is_stop(&t.check(&state)));
            state.messages.push(tool_result(
                &format!("c{i}"),
                "ERROR: User denied tool 'write_file'",
            ));
            assert!(
                !is_stop(&t.check(&state)),
                "denials should not count toward the error spiral"
            );
        }
    }
}
