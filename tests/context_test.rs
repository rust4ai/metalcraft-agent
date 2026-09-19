//! The two questions the context module answers from outside it: how big a
//! conversation is, and what a compacted one actually sends.
//!
//! Compaction used to be destructive — `context::compact` overwrote
//! `state.messages` with a summary plus a tail — and these tests asserted on
//! the mutation. It is a *record* now: the journal is kept whole and
//! [`context::effective_context`] derives what the turn runs on. The observable
//! contract is the same one (a summary, then the kept window, in that order),
//! so it is pinned against the API that exists rather than the one that did.

use metalcraft::{AgentMessage, AgentState};
use metalcraft_agent::context::{self, CompactionRecord};

/// A record that covers everything before `first_kept_index`.
fn record(summary: &str, first_kept_index: usize) -> CompactionRecord {
    CompactionRecord {
        summary: summary.into(),
        first_kept_index,
        ..Default::default()
    }
}

#[test]
fn estimate_tokens_empty() {
    let state = AgentState::new("hello");
    let tokens = context::estimate_tokens(&state.messages);
    // "hello" = 5 chars / 4 ≈ 1
    assert!(tokens >= 1 && tokens <= 2);
}

#[test]
fn estimate_tokens_with_history() {
    let mut state = AgentState::new("hello world");
    state.messages.push(AgentMessage::Assistant(
        "This is a response with some content.".into(),
    ));
    state
        .messages
        .push(AgentMessage::User("Follow up question here.".into()));
    let tokens = context::estimate_tokens(&state.messages);
    // Total chars: 11 + 37 + 24 = 72, / 4 = 18
    assert!(tokens > 10 && tokens < 30);
}

#[test]
fn a_compacted_context_is_the_summary_then_the_kept_window() {
    let messages = vec![
        AgentMessage::User("message 1".into()),
        AgentMessage::Assistant("response 1".into()),
        AgentMessage::User("message 2".into()),
        AgentMessage::Assistant("response 2".into()),
        AgentMessage::User("message 3".into()),
        AgentMessage::Assistant("response 3".into()),
    ];

    let context = context::effective_context(&messages, &[record("Summary of early conversation.", 4)]);

    assert_eq!(context.len(), 3, "summary + the two kept messages");
    match &context[0] {
        AgentMessage::Assistant(text) => {
            assert!(text.contains("Summary of early conversation"));
        }
        other => panic!("expected the summary first, got {other:?}"),
    }
    match &context[1] {
        AgentMessage::User(input) => assert_eq!(input.text, "message 3"),
        other => panic!("expected the kept window next, got {other:?}"),
    }
    match &context[2] {
        AgentMessage::Assistant(text) => assert_eq!(text, "response 3"),
        other => panic!("expected the kept window next, got {other:?}"),
    }
}

#[test]
fn an_uncompacted_conversation_is_its_own_context() {
    let messages = vec![
        AgentMessage::User("hello".into()),
        AgentMessage::Assistant("hi".into()),
    ];

    let context = context::effective_context(&messages, &[]);

    assert_eq!(context.len(), 2, "nothing was summarised, so nothing is replaced");
    match &context[0] {
        AgentMessage::User(input) => assert_eq!(input.text, "hello"),
        other => panic!("expected the original message, got {other:?}"),
    }
}

#[test]
fn a_tool_call_inside_the_kept_window_survives_with_its_result() {
    // The pairing is the point: a `tool_call` whose result was summarised away
    // is the orphan the Responses API rejects with a 400.
    let messages = vec![
        AgentMessage::User("do something".into()),
        AgentMessage::ToolCall {
            call_id: None,
            id: "1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "foo.rs"}),
        },
        AgentMessage::ToolResult {
            call_id: None,
            id: "1".into(),
            name: "read_file".into(),
            result: "file contents".into(),
        },
        AgentMessage::Assistant("done".into()),
    ];

    let context = context::effective_context(&messages, &[record("old stuff", 1)]);

    assert_eq!(context.len(), 4, "summary + the three kept messages");
    match &context[0] {
        AgentMessage::Assistant(text) => assert!(text.contains("old stuff")),
        other => panic!("expected the summary, got {other:?}"),
    }
    match (&context[1], &context[2]) {
        (
            AgentMessage::ToolCall { id: call, .. },
            AgentMessage::ToolResult { id: result, .. },
        ) => assert_eq!(call, result, "a call and its result stay together"),
        other => panic!("expected the call/result pair, got {other:?}"),
    }
}

#[test]
fn a_record_pointing_past_the_journal_degrades_to_the_summary() {
    // A hand-edited chat file, or a reset that dropped messages. Clamping is
    // wrong but answerable; slicing out of range would panic on the way into a
    // turn.
    let messages = vec![AgentMessage::User("only this".into())];

    let context = context::effective_context(&messages, &[record("everything", 99)]);

    assert_eq!(context.len(), 1);
    assert!(matches!(&context[0], AgentMessage::Assistant(text) if text.contains("everything")));
}
