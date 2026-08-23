//! LLM-in-the-loop end-to-end test of the Madrid weather user story.
//!
//! Proves a real flow *run* conducts the story: a `branch` node runs an agent
//! that calls a **mock** weather tool (no real API), terminates by selecting a
//! typed output handle, and the resulting `i64` payload flows down the chosen
//! edge into a `conditional` that routes numerically.
//!
//! Gated on `OPENAI_API_KEY` — skips (does not fail) when unset, so CI without a
//! key stays green.

use async_trait::async_trait;
use metalcraft_agent::flow_exec::{FlowExecutor, FlowRunSummary};
use metalcraft_agent::runtime::{AgentRuntimeContext, DEFAULT_MODEL};
use metalcraft_flows::{SavedFlow, validate};
use serde_json::{Value, json};
use std::sync::Arc;

/// A deterministic stand-in for a weather API: always returns the fixed temp it
/// was constructed with.
struct MockWeatherTool {
    temperature_f: i64,
}

#[async_trait]
impl metalcraft::Tool for MockWeatherTool {
    fn name(&self) -> &str {
        "weather"
    }
    fn description(&self) -> &str {
        "Get the current temperature in Fahrenheit for a city. Args: { \"city\": string }."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        })
    }
    async fn call(&self, _args: Value) -> metalcraft::Result<Value> {
        Ok(json!({ "temperature_f": self.temperature_f }))
    }
}

/// The Madrid flow: a persona-less `branch` (so no persona file is needed — the
/// weather tool is injected) whose typed outputs feed a numeric `conditional`
/// into three terminal `end` nodes.
fn madrid_flow() -> SavedFlow {
    let raw = r##"{
      "spec_version": "2",
      "id": "madrid-weather-test",
      "name": "Madrid weather (test)",
      "created_at": "2026-07-27T00:00:00Z",
      "updated_at": "2026-07-27T00:00:00Z",
      "enabled": false,
      "flow": {
        "nodes": [
          { "id": "entry", "node_type": "entry", "data": { "schedule_type": "manual" } },
          { "id": "get_temp", "node_type": "branch", "data": {
              "query": "Use the weather tool to look up the current temperature in Madrid, then report that integer temperature by calling report_temp. If the lookup fails, call error.",
              "outputs": [
                { "handle": "report_temp", "description": "Report the temperature that was retrieved", "schema": { "type": "integer", "description": "temperature in Fahrenheit" } },
                { "handle": "error", "description": "The temperature could not be determined", "schema": { "type": "string" } }
              ],
              "default_handle": "error"
          } },
          { "id": "check_hot", "node_type": "conditional", "data": {
              "conditions": [ { "handle": "hot", "variable": "_last", "operator": "gt", "value": 50 } ],
              "default_handle": "cold"
          } },
          { "id": "say_hot", "node_type": "end", "data": { "status": "hot" } },
          { "id": "say_cold", "node_type": "end", "data": { "status": "cold" } },
          { "id": "handle_err", "node_type": "end", "data": { "status": "err" } }
        ],
        "edges": [
          { "id": "e0", "source": "entry", "target": "get_temp" },
          { "id": "e1", "source": "get_temp", "target": "check_hot", "source_handle": "report_temp" },
          { "id": "e2", "source": "get_temp", "target": "handle_err", "source_handle": "error" },
          { "id": "e3", "source": "check_hot", "target": "say_hot", "source_handle": "hot" },
          { "id": "e4", "source": "check_hot", "target": "say_cold", "source_handle": "cold" }
        ]
      }
    }"##;
    let flow: SavedFlow = serde_json::from_str(raw).expect("flow parses");
    assert!(
        validate(&flow).is_empty(),
        "flow validates: {:?}",
        validate(&flow)
    );
    flow
}

async fn run_madrid(ctx: &AgentRuntimeContext, temperature_f: i64) -> FlowRunSummary {
    let mock: Arc<dyn metalcraft::Tool> = Arc::new(MockWeatherTool { temperature_f });
    FlowExecutor::new(
        ctx,
        madrid_flow(),
        ".",
        "coding-agent",
        DEFAULT_MODEL,
        &json!({}),
        None,
    )
    .expect("construct executor")
    .with_extra_tools(vec![mock])
    .run()
    .await
    .expect("run flow")
}

fn terminal_node(summary: &FlowRunSummary) -> &str {
    summary
        .steps
        .last()
        .map(|s| s.node_id.as_str())
        .unwrap_or("<none>")
}

