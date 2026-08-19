//! Gateway spice harness — proves the agent behaves correctly on the **gateway**
//! runtime path: the one inbound WhatsApp / PipeStreamr messages actually take.
//!
//! The other spice suites (workshop/starflask/cloudflare/…) drive
//! `run_one_shot_task`, i.e. `RuntimeOptions::default()` — free-text output,
//! `ToolChoice::Auto`, no reply sink. A gateway turn is a *different* control
//! flow (`dispatch_inbound` → `run_chat_turn`):
//!
//!     RuntimeOptions {
//!         reply_sink: Some(adapter_send),         // text → pipestreamr/twilio
//!         tool_choice: ToolChoice::Required,      // tool-only output
//!         terminal_tools: ["say_to_user"],        // this call ends the turn
//!     }
//!
//! So a green workshop/starflask run says nothing about whether the gateway path
//! works. This suite reproduces that exact config with a *capturing* reply sink
//! and asserts the parts unique to it:
//!
//!   * the agent terminates by calling `say_to_user` (doesn't stall / doesn't
//!     try to "finish" with free text),
//!   * exactly **one** reply is delivered (no double-send),
//!   * the reply text reaches the bound sink (the real adapter, in production).
//!
//! Two tiers, one process (single `METALCRAFT_DATA_DIR`):
//!   1. Offline (always): the `say_to_user` tool delivers through / acks without
//!      a sink — the reply-delivery half, no network.
//!   2. `live_gateway_*` — gated on `OPENAI_API_KEY`; drives a real agentic loop
//!      on the gateway runtime config. Run:
//!
//!          cargo test --test gateway_spice_test -- --nocapture

use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

use async_trait::async_trait;
use spice_framework::agent::{
    AgentConfig, AgentOutput, AgentUnderTest, ToolCall as SpiceToolCall, Turn as SpiceTurn,
};
use spice_framework::error::SpiceError;
use spice_framework::{suite, test, Runner, RunnerConfig};

use rig::client::CompletionClient;

use metalcraft::{AgentState, Executor, RunOutcome, ToolChoice, Tool};
use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::guard::{build_agent_guard, GuardConfig};
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{build_agent_runtime, AgentRuntimeContext, RuntimeOptions, DEFAULT_MODEL};
use metalcraft_agent::tools::{say_to_user::SayToUserTool, ReplySink};
use metalcraft_agent::seed;

/// The persona a WhatsApp / PipeStreamr channel binds by default when its
/// `persona` setting is left blank (see
/// `seed/gateway_channels/pipestreamr/channel_type.json`: "Leave blank to use
/// the orchestrator-agent"). This is the real persona inbound WhatsApp messages
/// run today — the dedicated `whatsapp-agent` persona was removed when the
/// session paths were harmonized onto tool-only `say_to_user` output.
const PERSONA_SLUG: &str = "orchestrator-agent";

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-gateway-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches paths::data_dir(); guarded
        // by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();
        seed::ensure_defaults();
    });
}

/// Build a `ReplySink` that records every delivered message into `buf` — the
/// test-double for a real adapter send (`pipestreamr::send`/`twilio::send_whatsapp`).
fn capturing_sink(buf: Arc<tokio::sync::Mutex<Vec<String>>>) -> ReplySink {
    Arc::new(move |content: String| {
        let buf = buf.clone();
        Box::pin(async move {
            buf.lock().await.push(content);
            Ok(())
        })
    })
}

// ---------------------------------------------------------------------------
// Tier 1 — offline: the reply-delivery half of the gateway path, no LLM.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn say_to_user_delivers_through_sink() {
    init();
    let buf = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let tool = SayToUserTool::new(Some(capturing_sink(buf.clone())));

    let out = tool
        .call(serde_json::json!({ "message": "hello over whatsapp" }))
        .await
        .expect("say_to_user call");
    assert_eq!(out.get("delivered").and_then(|v| v.as_bool()), Some(true));

    let delivered = buf.lock().await;
    assert_eq!(delivered.as_slice(), &["hello over whatsapp".to_string()]);
}

#[tokio::test]
async fn say_to_user_acks_when_no_sink() {
    init();
    // One-shot/flow runs have no session sink: the tool must still succeed (ack)
    // rather than error, so personas that include it run safely off-gateway.
    let tool = SayToUserTool::new(None);
    let out = tool
        .call(serde_json::json!({ "message": "no sink here" }))
        .await
        .expect("say_to_user call");
    assert_eq!(out.get("delivered").and_then(|v| v.as_bool()), Some(false));
}

// ---------------------------------------------------------------------------
// Spice adapter: drive a persona on the *gateway* runtime config.
// ---------------------------------------------------------------------------

/// Reproduces what `dispatch_inbound` → `run_chat_turn` builds for an inbound
/// gateway message, but with a capturing sink so the test can inspect what was
/// delivered. The number of sink deliveries is surfaced back to the suite via
/// `AgentOutput::final_text` (the joined replies) so `.expect()` closures can
/// assert on the actually-delivered text, and `tools_called` exposes whether
/// `say_to_user` (the terminal tool) was the one that ended the turn.
struct GatewayPersonaAgent {
    context: AgentRuntimeContext,
    persona_slug: String,
    available_tools: Vec<String>,
    model_name: String,
    cwd: String,
    display_name: String,
}

