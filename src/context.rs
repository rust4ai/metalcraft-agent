use metalcraft::{AgentMessage, AgentState};
use rig::completion::{Chat, CompletionModel, Message as RigMessage};

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

/// Replace old messages with a summary, keeping recent messages intact.
pub fn compact(state: &mut AgentState, summary: String, keep_recent: usize) {
    if state.messages.len() <= keep_recent {
        return;
    }
    let recent = state.messages.split_off(state.messages.len() - keep_recent);
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

    if state.messages.len() <= config.keep_recent_messages {
        return Ok(false);
    }

    let split = state.messages.len() - config.keep_recent_messages;
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
                let args_short = if args_brief.len() > 200 {
                    format!("{}...", &args_brief[..200])
                } else {
                    args_brief
                };
                transcript.push_str(&format!("Tool call: {}({})\n", name, args_short));
            }
            AgentMessage::ToolResult { name, result, .. } => {
                let result_short = if result.len() > 500 {
                    format!("{}...", &result[..500])
                } else {
                    result.clone()
                };
                transcript.push_str(&format!("Tool result [{}]: {}\n", name, result_short));
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
