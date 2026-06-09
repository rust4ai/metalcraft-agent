use metalcraft::{AgentMessage, AgentState};
use rig::completion::{Chat, CompletionModel, Message as RigMessage};
use std::borrow::Cow;

/// Configuration for automatic context compaction.
#[derive(Clone)]
pub struct CompactionConfig {
    /// Estimated context window size in tokens.
    pub context_window: usize,
    /// Compact when estimated tokens exceed this fraction of context_window.
    pub compact_threshold: f64,
    /// Number of recent messages to keep intact (never summarized).
    pub keep_recent_messages: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_window: 128_000,
            compact_threshold: 0.6,
            keep_recent_messages: 10,
        }
    }
}

impl CompactionConfig {
    fn threshold_tokens(&self) -> usize {
        (self.context_window as f64 * self.compact_threshold) as usize
    }
}

/// Rough token estimate for an AgentState (~4 chars per token).
pub fn estimate_tokens(state: &AgentState) -> usize {
    state
        .messages
        .iter()
        .map(|m| match m {
            AgentMessage::User(t) | AgentMessage::Assistant(t) => t.len(),
            AgentMessage::ToolCall { name, args, .. } => {
                name.len() + serde_json::to_string(args).unwrap_or_default().len()
            }
            AgentMessage::ToolResult { name, result, .. } => name.len() + result.len(),
        })
        .sum::<usize>()
        / 4
}

/// Truncate `s` to at most `max_chars` characters, appending `...` if it was cut.
/// Slices on char boundaries so multibyte UTF-8 never panics (byte slicing would).
fn truncate_chars(s: &str, max_chars: usize) -> Cow<'_, str> {
    // Byte length is an upper bound on char count, so this is a cheap fast path.
    if s.len() <= max_chars {
        return Cow::Borrowed(s);
    }
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => Cow::Owned(format!("{}...", &s[..byte_idx])),
        None => Cow::Borrowed(s),
    }
}

/// Pick the index at which to split history into "summarize" (before) and "keep
/// recent" (from here on). Starts at `len - keep_recent` but walks the boundary
/// earlier so the kept window never *begins* with a `ToolResult` — a tool result
/// with no preceding tool call ahead of it is an invalid sequence for the
/// provider and would make the very next request fail. Returns 0 if there is
/// nothing to summarize.
fn safe_split(messages: &[AgentMessage], keep_recent: usize) -> usize {
    if messages.len() <= keep_recent {
        return 0;
    }
    let mut split = messages.len() - keep_recent;
    while split > 0 && matches!(messages[split], AgentMessage::ToolResult { .. }) {
        split -= 1;
    }
    split
}

/// Replace old messages with a summary, keeping recent messages intact.
pub fn compact(state: &mut AgentState, summary: String, keep_recent: usize) {
    let split = safe_split(&state.messages, keep_recent);
    if split == 0 {
        return;
    }
    let recent = state.messages.split_off(split);
    state.messages.clear();
    state
        .messages
        .push(AgentMessage::Assistant(format!("[Summary of earlier conversation]: {summary}")));
    state.messages.extend(recent);
}

/// Check if compaction is needed and perform it using the given model.
///
/// Returns `true` if compaction was performed.
pub async fn compact_if_needed<M: CompletionModel + 'static>(
    state: &mut AgentState,
    model: &M,
    config: &CompactionConfig,
) -> Result<bool, String> {
    let tokens = estimate_tokens(state);
    if tokens < config.threshold_tokens() {
        return Ok(false);
    }

    let split = safe_split(&state.messages, config.keep_recent_messages);
    if split == 0 {
        return Ok(false);
    }
    let old_messages = &state.messages[..split];

    let summary = summarize_messages(model, old_messages).await?;

    log::info!(
        "Context compaction: {} tokens -> summarized {} old messages, keeping {} recent",
        tokens,
        split,
        config.keep_recent_messages
    );

    compact(state, summary, config.keep_recent_messages);
    Ok(true)
}

