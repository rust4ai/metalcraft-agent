//! Context compaction: folding old history into a summary the model reads
//! *instead of* the messages it covers — without deleting them.
//!
//! Compaction here is an **append**, never a rewrite. A pass produces a
//! [`CompactionRecord`] (a summary, plus the journal index where the kept window
//! begins) and the original messages stay exactly where they were. What the
//! model is sent is *derived* on the way into a turn by [`effective_context`]
//! and dropped again on the way out.
//!
//! That is the whole difference from the version this replaced, which assigned
//! `state.messages = vec![summary]`: a summary that came back truncated, wrong,
//! or about the wrong conversation destroyed the transcript it summarized, and
//! there was nothing left to recover it from. Now a bad summary costs one
//! derived view — the conversation is still on disk, and deleting the record
//! puts it back in front of the model.
use metalcraft::{AgentMessage, AgentState};
use rig::completion::{Chat, CompletionModel, Message as RigMessage};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;

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

/// Opens the derived summary message. Also the string an *older* version of this
/// module baked into a real `Assistant` message, so a transcript written back
/// then still reads the way it always did.
const SUMMARY_MARKER: &str = "[Summary of earlier conversation]";

/// How many paths the file ledger prints before it elides the rest.
const FILE_LEDGER_CAP: usize = 20;

/// Tools whose `path` argument names a file the agent *looked at*, and tools
/// whose `path` argument names a file it *changed*. These are the names
/// registered in [`crate::tools`] — a tool renamed there and not here stops
/// contributing to the ledger, which is why the lists sit next to each other.
const READ_TOOLS: [&str; 3] = ["read_file", "grep", "find_files"];
const WRITE_TOOLS: [&str; 2] = ["write_file", "edit_file"];

/// One compaction, recorded rather than applied.
///
/// Every field carries `#[serde(default)]` because these are persisted inside
/// conversations that were written before this type existed: a chat file with no
/// `compaction` key, or a record written by a future version with a field this
/// one has never heard of, must both still load.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    /// The summarizer's own words. Stored raw — the file ledger is re-rendered
    /// from [`Self::files_read`]/[`Self::files_modified`] on every read rather
    /// than baked in here, so a record can never disagree with itself.
    #[serde(default)]
    pub summary: String,
    /// Index into the conversation's message journal where the kept window
    /// starts. Everything before it is what this record's summary covers.
    #[serde(default)]
    pub first_kept_index: usize,
    /// Effective context size when this pass ran, for auditing a compaction that
    /// fired earlier or later than expected.
    #[serde(default)]
    pub tokens_before: usize,
    /// Cumulative read/modified sets — see [`CompactionRecord::rendered_summary`].
    #[serde(default)]
    pub files_read: Vec<String>,
    #[serde(default)]
    pub files_modified: Vec<String>,
    #[serde(default)]
    pub created_at: String,
}

impl CompactionRecord {
    /// The summary as the model reads it: the summarizer's text, then the file
    /// ledger.
    ///
    /// The ledger is appended here, from the record's own sets, rather than
    /// trusted to the summary text, because *which files were touched* is the one
    /// thing summarizing models reliably lose — asked to be brief about a
    /// hundred messages, they keep the narrative and drop the paths, and the next
    /// turn re-reads files that were already read two hours ago. Folding the sets
    /// forward and re-rendering them makes that loss impossible: the paths are
    /// data, not prose.
    pub fn rendered_summary(&self) -> String {
        let mut out = format!("{SUMMARY_MARKER}: {}", self.summary);
        if !self.files_read.is_empty() || !self.files_modified.is_empty() {
            out.push_str("\n\n");
            out.push_str(&render_file_ledger(&self.files_read, &self.files_modified));
        }
        out
    }
}

/// A conversation's compaction records, shared with the turn that may append to
/// them.
///
/// Sync-locked behind an `Arc` for the same reason the chat session's plan and
/// in-flight snapshot are: the turn runs with the state taken out of the
/// session, so the one place that can record a compaction cannot take the
/// session's async lock.
pub type CompactionLog = std::sync::Arc<std::sync::Mutex<Vec<CompactionRecord>>>;

/// Rough token estimate for a message list (~4 chars per token).
pub fn estimate_tokens(messages: &[AgentMessage]) -> usize {
    chars_of(messages) / 4
}