#[tokio::test]
async fn madrid_branch_routes_by_reported_temperature() {
    // Load .env so a key stored there is honored, matching from_environment().
    dotenvy::dotenv().ok();
    if std::env::var("OPENAI_API_KEY")
        .map(|k| k.is_empty())
        .unwrap_or(true)
    {
        eprintln!("skipping madrid_branch_routes_by_reported_temperature: OPENAI_API_KEY not set");
        return;
    }
    let ctx = AgentRuntimeContext::from_environment().expect("runtime context");

    // Cold day: mock reports 18°F. The branch must call report_temp(18); the
    // conditional (18 > 50 == false) routes to say_cold.
    let cold = run_madrid(&ctx, 18).await;
    // Run status is the terminal end node's declared label ("cold"), not a blanket "completed".
    assert_eq!(cold.status, "cold", "trace: {:?}", cold.steps);
    assert_eq!(
        terminal_node(&cold),
        "say_cold",
        "expected cold route for 18°F; _last={}, trace={:?}",
        cold.variables.get("_last").cloned().unwrap_or(Value::Null),
        cold.steps
    );

    // Warm day: mock reports 75°F → report_temp(75) → 75 > 50 → say_hot.
    let hot = run_madrid(&ctx, 75).await;
    assert_eq!(hot.status, "hot", "trace: {:?}", hot.steps);
    assert_eq!(
        terminal_node(&hot),
        "say_hot",
        "expected hot route for 75°F; _last={}, trace={:?}",
        hot.variables.get("_last").cloned().unwrap_or(Value::Null),
        hot.steps
    );
}

/// A branch that cannot produce an answer must route its `error` rail — not
/// silently report success. Here the classifier has no tools and is told the
/// lookup is impossible, so it selects `error`; the edge wired from the `error`
/// handle carries the run to the error terminal.
fn unanswerable_flow() -> SavedFlow {
    let raw = r##"{
      "spec_version": "2",
      "id": "branch-error-rail-test",
      "name": "Branch error rail (test)",
      "created_at": "2026-07-30T00:00:00Z",
      "updated_at": "2026-07-30T00:00:00Z",
      "enabled": false,
      "flow": {
        "nodes": [
          { "id": "entry", "node_type": "entry", "data": { "schedule_type": "manual" } },
          { "id": "classify", "node_type": "branch", "data": {
              "query": "You have no tools and cannot look anything up, so the temperature is impossible to determine. Do not guess. Call the error tool with a brief reason.",
              "outputs": [
                { "handle": "report_temp", "description": "Report a temperature you actually determined", "schema": { "type": "integer" } },
                { "handle": "error", "description": "The temperature could not be determined", "schema": { "type": "string" } }
              ]
          } },
          { "id": "say_temp", "node_type": "end", "data": { "status": "ok" } },
          { "id": "handle_err", "node_type": "end", "data": { "status": "err" } }
        ],
        "edges": [
          { "id": "e0", "source": "entry", "target": "classify" },
          { "id": "e1", "source": "classify", "target": "say_temp", "source_handle": "report_temp" },
          { "id": "e2", "source": "classify", "target": "handle_err", "source_handle": "error" }
        ]
      }
    }"##;
    let flow: SavedFlow = serde_json::from_str(raw).expect("flow parses");
    assert!(
        validate(&flow).is_empty(),
        "flow validates: {:?}",
        validate(&flow)
    );
    flow
}

#[tokio::test]
async fn branch_routes_error_rail_when_unanswerable() {
    dotenvy::dotenv().ok();
    if std::env::var("OPENAI_API_KEY")
        .map(|k| k.is_empty())
        .unwrap_or(true)
    {
        eprintln!("skipping branch_routes_error_rail_when_unanswerable: OPENAI_API_KEY not set");
        return;
    }
    let ctx = AgentRuntimeContext::from_environment().expect("runtime context");

    let summary = FlowExecutor::new(
        &ctx,
        unanswerable_flow(),
        ".",
        "coding-agent",
        DEFAULT_MODEL,
        &json!({}),
        None,
    )
    .expect("construct executor")
    .run()
    .await
    .expect("run flow");

    // The run reaches the error terminal — it must NOT reach say_temp, and must
    // NOT report a non-error terminal as if the branch succeeded.
    assert_eq!(
        terminal_node(&summary),
        "handle_err",
        "expected error rail; _last={}, trace={:?}",
        summary
            .variables
            .get("_last")
            .cloned()
            .unwrap_or(Value::Null),
        summary.steps
    );
}
