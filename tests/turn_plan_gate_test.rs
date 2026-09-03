//! The plan gate, end to end through the real tool registry.
//!
//! The unit tests in `turn_plan` cover the bookkeeping; these cover the wiring
//! that makes it a mechanism: that `update_plan`, `say_to_user` and `ask_user`
//! actually share one plan when built from one `ToolConfig`, and that the
//! refusal shows up as a tool error — which is what stops the turn ending, since
//! metalcraft 0.10 no longer treats a failed terminal tool as terminal.

use std::sync::Arc;

use metalcraft::ToolRegistry;
use metalcraft_agent::tools::{
    ReplyEnvelope, ReplySink, ToolConfig, create_registry_for_with_config,
};
use tokio::sync::Mutex;

type Delivered = Arc<Mutex<Vec<(String, bool)>>>;

/// A sink that records (text, awaiting_reply) for every message delivered.
fn recording_sink(buf: Delivered) -> ReplySink {
    Arc::new(move |reply: ReplyEnvelope| {
        let buf = buf.clone();
        Box::pin(async move {
            buf.lock().await.push((reply.text, reply.awaiting_reply));
            Ok(())
        })
    })
}

fn registry_with_plan(sink: ReplySink) -> ToolRegistry {
    let config = ToolConfig {
        api_key: "test-key".into(),
        model_name: "gpt-5.4".into(),
        system_prompt: "You orchestrate.".into(),
        skills_dir: std::path::PathBuf::from("skills"),
        available_skills: vec![],
        reply_sink: Some(sink),
        session_binding: None,
        reschedule_depth: 0,
        preset_personas: None,
        sub_agent_depth: 0,
        instance_id: None,
        interrupt: None,
        turn_plan: Some(metalcraft_agent::turn_plan::new_shared()),
    goal_id: None,
    };
    let names: Vec<String> = ["update_plan", "say_to_user", "ask_user"]
        .into_iter()
        .map(String::from)
        .collect();
    create_registry_for_with_config(&names, Some(&config))
}

fn plan(steps: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "steps": steps })
}

#[tokio::test]
async fn an_open_plan_stops_the_turn_from_closing() {
    let delivered: Delivered = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with_plan(recording_sink(delivered.clone()));

    registry
        .call(
            "update_plan",
            plan(serde_json::json!([
                {"step": "read the repo's features", "persona": "research-agent", "status": "done"},
                {"step": "correct the landing page", "persona": "coding-agent"},
            ])),
        )
        .await
        .expect("writing a plan succeeds");

    // The orchestrator tries to answer after one delegation. It must not land.
    let refused = registry
        .call("say_to_user", serde_json::json!({"message": "All done!"}))
        .await
        .expect_err("an open plan refuses the reply");
    let refused = refused.to_string();
    assert!(
        refused.contains("correct the landing page"),
        "the refusal must name what is still owed: {refused}"
    );
    assert!(
        delivered.lock().await.is_empty(),
        "a refused reply must not reach the user"
    );

    // Closing the step lets the same answer through.
    registry
        .call(
            "update_plan",
            plan(serde_json::json!([
                {"step": "read the repo's features", "status": "done"},
                {"step": "correct the landing page", "status": "done"},
            ])),
        )
        .await
        .expect("closing the plan succeeds");
    registry
        .call("say_to_user", serde_json::json!({"message": "All done!"}))
        .await
        .expect("a closed plan delivers");
    assert_eq!(
        delivered.lock().await.as_slice(),
        &[("All done!".to_string(), false)]
    );
}

/// A question is always a legal way to end a turn — being stuck mid-plan is
/// precisely when the agent most needs to ask.
#[tokio::test]
async fn a_question_is_never_gated() {
    let delivered: Delivered = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with_plan(recording_sink(delivered.clone()));

    registry
        .call(
            "update_plan",
            plan(serde_json::json!([{"step": "correct the landing page"}])),
        )
        .await
        .expect("writing a plan succeeds");

    registry
        .call(
            "ask_user",
            serde_json::json!({
                "question": "Should I report the drift or fix the page?",
                "options": ["Just report it", "Report and fix"],
                "why": "Fixing means editing Hero.tsx.",
            }),
        )
        .await
        .expect("ask_user is not gated by the plan");

    let out = delivered.lock().await;
    let (text, awaiting) = out.first().expect("the question was delivered");
    assert!(*awaiting, "a question leaves the conversation open");
    assert!(text.contains("report the drift"));
    assert!(
        text.contains("Fixing means editing Hero.tsx."),
        "`why` rides along in the text so every channel can carry it: {text}"
    );
}

/// The rail must slow a turn down, never trap it.
#[tokio::test]
async fn the_gate_gives_up_rather_than_deadlocking_the_turn() {
    let delivered: Delivered = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with_plan(recording_sink(delivered.clone()));

    registry
        .call(
            "update_plan",
            plan(serde_json::json!([{"step": "still open"}])),
        )
        .await
        .expect("writing a plan succeeds");

    for attempt in 1..=2 {
        let result = registry
            .call("say_to_user", serde_json::json!({"message": "done?"}))
            .await;
        assert!(result.is_err(), "attempt {attempt} should be refused");
    }
    registry
        .call("say_to_user", serde_json::json!({"message": "done."}))
        .await
        .expect("the third attempt is let through with the plan still open");
    assert_eq!(delivered.lock().await.len(), 1);
}

/// Without a shared plan there is no gate at all — a sub-agent, a flow node or a
/// one-shot run answers whenever it likes, and `update_plan` is not offered.
#[tokio::test]
async fn no_plan_means_no_gate_and_no_plan_tool() {
    let delivered: Delivered = Arc::new(Mutex::new(Vec::new()));
    let config = ToolConfig {
        api_key: "k".into(),
        model_name: "gpt-5.4".into(),
        system_prompt: "p".into(),
        skills_dir: std::path::PathBuf::from("skills"),
        available_skills: vec![],
        reply_sink: Some(recording_sink(delivered.clone())),
        session_binding: None,
        reschedule_depth: 0,
        preset_personas: None,
        sub_agent_depth: 0,
        instance_id: None,
        interrupt: None,
        turn_plan: None,
    goal_id: None,
    };
    let names: Vec<String> = ["update_plan", "say_to_user"]
        .into_iter()
        .map(String::from)
        .collect();
    let registry = create_registry_for_with_config(&names, Some(&config));

    assert!(
        !registry.names().contains(&"update_plan"),
        "a tool whose writes nothing would read must not be offered"
    );
    registry
        .call("say_to_user", serde_json::json!({"message": "hi"}))
        .await
        .expect("an ungated reply delivers");
    assert_eq!(delivered.lock().await.len(), 1);
}