fn chars_of(messages: &[AgentMessage]) -> usize {
    messages
        .iter()
        .map(|m| match m {
            // `User` carries a `UserInput` (text plus any attached images); the
            // text is the part that costs context tokens here.
            AgentMessage::User(t) => t.text.len(),
            AgentMessage::Assistant(t) => t.len(),
            AgentMessage::ToolCall { name, args, .. } => {
                name.len() + serde_json::to_string(args).unwrap_or_default().len()
            }
            AgentMessage::ToolResult { name, result, .. } => name.len() + result.len(),
            // The encrypted reasoning payload is sent back to the provider, so
            // it counts toward context; its length is a rough proxy.
            AgentMessage::Reasoning { encrypted, .. } => encrypted.len(),
        })
        .sum::<usize>()
}

/// Size of the context a turn would actually be sent.
///
/// The threshold has to be measured on this rather than on the journal. The
/// journal only ever grows, so measuring it would leave a compacted conversation
/// permanently over the line — announcing and paying for a summarization call on
/// every single turn, each one folding in one more message.
pub fn effective_tokens(messages: &[AgentMessage], records: &[CompactionRecord]) -> usize {
    match records.last() {
        Some(record) => {
            let kept = &messages[record.first_kept_index.min(messages.len())..];
            (record.rendered_summary().len() + chars_of(kept)) / 4
        }
        None => chars_of(messages) / 4,
    }
}

/// The messages a turn is actually sent: the newest summary, then every journal
/// message from that summary's boundary on.
///
/// The single source of truth for "what does the model see" — the turn path, the
/// token estimate and the next compaction all derive from this one function, so
/// the boundary cannot be interpreted two ways.
///
/// Only the **last** record is consulted, and that is a property of how records
/// are made rather than a shortcut: every pass re-partitions the whole effective
/// sequence, so the newest summary already contains every older one (see
/// [`plan_compaction`]).
pub fn effective_context(
    messages: &[AgentMessage],
    records: &[CompactionRecord],
) -> Vec<AgentMessage> {
    let Some(record) = records.last() else {
        return messages.to_vec();
    };
    // A record can outlive the journal it indexed — a hand-edited chat file, a
    // reset that dropped messages — and an out-of-range slice would panic on the
    // way into a turn. Clamping degrades to "summary only", which is wrong but
    // answerable; a panic is neither.
    let first_kept = record.first_kept_index.min(messages.len());
    let kept = &messages[first_kept..];
    let mut out = Vec::with_capacity(kept.len() + 1);
    out.push(AgentMessage::Assistant(record.rendered_summary()));
    out.extend_from_slice(kept);
    out
}

/// A turn's full history, held aside while the turn runs on the derived context.
///
/// The executor is handed an `AgentState`, and whatever is in `state.messages`
/// is what goes to the provider — so a compacted turn has to *run* on the
/// derived list. This is the other half of that trade: it keeps the journal and
/// splices the turn's new messages back onto it afterwards, so what the caller
/// persists is the whole conversation rather than the narrow view one turn
/// happened to need.
pub struct HeldJournal {
    messages: Vec<AgentMessage>,
    /// Length of the derived context handed to the turn. Everything the state
    /// grew past this point is new and belongs on the end of the journal.
    context_len: usize,
}

impl HeldJournal {
    /// Swap `state.messages` for the context this turn should run on, keeping the
    /// journal.
    ///
    /// `None` when there is nothing to derive: an uncompacted conversation *is*
    /// its own context, and copying the entire history every turn to prove it
    /// would be pure waste.
    pub fn derive(state: &mut AgentState, records: &[CompactionRecord]) -> Option<Self> {
        if records.is_empty() {
            return None;
        }
        let messages = std::mem::take(&mut state.messages);
        state.messages = effective_context(&messages, records);
        Some(Self {
            context_len: state.messages.len(),
            messages,
        })
    }

