//! Running a flow that declares inputs nobody supplied.
//!
//! The rule this pins: **an unsupplied required input is a warning, not a
//! refusal.** The pod used to answer `missing required inputs: message` and run
//! nothing, which landed on exactly the flows people were most likely to press
//! Run on — every template worth shipping declares its inputs, so the templates
//! were the flows that could not be tried. Now the run happens, the input is
//! simply unset, and the warning rides in the summary (and the log, and the
//! diagnostics trace) so a prompt that reads oddly has its cause attached.
//!
//! Deliberately LLM-free: `set_variable` interpolates without an agent, so this
//! proves the empty-render behaviour deterministically and runs in CI with no key.

use metalcraft_agent::flow_exec::FlowExecutor;
use metalcraft_agent::runtime::AgentRuntimeContext;
use metalcraft_flows::SavedFlow;
use serde_json::json;

/// entry(`who` required, `greeting` defaulted) → set_variable → end.
fn greeting_flow() -> SavedFlow {
    serde_json::from_str(
        r##"{
          "spec_version": "3",
          "id": "greeting-inputs-test",
          "name": "Greeting (test)",
          "created_at": "2026-08-27T00:00:00Z",
          "updated_at": "2026-08-27T00:00:00Z",
          "flow": {
            "nodes": [
              { "id": "entry", "node_type": "entry", "data": { "inputs": {
                  "who": { "type": "string", "required": true },
                  "greeting": { "type": "string", "required": false, "default": "hello" }
              } } },
              { "id": "compose", "node_type": "set_variable",
                "data": { "variable": "line", "value": "{{greeting}}, {{who}}!" } },
              { "id": "done", "node_type": "end", "data": { "status": "composed" } }
            ],
            "edges": [
              { "id": "e0", "source": "entry", "target": "compose" },
              { "id": "e1", "source": "compose", "target": "done" }
            ]
          }
        }"##,
    )
    .expect("flow parses")
}

fn context() -> AgentRuntimeContext {
    // Built by hand rather than `from_environment`: this flow never reaches an
    // agent, and the environment's only contribution would be an API key that
    // must not be needed to run one.
    AgentRuntimeContext {
        personas_dir: std::env::temp_dir().join("flow-inputs-test-personas"),
        skills_dir: std::env::temp_dir().join("flow-inputs-test-skills"),
        api_key: "not-used-by-this-flow".into(),
    }
}

#[tokio::test]
async fn a_missing_required_input_warns_and_still_runs() {
    let summary = FlowExecutor::new(
        &context(),
        greeting_flow(),
        ".",
        "coding-agent",
        "gpt-5.6-luna",
        &json!({}),
        None,
    )
    .expect("an unsupplied input must not stop the executor being built")
    .run()
    .await
    .expect("run flow");

    assert_eq!(summary.status, "composed", "the run reaches its end node");
    assert!(
        summary.warnings.iter().any(|w| w.contains("who")),
        "the summary names the input nobody supplied: {:?}",
        summary.warnings
    );
    assert!(
        !summary.warnings.iter().any(|w| w.contains("greeting")),
        "an input with a default was supplied by the flow itself: {:?}",
        summary.warnings
    );
    // Unset reads as empty — the same as any missing `{{path}}`, so the output
    // shows what is absent rather than inventing a value for it.
    assert_eq!(
        summary.variables.get("line").and_then(|v| v.as_str()),
        Some("hello, !"),
    );
}

#[tokio::test]
async fn supplied_inputs_are_used_and_warn_about_nothing() {
    let summary = FlowExecutor::new(
        &context(),
        greeting_flow(),
        ".",
        "coding-agent",
        "gpt-5.6-luna",
        &json!({ "who": "Andrew", "greeting": "good morning" }),
        None,
    )
    .expect("construct executor")
    .run()
    .await
    .expect("run flow");

    assert_eq!(
        summary.variables.get("line").and_then(|v| v.as_str()),
        Some("good morning, Andrew!"),
    );
    assert!(
        summary.warnings.is_empty(),
        "nothing to warn about: {:?}",
        summary.warnings
    );
}