async fn summarize_messages<M: CompletionModel + 'static>(
    model: &M,
    messages: &[AgentMessage],
) -> Result<String, String> {
    let mut transcript = String::new();
    for msg in messages {
        match msg {
            AgentMessage::User(text) => {
                transcript.push_str(&format!("User: {}\n", text));
            }
            AgentMessage::Assistant(text) => {
                transcript.push_str(&format!("Assistant: {}\n", text));
            }
            AgentMessage::ToolCall { name, args, .. } => {
                let args_brief = serde_json::to_string(args).unwrap_or_default();
                transcript.push_str(&format!(
                    "Tool call: {}({})\n",
                    name,
                    truncate_chars(&args_brief, 200)
                ));
            }
            AgentMessage::ToolResult { name, result, .. } => {
                transcript.push_str(&format!(
                    "Tool result [{}]: {}\n",
                    name,
                    truncate_chars(result, 500)
                ));
            }
        }
    }

    let agent = rig::agent::AgentBuilder::new(model.clone())
        .preamble(
            "You are a conversation summarizer. Summarize the following agent conversation \
             transcript concisely. Preserve: key decisions made, files read/written, commands run, \
             important findings, and any errors encountered. Be factual and brief.",
        )
        .build();

    let summary = agent
        .chat(&format!("Summarize this conversation:\n\n{transcript}"), &mut Vec::<RigMessage>::new())
        .await
        .map_err(|e| format!("Compaction LLM call failed: {e}"))?;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(name: &str) -> AgentMessage {
        AgentMessage::ToolCall {
            id: "id".into(),
            call_id: Some("cid".into()),
            name: name.into(),
            args: serde_json::json!({}),
        }
    }

    fn tool_result(name: &str, result: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            id: "id".into(),
            call_id: Some("cid".into()),
            name: name.into(),
            result: result.into(),
        }
    }

    #[test]
    fn truncate_chars_handles_multibyte_without_panicking() {
        // A boundary that falls inside a multibyte char would panic under byte
        // slicing. "é" is two bytes, so byte index 5 lands mid-char.
        let s = "ééééé"; // 5 chars, 10 bytes
        let out = truncate_chars(s, 3);
        assert_eq!(out, "ééé...");

        // Shorter-than-limit strings are returned whole, borrowed.
        assert!(matches!(truncate_chars("hi", 10), Cow::Borrowed("hi")));

        // ASCII truncation appends the ellipsis at the right place.
        assert_eq!(truncate_chars("abcdef", 3), "abc...");
    }

    #[test]
    fn safe_split_does_not_leave_recent_starting_on_tool_result() {
        // Boundary at len-keep_recent would land on a ToolResult; safe_split must
        // walk earlier so the kept window starts on the preceding ToolCall.
        let messages = vec![
            AgentMessage::User("hi".into()),       // 0
            AgentMessage::Assistant("working".into()), // 1
            tool_call("read"),                     // 2
            tool_result("read", "contents"),       // 3  <- naive boundary (keep_recent=2) starts here
            AgentMessage::Assistant("done".into()), // 4
        ];
        // Naive split = 5 - 2 = 3 (a ToolResult). safe_split walks back to 2.
        assert_eq!(safe_split(&messages, 2), 2);
        assert!(!matches!(messages[safe_split(&messages, 2)], AgentMessage::ToolResult { .. }));
    }

    #[test]
    fn compact_keeps_recent_and_prepends_summary() {
        let mut state = AgentState::new("first".to_string());
        state.messages.push(AgentMessage::Assistant("a1".into()));
        state.messages.push(AgentMessage::User("second".into()));
        state.messages.push(AgentMessage::Assistant("a2".into()));
        state.messages.push(AgentMessage::User("third".into()));

        compact(&mut state, "earlier stuff".to_string(), 2);

        // 1 summary + last 2 messages.
        assert_eq!(state.messages.len(), 3);
        assert!(matches!(&state.messages[0], AgentMessage::Assistant(t) if t.starts_with("[Summary of earlier conversation]")));
        assert!(matches!(&state.messages[1], AgentMessage::Assistant(t) if t == "a2"));
        assert!(matches!(&state.messages[2], AgentMessage::User(t) if t == "third"));
    }

    #[test]
    fn compact_is_noop_when_within_keep_recent() {
        let mut state = AgentState::new("only".to_string());
        let before = state.messages.len();
        compact(&mut state, "summary".to_string(), 10);
        assert_eq!(state.messages.len(), before);
    }
}