    /// Put the journal back, with whatever the turn appended on the end.
    pub fn restore(self, state: &mut AgentState) {
        let mut journal = self.messages;
        if state.messages.len() > self.context_len {
            journal.extend(state.messages.drain(self.context_len..));
        }
        state.messages = journal;
    }
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
/// earlier so the kept window never *begins* in the middle of a tool block:
///
/// - a leading `ToolResult` has no preceding tool call ahead of it, and
/// - a leading `ToolCall` would have its paired `Reasoning` item summarized away
///   (reasoning items immediately precede their tool call),
///
/// both of which are invalid sequences for the provider and would make the very
/// next request fail. Walking back stops at a `User`, `Assistant`, or `Reasoning`
/// message — a `Reasoning` start is fine because its whole block (reasoning →
/// tool call → tool result) is then kept together. Returns 0 if there is nothing
/// to summarize.
fn safe_split(messages: &[AgentMessage], keep_recent: usize) -> usize {
    if messages.len() <= keep_recent {
        return 0;
    }
    let mut split = messages.len() - keep_recent;
    // Never start the kept window on a tool result (its call would be gone).
    while split > 0 && matches!(messages[split], AgentMessage::ToolResult { .. }) {
        split -= 1;
    }
    // If the window would start on a tool call, pull in a reasoning item that
    // directly precedes the block (walking past any parallel calls in the same
    // batch). Otherwise the kept tool call loses its paired reasoning item and
    // the provider rejects it. A block with no leading reasoning is left as-is.
    if split > 0 && matches!(messages[split], AgentMessage::ToolCall { .. }) {
        let mut block_start = split;
        while block_start > 0 && matches!(messages[block_start], AgentMessage::ToolCall { .. }) {
            block_start -= 1;
        }
        if matches!(messages[block_start], AgentMessage::Reasoning { .. }) {
            split = block_start;
        }
    }
    split
}

/// Which journal region the next compaction folds up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactionPlan {
    /// Where the summarized region starts: the previous record's boundary, or 0.
    summarize_from: usize,
    /// Where the kept window starts once this pass lands.
    first_kept_index: usize,
}

impl CompactionPlan {
    /// How many journal messages this pass folds up. Zero never occurs — a plan
    /// that would fold nothing is not produced at all.
    fn folded(&self) -> usize {
        self.first_kept_index - self.summarize_from
    }
}

/// Plan the next compaction over the *whole effective sequence*, not just the
/// messages that arrived since the last one.
///
/// This is the partition guarantee, and it is the bug that "summarize the new
/// tail" has: with a previous boundary at 20 and ten new messages, summarizing
/// only messages 30.. would leave 20..30 covered by neither summary — they are
/// already behind the new boundary, so the model never sees them again, and no
/// summary ever mentioned them. Folding the previous summary into the new one
/// closes that hole by construction: the summarized region always starts exactly
/// where the last one ended, and the previous summary travels with it.
///
/// `None` when no journal message would be folded. The previous summary alone is
/// not worth an LLM call: re-summarizing a summary loses detail and buys nothing.
fn plan_compaction(
    messages: &[AgentMessage],
    records: &[CompactionRecord],
    keep_recent: usize,
) -> Option<CompactionPlan> {
    let summarize_from = records
        .last()
        .map(|r| r.first_kept_index.min(messages.len()))
        .unwrap_or(0);
    // Computed on the kept tail rather than on the materialized effective list:
    // the derived summary message occupies exactly one slot at the front, so it
    // shifts every index by one and changes no decision `safe_split` makes.
    let split = safe_split(&messages[summarize_from..], keep_recent);
    (split > 0).then_some(CompactionPlan {
        summarize_from,
        first_kept_index: summarize_from + split,
    })
}

/// Why a compaction is being attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// The context crossed [`CompactionConfig::compact_threshold`] before a turn.
    Threshold,
    /// Someone asked for it — `/compact`. Skips the threshold check, because the
    /// point of asking is that you want it *now*: the automatic rule fires at 60%
    /// of the window, which is long after the point where someone can feel a
    /// conversation getting heavy and wants room before the question that matters.
    Forced,
}

/// Check if compaction is needed and record it using the given model.
///
/// Returns the record that was appended to `records`, or `None` if no compaction
/// was needed. `messages` is read, never written: the caller's history is
/// untouched and the new boundary lives on the record.
///
/// The record is returned rather than only appended because its summary is the
/// most concentrated description of the conversation that exists — an LLM call
/// has already been paid for it — and the memory system captures it on the way
/// past instead of letting it sit unread in a record.
pub async fn compact_if_needed<M: CompletionModel + Clone + 'static>(
    messages: &[AgentMessage],
    model: &M,
    config: &CompactionConfig,
    records: &mut Vec<CompactionRecord>,
) -> Result<Option<CompactionRecord>, String> {
    compact_with(
        messages,
        model,
        config,
        records,
        CompactionTrigger::Threshold,
    )
    .await
}

