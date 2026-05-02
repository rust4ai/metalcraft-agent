use metalcraft::{AgentMessage, AgentState};
use metalcraft_agent::context;

#[test]
fn estimate_tokens_empty() {
    let state = AgentState::new("hello");
    let tokens = context::estimate_tokens(&state);
    // "hello" = 5 chars / 4 ≈ 1
    assert!(tokens >= 1 && tokens <= 2);
}

#[test]
fn estimate_tokens_with_history() {
    let mut state = AgentState::new("hello world");
    state.messages.push(AgentMessage::Assistant("This is a response with some content.".into()));
    state.messages.push(AgentMessage::User("Follow up question here.".into()));
    let tokens = context::estimate_tokens(&state);
    // Total chars: 11 + 37 + 24 = 72, / 4 = 18
    assert!(tokens > 10 && tokens < 30);
}

#[test]
fn compact_replaces_old_messages() {
    let mut state = AgentState::new("message 1");
    state.messages.push(AgentMessage::Assistant("response 1".into()));
    state.messages.push(AgentMessage::User("message 2".into()));
    state.messages.push(AgentMessage::Assistant("response 2".into()));
    state.messages.push(AgentMessage::User("message 3".into()));
    state.messages.push(AgentMessage::Assistant("response 3".into()));

    assert_eq!(state.messages.len(), 6);

    context::compact(&mut state, "Summary of early conversation.".into(), 2);

    // Should have: summary + 2 recent messages = 3
    assert_eq!(state.messages.len(), 3);

    // First message should be the summary
    match &state.messages[0] {
        AgentMessage::Assistant(text) => {
            assert!(text.contains("Summary of early conversation"));
        }
        _ => panic!("Expected Assistant message with summary"),
    }

    // Last two should be the original last two
    match &state.messages[1] {
        AgentMessage::User(text) => assert_eq!(text, "message 3"),
        _ => panic!("Expected User message"),
    }
    match &state.messages[2] {
        AgentMessage::Assistant(text) => assert_eq!(text, "response 3"),
        _ => panic!("Expected Assistant message"),
    }
}

#[test]
fn compact_noop_when_few_messages() {
    let mut state = AgentState::new("hello");
    state.messages.push(AgentMessage::Assistant("hi".into()));

    context::compact(&mut state, "should not apply".into(), 5);

    // Only 2 messages, keep_recent=5, so no compaction
    assert_eq!(state.messages.len(), 2);
    match &state.messages[0] {
        AgentMessage::User(text) => assert_eq!(text, "hello"),
        _ => panic!("Expected original message"),
    }
}

#[test]
fn compact_preserves_tool_calls_in_recent() {
    let mut state = AgentState::new("do something");
    state.messages.push(AgentMessage::ToolCall {
        call_id: None, id: "1".into(),
        name: "read_file".into(),
        args: serde_json::json!({"path": "foo.rs"}),
    });
    state.messages.push(AgentMessage::ToolResult {
        call_id: None, id: "1".into(),
        name: "read_file".into(),
        result: "file contents".into(),
    });
    state.messages.push(AgentMessage::Assistant("done".into()));

    context::compact(&mut state, "old stuff".into(), 3);

    // 4 messages, keep 3 => summary + 3 = 4
    assert_eq!(state.messages.len(), 4);
    match &state.messages[0] {
        AgentMessage::Assistant(text) => assert!(text.contains("old stuff")),
        _ => panic!("Expected summary"),
    }
    match &state.messages[1] {
        AgentMessage::ToolCall { name, .. } => assert_eq!(name, "read_file"),
        _ => panic!("Expected ToolCall"),
    }
}
