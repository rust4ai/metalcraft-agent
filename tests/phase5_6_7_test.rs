use metalcraft::{
    AgentState, AgentUpdate, GuardAction, PendingToolCall, Reducer, StepEvent, ToolResult,
};
use metalcraft_agent::guard::{GuardConfig, build_agent_guard};

// ============================================================================
// AgentState basics — turns, tools_called, final_answer
// ============================================================================

#[test]
fn turns_tracks_tool_calls_and_results() {
    let mut state = AgentState::new("test");

    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "foo.txt"}),
        }],
    });

    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "1".into(),
        name: "read_file".into(),
        result: Ok(serde_json::json!({"content": "hello"})),
    }]));

    state.apply(AgentUpdate::FinalAnswer("done".into()));

    let turns = state.turns();
    assert_eq!(turns.len(), 2); // tool turn + final answer turn
    assert_eq!(turns[0].tool_calls.len(), 1);
    assert_eq!(turns[0].tool_calls[0].name, "read_file");
    assert_eq!(turns[1].assistant_text.as_deref(), Some("done"));
}

#[test]
fn tools_called_returns_all_tool_names() {
    let mut state = AgentState::new("test");

    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![
            PendingToolCall {
                call_id: None,
                id: "1".into(),
                name: "read_file".into(),
                args: serde_json::json!({}),
            },
            PendingToolCall {
                call_id: None,
                id: "2".into(),
                name: "grep".into(),
                args: serde_json::json!({}),
            },
        ],
    });
    state.apply(AgentUpdate::ToolResults(vec![
        ToolResult {
            call_id: None,
            id: "1".into(),
            name: "read_file".into(),
            result: Ok(serde_json::json!({})),
        },
        ToolResult {
            call_id: None,
            id: "2".into(),
            name: "grep".into(),
            result: Ok(serde_json::json!({})),
        },
    ]));

    let called = state.tools_called();
    assert!(called.contains(&"read_file".to_string()));
    assert!(called.contains(&"grep".to_string()));
}

#[test]
fn final_answer_only_when_done() {
    let mut state = AgentState::new("test");
    assert_eq!(state.final_answer(), None);

    state.apply(AgentUpdate::FinalAnswer("the answer".into()));
    assert_eq!(state.final_answer(), Some("the answer"));
}

#[test]
fn continue_with_preserves_history() {
    let mut state = AgentState::new("first");
    state.apply(AgentUpdate::FinalAnswer("answer 1".into()));

    let state = state.continue_with("second");
    assert!(!state.is_done);
    assert_eq!(state.messages.len(), 3); // User, Assistant, User
}

// ============================================================================
// Sub-Agent registration tests
// ============================================================================

#[test]
fn sub_agent_registered_with_config() {
    use metalcraft_agent::tools::{ToolConfig, create_registry_for_with_config};

    let config = ToolConfig {
        api_key: "test-key".into(),
        model_name: "gpt-4o".into(),
        system_prompt: "You are helpful.".into(),
        skills_dir: std::path::PathBuf::from("skills"),
        available_skills: vec![],
        reply_sink: None,
        session_binding: None,
        reschedule_depth: 0,
        preset_personas: None,
        instance_id: None,
        // Nothing to stop for: this asserts what gets registered, not what a
        // delegated run does when the turn is stopped.
        interrupt: None,
        turn_plan: None,
    goal_id: None,
    };

    let tools: Vec<String> = vec!["read_file", "sub_agent"]
        .into_iter()
        .map(String::from)
        .collect();
    let registry = create_registry_for_with_config(&tools, Some(&config));

    let names = registry.names();
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"sub_agent"));
}

#[test]
fn sub_agent_skipped_without_config() {
    use metalcraft_agent::tools::create_registry_for;

    let tools: Vec<String> = vec!["read_file", "sub_agent"]
        .into_iter()
        .map(String::from)
        .collect();
    let registry = create_registry_for(&tools);

    let names = registry.names();
    assert!(names.contains(&"read_file"));
    assert!(!names.contains(&"sub_agent")); // skipped, no config
}

// ============================================================================
// Guard: error spiral detection
// ============================================================================

fn dummy_event() -> StepEvent {
    StepEvent {
        node: "tools".into(),
        next: "agent".into(),
        duration: std::time::Duration::from_millis(0),
        outcome: metalcraft::StepOutcome::Success,
    }
}

#[test]
fn guard_detects_error_spiral() {
    let guard = build_agent_guard(
        GuardConfig {
            verbose: false,
            max_consecutive_errors: 2,
            max_identical_repeats: 0,
            max_poll_repeats: 0,
            ..GuardConfig::default()
        },
        None,
    );

    let mut state = AgentState::new("test");

    // First all-error turn
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "bad"}),
        }],
    });
    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "1".into(),
        name: "bash".into(),
        result: Err("failed".into()),
    }]));
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));

    // Second all-error turn -> should stop
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "2".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "also bad"}),
        }],
    });
    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "2".into(),
        name: "bash".into(),
        result: Err("also failed".into()),
    }]));
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Stop(_)
    ));
}

#[test]
fn guard_resets_on_success() {
    let guard = build_agent_guard(
        GuardConfig {
            verbose: false,
            max_consecutive_errors: 2,
            max_identical_repeats: 0,
            max_poll_repeats: 0,
            ..GuardConfig::default()
        },
        None,
    );

    let mut state = AgentState::new("test");

    // One error turn
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "bad"}),
        }],
    });
    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "1".into(),
        name: "bash".into(),
        result: Err("failed".into()),
    }]));
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));

    // Success resets counter
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "2".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "good"}),
        }],
    });
    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "2".into(),
        name: "bash".into(),
        result: Ok(serde_json::json!("ok")),
    }]));
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));

    // Another error — should be fine (counter was reset)
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "3".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "bad again"}),
        }],
    });
    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "3".into(),
        name: "bash".into(),
        result: Err("nope".into()),
    }]));
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));
}

// ============================================================================
// Guard: loop detection
// ============================================================================

#[test]
fn guard_detects_repeated_tool_call() {
    let guard = build_agent_guard(
        GuardConfig {
            verbose: false,
            max_consecutive_errors: 0,
            max_identical_repeats: 1,
            ..GuardConfig::default()
        },
        None,
    );

    let mut state = AgentState::new("test");

    // First call
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "foo.txt"}),
        }],
    });
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));

    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "1".into(),
        name: "read_file".into(),
        result: Ok(serde_json::json!({"content": "hello"})),
    }]));
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));

    // Same call again -> loop detected
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "2".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "foo.txt"}),
        }],
    });
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Stop(_)
    ));
}

#[test]
fn guard_allows_different_args() {
    let guard = build_agent_guard(
        GuardConfig {
            verbose: false,
            max_consecutive_errors: 0,
            max_identical_repeats: 1,
            ..GuardConfig::default()
        },
        None,
    );

    let mut state = AgentState::new("test");

    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "foo.txt"}),
        }],
    });
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));

    state.apply(AgentUpdate::ToolResults(vec![ToolResult {
        call_id: None,
        id: "1".into(),
        name: "read_file".into(),
        result: Ok(serde_json::json!({})),
    }]));
    guard(&state, &dummy_event());

    // Different args — not a loop
    state.apply(AgentUpdate::ToolCalls {
        reasoning: vec![],
        calls: vec![PendingToolCall {
            call_id: None,
            id: "2".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "bar.txt"}),
        }],
    });
    assert!(matches!(
        guard(&state, &dummy_event()),
        GuardAction::Continue
    ));
}