/// Whether [`compact_if_needed`] would actually pay for a summarization call.
///
/// Exists so a caller can *announce* compaction before it starts: a progress
/// frame that says "compacting" on every turn, because the caller could not tell
/// in advance, teaches people to ignore it. Runs the same predicate the real
/// path runs — an estimate and a plan, no model — rather than a second copy of
/// the rule that could drift away from it.
pub fn needs_compaction(
    messages: &[AgentMessage],
    records: &[CompactionRecord],
    config: &CompactionConfig,
) -> bool {
    should_summarize(
        effective_tokens(messages, records),
        plan_compaction(messages, records, config.keep_recent_messages)
            .map(|p| p.folded())
            .unwrap_or(0),
        config,
        CompactionTrigger::Threshold,
    )
}

/// Compact now, whatever the context size — the primitive behind `/compact`.
///
/// Still returns `None` when there is genuinely nothing to do: a conversation with
/// nothing older than `keep_recent_messages` has no old half to summarize, and
/// saying so beats paying for a summary of nothing.
pub async fn compact_now<M: CompletionModel + Clone + 'static>(
    messages: &[AgentMessage],
    model: &M,
    config: &CompactionConfig,
    records: &mut Vec<CompactionRecord>,
) -> Result<Option<CompactionRecord>, String> {
    compact_with(messages, model, config, records, CompactionTrigger::Forced).await
}

/// Whether an attempt should go on to the summarization call, which costs an LLM
/// round trip — split out so the one asymmetry between the triggers is testable
/// without a model.
///
/// `Forced` skips the size check and nothing else. It still needs an old half to
/// summarize: with none, there is no work to do and no reason to pay for a call.
fn should_summarize(
    tokens: usize,
    folded: usize,
    config: &CompactionConfig,
    trigger: CompactionTrigger,
) -> bool {
    if folded == 0 {
        return false;
    }
    trigger == CompactionTrigger::Forced || tokens >= config.threshold_tokens()
}

