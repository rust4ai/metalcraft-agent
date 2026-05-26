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
use std::sync::{Arc, Mutex};

/// Configuration for the agent step guard.
#[derive(Clone)]
pub struct GuardConfig {
    /// Stop after this many consecutive all-error tool turns. 0 = disabled.
    pub max_consecutive_errors: usize,
    /// Detect repeated identical tool calls within this window of recent calls.
    /// 0 = disabled.
    pub loop_window: usize,
    /// Print tool calls and results as they happen.
    pub verbose: bool,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            max_consecutive_errors: 3,
            loop_window: 5,
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
    /// Hashes of recent tool calls for loop detection.
    recent_calls: Vec<u64>,
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
        let mut new_tool_calls: Vec<u64> = Vec::new();

        for msg in new_messages {
            match msg {
                AgentMessage::ToolCall { name, args, .. } => {
                    if self.config.verbose {
                        let args_brief = summarize_args(args);
                        eprintln!("  {}({args_brief})", ui::tool(format!("▶ {name}")));
                    }
                    let hash = call_hash(name, args);
                    new_tool_calls.push(hash);
                }
                AgentMessage::ToolResult { name, result, .. } => {
                    let is_error = result.starts_with("ERROR:");
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
                    batch_results.push(is_error);
                }
                _ => {}
            }
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
        // Detect true loops: the same tool call appearing consecutively (back-to-back)
        // or the same sequence repeating. A tool like `cargo check` legitimately runs
        // multiple times between edits, so we only flag it when the *most recent* call
        // is identical to the new one (i.e. nothing different happened in between).
        if self.config.loop_window > 0 && !new_tool_calls.is_empty() {
            if let Some(&last) = self.recent_calls.last() {
                if new_tool_calls[0] == last {
                    return GuardAction::Stop(
                        "Loop detected: repeated identical tool call".into(),
                    );
                }
            }

            self.recent_calls.extend(new_tool_calls);
            // Trim to window size
            let window = self.config.loop_window;
            if self.recent_calls.len() > window {
                let drain = self.recent_calls.len() - window;
                self.recent_calls.drain(..drain);
            }
        }

        GuardAction::Continue
    }
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