impl GatewayPersonaAgent {
    fn for_persona(slug: &str) -> Result<Self, String> {
        let context = AgentRuntimeContext::from_environment().map_err(|e| e.to_string())?;
        let persona = Persona::load(slug, &context.personas_dir)?;
        Ok(Self {
            context,
            persona_slug: slug.to_string(),
            available_tools: persona.resolved_tool_names(),
            model_name: DEFAULT_MODEL.to_string(),
            cwd: ".".to_string(),
            display_name: format!("gateway:{slug}"),
        })
    }
}

#[async_trait]
impl AgentUnderTest for GatewayPersonaAgent {
    async fn run(&self, user_message: &str, _config: &AgentConfig) -> Result<AgentOutput, SpiceError> {
        let start = std::time::Instant::now();
        let delivered = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));

        // The exact options dispatch_inbound uses (workshop_api.rs), only the
        // sink differs (capture instead of adapter send).
        let options = RuntimeOptions {
            reply_sink: Some(capturing_sink(delivered.clone())),
            tool_choice: ToolChoice::Required,
            terminal_tools: vec!["say_to_user".to_string()],
            session_binding: None,
            reschedule_depth: 0,
            prompt_extras: metalcraft_agent::persona::PromptExtras::load().await,
            preset_personas: None,
            instance_id: None,
        };

        let persona = Persona::load(&self.persona_slug, &self.context.personas_dir)
            .map_err(|e| SpiceError::AgentError(e.to_string()))?;
        let runtime = build_agent_runtime(
            &self.context,
            &persona,
            &self.cwd,
            &self.model_name,
            ApprovalMode::AutoApprove,
            None,
            None,
            options,
            |client, name| client.completion_model(name),
        )
        .map_err(|e| SpiceError::AgentError(e.to_string()))?;

        let guard = build_agent_guard(GuardConfig::default(), None);
        let executor = Executor::new_from_arc(runtime.graph)
            .max_steps(90)
            .with_step_guard(guard);

        let outcome = executor
            .run(AgentState::new(user_message), "agent")
            .await
            .map_err(|e| SpiceError::AgentError(e.to_string()))?;

        let (state, error) = match outcome {
            RunOutcome::Completed(state) => (state, None),
            RunOutcome::Interrupted { state, reason, .. } => {
                (state, Some(format!("interrupted: {reason}")))
            }
            RunOutcome::Failed { state, node, error } => {
                (state, Some(format!("node `{node}` failed: {error}")))
            }
        };

        let turns = state
            .turns()
            .into_iter()
            .map(|t| SpiceTurn {
                index: t.index,
                output_text: t.assistant_text,
                tool_calls: t
                    .tool_calls
                    .into_iter()
                    .map(|c| SpiceToolCall { id: c.id, name: c.name, arguments: c.args })
                    .collect(),
                tool_results: t
                    .tool_results
                    .into_iter()
                    .map(|r| serde_json::json!({ "name": r.name, "result": r.result }))
                    .collect(),
                stop_reason: None,
                duration: Duration::ZERO,
            })
            .collect();

        // The user-facing reply IS what went to the sink — surface it as
        // final_text so suite assertions see what the channel would have sent.
        let replies = delivered.lock().await.clone();

        Ok(AgentOutput {
            final_text: replies.join("\n---\n"),
            turns,
            tools_called: state.tools_called(),
            duration: start.elapsed(),
            error,
        })
    }

    fn available_tools(&self, _config: &AgentConfig) -> Vec<String> {
        self.available_tools.clone()
    }

    fn name(&self) -> &str {
        &self.display_name
    }
}

// ---------------------------------------------------------------------------
// Tier 2 — live: a real gateway turn must reply via say_to_user exactly once.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_gateway_replies_once_via_say_to_user() {
    init();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!(
            "SKIP live_gateway_replies_once_via_say_to_user: set OPENAI_API_KEY \
             (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    let agent = GatewayPersonaAgent::for_persona(PERSONA_SLUG)
        .expect("build gateway agent under test");

    let tests = vec![
        test(
            "gateway-greeting",
            "Hi! Who are you and what can you help me with? Keep it short.",
        )
        .name("Gateway turn terminates with a single say_to_user reply")
        .expect_tools(&["say_to_user"])
        .expect_no_error()
        .expect(|out: &AgentOutput| {
            // The terminal tool must be the one that ended the turn, and it must
            // fire exactly once — a second say_to_user would be a double-send to
            // the user's phone.
            let says = out.tools_called.iter().filter(|t| *t == "say_to_user").count();
            if says != 1 {
                return Err(format!(
                    "expected exactly one say_to_user (the terminal reply), got {says}; \
                     tools_called = {:?}",
                    out.tools_called
                ));
            }
            // And the delivered reply (captured from the sink) must be non-empty —
            // proving the text actually reached the adapter.
            if out.final_text.trim().is_empty() {
                return Err("say_to_user fired but the sink received no text".to_string());
            }
            Ok(())
        })
        .build(),
        test(
            "gateway-simple-question",
            "What is 2 + 2? Reply with just the answer.",
        )
        .name("A trivial question still routes through say_to_user once")
        .expect_tools(&["say_to_user"])
        .expect_no_error()
        .expect(|out: &AgentOutput| {
            let says = out.tools_called.iter().filter(|t| *t == "say_to_user").count();
            if says == 1 {
                Ok(())
            } else {
                Err(format!(
                    "expected exactly one say_to_user, got {says}; tools_called = {:?}",
                    out.tools_called
                ))
            }
        })
        .build(),
    ];

    let suite = suite("Gateway persona replies over the tool-only path", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(180),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} gateway spice tests failed",
        report.failed, report.total
    );
}