async fn compact_with<M: CompletionModel + Clone + 'static>(
    messages: &[AgentMessage],
    model: &M,
    config: &CompactionConfig,
    records: &mut Vec<CompactionRecord>,
    trigger: CompactionTrigger,
) -> Result<Option<CompactionRecord>, String> {
    let tokens = effective_tokens(messages, records);
    let Some(plan) = plan_compaction(messages, records, config.keep_recent_messages) else {
        return Ok(None);
    };
    if !should_summarize(tokens, plan.folded(), config, trigger) {
        return Ok(None);
    }
    let folded = &messages[plan.summarize_from..plan.first_kept_index];
    let previous = records.last();

    let summary = summarize_messages(model, previous.map(|r| r.summary.as_str()), folded).await?;

    // The ledger folds forward: paths named in the region being summarized now,
    // plus everything every earlier pass already collected. Without the fold, the
    // second compaction would forget the first hour of file work.
    let mut files_read: BTreeSet<String> = previous
        .map(|r| r.files_read.iter().cloned().collect())
        .unwrap_or_default();
    let mut files_modified: BTreeSet<String> = previous
        .map(|r| r.files_modified.iter().cloned().collect())
        .unwrap_or_default();
    collect_file_ops(folded, &mut files_read, &mut files_modified);

    log::info!(
        "Context compaction: {} tokens -> summarized journal[{}..{}], keeping {} recent ({} retained)",
        tokens,
        plan.summarize_from,
        plan.first_kept_index,
        messages.len() - plan.first_kept_index,
        messages.len()
    );

    let record = CompactionRecord {
        summary,
        first_kept_index: plan.first_kept_index,
        tokens_before: tokens,
        files_read: files_read.into_iter().collect(),
        files_modified: files_modified.into_iter().collect(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    records.push(record.clone());
    Ok(Some(record))
}

/// Collect the file paths named by tool calls in `messages` into the two sets.
///
/// A path can land in both sets — read, then edited — and is left in both, so
/// the rendered ledger can say so rather than guessing which fact mattered.
fn collect_file_ops(
    messages: &[AgentMessage],
    files_read: &mut BTreeSet<String>,
    files_modified: &mut BTreeSet<String>,
) {
    for message in messages {
        let AgentMessage::ToolCall { name, args, .. } = message else {
            continue;
        };
        let Some(path) = args.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        if READ_TOOLS.contains(&name.as_str()) {
            files_read.insert(path.to_string());
        } else if WRITE_TOOLS.contains(&name.as_str()) {
            files_modified.insert(path.to_string());
        }
    }
}

/// Render the cumulative file ledger for a summary.
///
/// Capped, because the point is to stop the model re-discovering work it already
/// did, and a hundred-path list costs more context than it saves. The elision
/// line is there so the model can tell a short list from a truncated one — a cap
/// the reader cannot see is a cap that gets mistaken for the whole story.
fn render_file_ledger(files_read: &[String], files_modified: &[String]) -> String {
    let mut paths: Vec<&String> = files_read.iter().chain(files_modified.iter()).collect();
    paths.sort();
    paths.dedup();
    let total = paths.len();
    let mut out = String::from("Files touched so far (cumulative across compactions):\n");
    for path in paths.iter().take(FILE_LEDGER_CAP) {
        let read = files_read.contains(path);
        let modified = files_modified.contains(path);
        let how = match (read, modified) {
            (true, true) => "read, modified",
            (false, true) => "modified",
            _ => "read",
        };
        out.push_str(&format!("- {path} ({how})\n"));
    }
    if total > FILE_LEDGER_CAP {
        out.push_str(&format!(
            "[... {} more files elided ...]\n",
            total - FILE_LEDGER_CAP
        ));
    }
    out
}

/// Render the summarizer's input: the previous summary, then the transcript of
/// the region being folded.
///
/// Split out from the call so the partition can be asserted without a model —
/// the guarantee that matters (every message is covered by exactly one summary)
/// is a property of this text, not of the provider's answer.
fn summarizer_input(previous: Option<&str>, messages: &[AgentMessage]) -> String {
    let mut transcript = String::new();
    if let Some(previous) = previous {
        transcript.push_str("Summary of the conversation before this transcript:\n");
        transcript.push_str(previous);
        transcript.push_str("\n\n");
    }
    for msg in messages {
        match msg {
            AgentMessage::User(text) => {
                transcript.push_str(&format!("User: {}\n", text.text));
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
            // Reasoning items are opaque encrypted payloads — nothing useful to
            // add to a human-readable summary transcript.
            AgentMessage::Reasoning { .. } => {}
        }
    }
    transcript
}

async fn summarize_messages<M: CompletionModel + Clone + 'static>(
    model: &M,
    previous: Option<&str>,
    messages: &[AgentMessage],
) -> Result<String, String> {
    let transcript = summarizer_input(previous, messages);

    let agent = rig::agent::AgentBuilder::new(model.clone())
        .preamble(
            "You are a conversation summarizer. Summarize the following agent conversation \
             transcript concisely. Preserve: key decisions made, files read/written, commands run, \
             important findings, and any errors encountered. Be factual and brief. When the \
             transcript is preceded by a summary of earlier conversation, produce one summary \
             covering both — the earlier summary is the only remaining record of that history, so \
             nothing in it may be dropped.",
        )
        .build();

    let summary = agent
        .chat(
            &format!("Summarize this conversation:\n\n{transcript}"),
            &mut Vec::<RigMessage>::new(),
        )
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

    fn file_call(name: &str, path: &str) -> AgentMessage {
        AgentMessage::ToolCall {
            id: "id".into(),
            call_id: Some("cid".into()),
            name: name.into(),
            args: serde_json::json!({ "path": path }),
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

    /// `n` plain messages, each carrying its own index so a test can tell which
    /// region a message ended up in.
    fn numbered(range: std::ops::Range<usize>) -> Vec<AgentMessage> {
        range
            .map(|i| AgentMessage::Assistant(format!("msg-{i}")))
            .collect()
    }

    fn record(summary: &str, first_kept_index: usize) -> CompactionRecord {
        CompactionRecord {
            summary: summary.into(),
            first_kept_index,
            ..Default::default()
        }
    }

    #[test]
    fn the_announcement_predicate_agrees_with_the_work_it_announces() {
        // `needs_compaction` exists only so a client can be told "compacting"
        // before the call starts. If it ever said yes where the real path says
        // no, the session view would announce work that never happens.
        let config = CompactionConfig::default();

        let mut small = AgentState::new("hi".to_string());
        assert!(
            !needs_compaction(&small.messages, &[], &config),
            "a two-message conversation has no old half and nothing to summarize"
        );

        // Long enough to cross the threshold, with plenty older than the
        // keep-recent window.
        let filler = "x".repeat(config.threshold_tokens() * 8);
        for _ in 0..(config.keep_recent_messages + 4) {
            small.messages.push(AgentMessage::Assistant(filler.clone()));
        }
        assert!(
            needs_compaction(&small.messages, &[], &config),
            "a conversation past the threshold is one the caller may announce"
        );
    }

    #[test]
    fn a_compacted_conversation_stops_announcing_compaction() {
        // The threshold is measured on the derived context, not the journal. Get
        // that wrong and a compacted conversation stays over the line forever,
        // paying for a summarization call on every single turn.
        let config = CompactionConfig::default();
        // Each message costs a fifteenth of the threshold, so the 30-message
        // journal is twice over the line while the 5-message kept tail is a
        // third of it.
        let filler = "x".repeat(config.threshold_tokens() * 4 / 15);
        let messages: Vec<AgentMessage> = (0..30)
            .map(|_| AgentMessage::Assistant(filler.clone()))
            .collect();
        let records = vec![record("earlier", 25)];

        assert!(
            estimate_tokens(&messages) >= config.threshold_tokens(),
            "the journal itself is over the threshold, which is the trap"
        );
        assert!(
            effective_tokens(&messages, &records) < config.threshold_tokens(),
            "the derived context is what the model is sent, and it is small"
        );
        assert!(!needs_compaction(&messages, &records, &config));
    }

    #[test]
    fn forcing_a_compaction_skips_the_size_check_and_nothing_else() {
        let config = CompactionConfig::default();
        let under = config.threshold_tokens() - 1;
        let over = config.threshold_tokens() + 1;

        // The reason `/compact` exists: automatic compaction only fires at 60% of
        // the window, which is long after the point where someone can feel a
        // conversation getting heavy and wants room before the next question.
        assert!(!should_summarize(
            under,
            20,
            &config,
            CompactionTrigger::Threshold
        ));
        assert!(should_summarize(
            under,
            20,
            &config,
            CompactionTrigger::Forced
        ));

        // Both still compact once the context is genuinely large.
        assert!(should_summarize(
            over,
            20,
            &config,
            CompactionTrigger::Threshold
        ));
        assert!(should_summarize(
            over,
            20,
            &config,
            CompactionTrigger::Forced
        ));

        // Neither pays for a summary of nothing: with no messages older than
        // `keep_recent_messages` there is no old half to fold up.
        assert!(!should_summarize(
            over,
            0,
            &config,
            CompactionTrigger::Forced
        ));
        assert!(!should_summarize(
            under,
            0,
            &config,
            CompactionTrigger::Forced
        ));
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
            AgentMessage::User("hi".into()),           // 0
            AgentMessage::Assistant("working".into()), // 1
            tool_call("read"),                         // 2
            tool_result("read", "contents"), // 3  <- naive boundary (keep_recent=2) starts here
            AgentMessage::Assistant("done".into()), // 4
        ];
        // Naive split = 5 - 2 = 3 (a ToolResult). safe_split walks back to 2.
        assert_eq!(safe_split(&messages, 2), 2);
        assert!(!matches!(
            messages[safe_split(&messages, 2)],
            AgentMessage::ToolResult { .. }
        ));
    }

    #[test]
    fn safe_split_keeps_reasoning_with_its_tool_call() {
        // A reasoning item leads the tool block. The kept window must not start
        // after it, or the tool call loses its paired reasoning item and the
        // Responses API rejects the next request.
        let messages = vec![
            AgentMessage::User("hi".into()), // 0
            AgentMessage::Reasoning {
                id: "rs_1".into(),
                encrypted: "enc".into(),
                summary: Vec::new(),
            }, // 1
            tool_call("read"),               // 2
            tool_result("read", "contents"), // 3  <- naive boundary (keep_recent=2)
            AgentMessage::Assistant("done".into()), // 4
        ];
        // Naive split = 3 (ToolResult) -> walk to 2 (ToolCall) -> pull in the
        // preceding reasoning at 1.
        assert_eq!(safe_split(&messages, 2), 1);
        assert!(matches!(
            messages[safe_split(&messages, 2)],
            AgentMessage::Reasoning { .. }
        ));
    }

    #[test]
    fn safe_split_keeps_reasoning_with_a_parallel_tool_batch() {
        // Reasoning followed by two parallel tool calls: walking back from a
        // mid-batch boundary must pass both calls and still land on the reasoning.
        let messages = vec![
            AgentMessage::User("hi".into()), // 0
            AgentMessage::Reasoning {
                id: "rs_1".into(),
                encrypted: "enc".into(),
                summary: Vec::new(),
            }, // 1
            tool_call("read"),               // 2
            tool_call("grep"),               // 3
            tool_result("read", "a"),        // 4
            tool_result("grep", "b"),        // 5  <- naive boundary (keep_recent=1)
        ];
        assert_eq!(safe_split(&messages, 1), 1);
    }

    #[test]
    fn compaction_keeps_the_originals_and_derives_summary_plus_tail() {
        // The defect this module exists to fix: the transcript used to be
        // replaced by the summary. The journal must come out of a compaction
        // byte-for-byte identical, with only the derived view narrowed.
        let messages = numbered(0..12);
        let records = vec![record("what happened earlier", 9)];

        let effective = effective_context(&messages, &records);
        assert_eq!(
            effective.len(),
            4,
            "one summary plus messages 9, 10 and 11"
        );
        assert!(
            matches!(&effective[0], AgentMessage::Assistant(t) if t.starts_with(SUMMARY_MARKER)
                && t.contains("what happened earlier"))
        );
        assert!(matches!(&effective[1], AgentMessage::Assistant(t) if t == "msg-9"));
        assert!(matches!(&effective[3], AgentMessage::Assistant(t) if t == "msg-11"));
        assert!(
            messages.len() == 12
                && messages.iter().enumerate().all(
                    |(i, m)| matches!(m, AgentMessage::Assistant(t) if t == &format!("msg-{i}"))
                ),
            "the journal is untouched — deleting the record puts every message back"
        );
    }

    #[test]
    fn a_stale_boundary_degrades_instead_of_panicking() {
        // A record can outlive the journal it indexed (hand-edited chat file).
        // Slicing past the end would panic on the way into a turn.
        let messages = numbered(0..3);
        let effective = effective_context(&messages, &vec![record("gone", 99)]);
        assert_eq!(effective.len(), 1);
    }

    #[test]
    fn a_second_compaction_leaves_no_message_region_uncovered() {
        // The bug in "summarize only what arrived since last time": the region
        // between the old boundary and the new one belongs to neither summary,
        // and the model never sees it again.
        let config = CompactionConfig {
            keep_recent_messages: 10,
            ..Default::default()
        };
        let mut records: Vec<CompactionRecord> = Vec::new();

        // First pass over a 30-message journal.
        let first_journal = numbered(0..30);
        let plan1 = plan_compaction(&first_journal, &records, config.keep_recent_messages)
            .expect("30 messages, keeping 10, has an old half");
        assert_eq!((plan1.summarize_from, plan1.first_kept_index), (0, 20));
        let input1 = summarizer_input(
            None,
            &first_journal[plan1.summarize_from..plan1.first_kept_index],
        );
        records.push(record("SUMMARY-ONE", plan1.first_kept_index));

        // Ten more messages arrive; the kept window after pass one was 20..30.
        let journal = numbered(0..40);
        let plan2 = plan_compaction(&journal, &records, config.keep_recent_messages)
            .expect("ten new messages are an old half again");
        assert_eq!(
            (plan2.summarize_from, plan2.first_kept_index),
            (20, 30),
            "the second pass starts exactly where the first stopped"
        );
        let input2 = summarizer_input(
            records.last().map(|r| r.summary.as_str()),
            &journal[plan2.summarize_from..plan2.first_kept_index],
        );

        // The first summary is folded in, not left sitting beside a gap.
        assert!(
            input2.contains("SUMMARY-ONE"),
            "the previous summary must be part of the second summarization input"
        );
        // Every message that was "recent" after pass one is now summarized.
        for i in 20..30 {
            assert!(
                input2.contains(&format!("msg-{i}")),
                "message {i} was recent after pass one and must be summarized by pass two"
            );
        }
        records.push(record("SUMMARY-TWO", plan2.first_kept_index));

        // Nothing is unrepresented: each index is summarized by pass one, by
        // pass two, or still verbatim in the context the model is now sent.
        let kept: Vec<String> = effective_context(&journal, &records)
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Assistant(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        for i in 0..40 {
            let needle = format!("msg-{i}");
            assert!(
                input1.contains(&needle) || input2.contains(&needle) || kept.contains(&needle),
                "message {i} is covered by neither summary nor the kept window"
            );
        }
    }

    #[test]
    fn a_second_compaction_will_not_pay_to_resummarize_a_summary() {
        // With nothing new behind the boundary there is no journal message to
        // fold, and re-summarizing the previous summary only loses detail.
        let journal = numbered(0..30);
        let records = vec![record("SUMMARY-ONE", 20)];
        assert!(plan_compaction(&journal, &records, 10).is_none());
    }

    #[test]
    fn the_file_ledger_folds_forward_and_marks_how_each_file_was_touched() {
        let mut read = BTreeSet::from(["carried/over.rs".to_string()]);
        let mut modified = BTreeSet::new();
        let folded = vec![
            file_call("read_file", "src/a.rs"),
            file_call("grep", "src/"),
            file_call("edit_file", "src/a.rs"),
            file_call("write_file", "src/new.rs"),
            file_call("bash", "ignored.rs"),
            tool_call("read_file"), // no `path` argument at all
        ];
        collect_file_ops(&folded, &mut read, &mut modified);

        assert!(read.contains("carried/over.rs"), "prior sets survive");
        assert!(read.contains("src/a.rs") && read.contains("src/"));
        assert!(modified.contains("src/a.rs") && modified.contains("src/new.rs"));
        assert!(
            !read.contains("ignored.rs") && !modified.contains("ignored.rs"),
            "a tool that is not a file tool names no file"
        );

        let rendered = render_file_ledger(
            &read.iter().cloned().collect::<Vec<_>>(),
            &modified.iter().cloned().collect::<Vec<_>>(),
        );
        assert!(rendered.contains("- src/a.rs (read, modified)"));
        assert!(rendered.contains("- src/new.rs (modified)"));
        assert!(rendered.contains("- carried/over.rs (read)"));
    }

    #[test]
    fn the_file_ledger_says_when_it_elided() {
        // A cap the reader cannot see is a cap that reads as the whole story.
        let read: Vec<String> = (0..FILE_LEDGER_CAP + 3)
            .map(|i| format!("src/f{i:02}.rs"))
            .collect();
        let rendered = render_file_ledger(&read, &[]);
        assert_eq!(rendered.lines().filter(|l| l.starts_with("- ")).count(), 20);
        assert!(rendered.contains("[... 3 more files elided ...]"));
    }

    #[test]
    fn a_held_journal_restores_the_history_under_the_turns_new_messages() {
        let mut state = AgentState::new("first".to_string());
        state.messages = numbered(0..12);
        let records = vec![record("earlier", 9)];

        let held = HeldJournal::derive(&mut state, &records).expect("a record means a derived view");
        assert_eq!(state.messages.len(), 4, "summary + msg-9..msg-11");

        // The turn appends, as every turn does.
        state.messages.push(AgentMessage::Assistant("new-1".into()));
        state.messages.push(AgentMessage::Assistant("new-2".into()));
        held.restore(&mut state);

        assert_eq!(state.messages.len(), 14);
        assert!(matches!(&state.messages[0], AgentMessage::Assistant(t) if t == "msg-0"));
        assert!(matches!(&state.messages[11], AgentMessage::Assistant(t) if t == "msg-11"));
        assert!(matches!(&state.messages[12], AgentMessage::Assistant(t) if t == "new-1"));
        assert!(matches!(&state.messages[13], AgentMessage::Assistant(t) if t == "new-2"));
        assert!(
            !state
                .messages
                .iter()
                .any(|m| matches!(m, AgentMessage::Assistant(t) if t.starts_with(SUMMARY_MARKER))),
            "the derived summary is a view, and must never be persisted as history"
        );
    }

    #[test]
    fn an_uncompacted_turn_derives_nothing() {
        let mut state = AgentState::new("hi".to_string());
        assert!(
            HeldJournal::derive(&mut state, &[]).is_none(),
            "with no records the journal is the context; copying it would be waste"
        );
        assert_eq!(state.messages.len(), 1, "and the state is left alone");
    }

    #[test]
    fn a_record_loads_from_a_file_that_predates_its_fields() {
        // Records live inside persisted chats. A file written before a field
        // existed must still load, or a release deletes conversations.
        let sparse: CompactionRecord =
            serde_json::from_str(r#"{"summary":"old","first_kept_index":7}"#).unwrap();
        assert_eq!(sparse.summary, "old");
        assert_eq!(sparse.first_kept_index, 7);
        assert!(sparse.files_read.is_empty() && sparse.files_modified.is_empty());
        assert_eq!(sparse.tokens_before, 0);

        let empty: CompactionRecord = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, CompactionRecord::default());
    }
}
