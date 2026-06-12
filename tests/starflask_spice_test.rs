//! Spice test harness for the **starflask** integration pack.
//!
//! Two tiers, both run from one test binary (one process) so the
//! process-global `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set
//! exactly once and never raced:
//!
//!   1. `starflask_pack_wires_up` — always runs, no network. Seeds the bundled
//!      packs into an isolated data dir, enables `starflask`, loads the
//!      `media-studio-agent` persona, and asserts every tool it references
//!      resolves to a parseable api-tool config. This proves the pack is
//!      internally consistent without spending a cent.
//!
//!   2. `live_chat_uses_starflask_persona` — a real, gated [Spice] suite that
//!      drives an actual agentic loop (OpenAI LLM -> starflask HTTP tools ->
//!      live starflask.com API) through the `media-studio-agent` persona and
//!      asserts which tools the agent calls. It is **skipped** unless both
//!      `OPENAI_API_KEY` and `STARFLASK_API_KEY` are present — drop them in a
//!      `.env` at the crate root (the harness loads it via dotenvy) and run:
//!
//!          cargo test --test starflask_spice_test -- --nocapture
//!
//!      The default live assertions only exercise cheap, credit-free GET
//!      endpoints (account / list-models). The credit-spending
//!      image-generation case additionally requires `STARFLASK_SPICE_GENERATE=1`.
//!
//! [Spice]: https://crates.io/crates/spice-framework

use std::sync::Once;
use std::time::Duration;

use async_trait::async_trait;
use spice_framework::agent::{
    AgentConfig, AgentOutput, AgentUnderTest, ToolCall as SpiceToolCall, Turn as SpiceTurn,
};
use spice_framework::error::SpiceError;
use spice_framework::{suite, test, Runner, RunnerConfig};

use metalcraft::RunOutcome;
use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{run_one_shot_task, AgentRuntimeContext, RunOneShotRequest};
use metalcraft_agent::{integration_packs, paths, seed};

const PACK_ID: &str = "starflask";
const PERSONA_SLUG: &str = "media-studio-agent";

/// The tools the persona declares — kept in lockstep with
/// `seed/integration_packs/starflask/personas/media-studio-agent.json`.
const EXPECTED_TOOLS: &[&str] = &[
    "starflask_generate_image",
    "starflask_edit_image",
    "starflask_generate_video",
    "starflask_generate_3d",
    "starflask_generate_speech",
    "starflask_create_job",
    "starflask_get_job",
    "starflask_list_models",
    "starflask_list_styles",
    "starflask_upload_media",
    "starflask_get_media",
    "starflask_account",
];

static INIT: Once = Once::new();

/// Point the app at an isolated temp data dir, load `.env`, seed the bundled
/// integration packs into it, and enable the starflask pack. Runs once per
/// process (the `paths::data_dir()` `OnceLock` memoizes the dir on first use,
/// so `METALCRAFT_DATA_DIR` must be set before anything calls it).
fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-starflask-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        // Load OPENAI_API_KEY / STARFLASK_API_KEY from a crate-root .env if present.
        // (Does not override vars already exported in the environment.)
        dotenvy::dotenv().ok();

        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable starflask pack");
    });
}

/// True only when the keys needed for a real agentic loop are available.
fn live_keys_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("STARFLASK_API_KEY").is_ok()
}

// ---------------------------------------------------------------------------
// Spice adapter: drive the real metalcraft persona agent as an AgentUnderTest.
// ---------------------------------------------------------------------------

struct MetalcraftPersonaAgent {
    context: AgentRuntimeContext,
    persona_slug: String,
    available_tools: Vec<String>,
    model_name: String,
    cwd: String,
    display_name: String,
}

impl MetalcraftPersonaAgent {
    /// Build an agent bound to `slug`. Requires `OPENAI_API_KEY` (via
    /// `AgentRuntimeContext::from_environment`) — only call when keys are present.
    fn for_persona(slug: &str) -> Result<Self, String> {
        let context = AgentRuntimeContext::from_environment().map_err(|e| e.to_string())?;
        let persona = Persona::load(slug, &context.personas_dir)?;
        Ok(Self {
            context,
            persona_slug: slug.to_string(),
            available_tools: persona.resolved_tool_names(),
            model_name: metalcraft_agent::runtime::DEFAULT_MODEL.to_string(),
            cwd: ".".to_string(),
            display_name: format!("metalcraft:{slug}"),
        })
    }
}

#[async_trait]
impl AgentUnderTest for MetalcraftPersonaAgent {
    async fn run(
        &self,
        user_message: &str,
        _config: &AgentConfig,
    ) -> Result<AgentOutput, SpiceError> {
        let start = std::time::Instant::now();

        let request = RunOneShotRequest {
            persona_slug: &self.persona_slug,
            cwd: &self.cwd,
            model_name: &self.model_name,
            task: user_message,
            // Non-interactive: there is no TTY to approve tool calls, and the
            // starflask_* tools classify as `Execute` (would otherwise block).
            approval_mode: ApprovalMode::AutoApprove,
            diagnostics: None,
        };

        let outcome = run_one_shot_task(&self.context, request)
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
                    .map(|c| SpiceToolCall {
                        id: c.id,
                        name: c.name,
                        arguments: c.args,
                    })
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

        Ok(AgentOutput {
            final_text: state.final_answer().unwrap_or_default().to_string(),
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
// Tier 1 — offline: the pack is internally consistent.
// ---------------------------------------------------------------------------

#[test]
fn starflask_pack_wires_up() {
    init();

    // The pack is installed and enabled.
    assert!(
        integration_packs::is_enabled(PACK_ID),
        "starflask pack should be enabled after init()"
    );

    // The persona resolves from the (enabled) pack and declares exactly the
    // tools we expect, plus the native load_skill, plus its skill.
    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("media-studio-agent persona should resolve from the enabled pack");

    // The persona is scoped to the starflask pack rather than listing each tool.
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the starflask pack via `packs`"
    );
    // Its resolved tool set (explicit tools + pack-scoped tools) exposes every
    // starflask tool plus the native load_skill.
    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "persona's resolved tools are missing expected tool `{tool}`"
        );
    }
    assert!(
        resolved.iter().any(|t| t == "load_skill"),
        "persona should include the native load_skill tool"
    );
    assert!(
        persona.skills.iter().any(|s| s == "starflask-media"),
        "persona should reference the starflask-media skill"
    );

    // Every starflask tool the persona names resolves to a parseable api-tool
    // config in the pack — i.e. the agent will actually be able to load them.
    let api_tools_dir = paths::api_tools_dir();
    for tool in EXPECTED_TOOLS {
        let (path, _origin) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .unwrap_or_else(|| panic!("api tool `{tool}` should resolve from the pack"));
        let raw = std::fs::read_to_string(&path).expect("read api tool config");
        let cfg: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("api tool `{tool}` is not valid JSON: {e}"));
        assert_eq!(
            cfg["name"], *tool,
            "api tool `{tool}` config `name` should match its filename"
        );
        assert!(
            cfg["url"].as_str().is_some_and(|u| u.contains("starflask.com")),
            "api tool `{tool}` should target starflask.com"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$STARFLASK_API_KEY")),
            "api tool `{tool}` should authenticate with $STARFLASK_API_KEY"
        );
    }

    // The pack declares the API key it needs. recommended_env() is keyed by
    // env-var name, with the value listing the packs that recommend it.
    let recommended = integration_packs::recommended_env();
    assert!(
        recommended
            .iter()
            .any(|(var, packs)| var == "STARFLASK_API_KEY" && packs.iter().any(|p| p == PACK_ID)),
        "starflask pack should recommend STARFLASK_API_KEY"
    );
}

// ---------------------------------------------------------------------------
// Tier 2 — live: a real agentic loop through the persona (gated on keys).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_chat_uses_starflask_persona() {
    init();

    if !live_keys_present() {
        eprintln!(
            "SKIP live_chat_uses_starflask_persona: set OPENAI_API_KEY and \
             STARFLASK_API_KEY (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    let agent = MetalcraftPersonaAgent::for_persona(PERSONA_SLUG)
        .expect("build media-studio-agent under test");

    // Cheap, credit-free assertions: these hit only GET endpoints on starflask.
    let mut tests = vec![
        test(
            "account-balance",
            "How many Starflask credits do I have left on my account?",
        )
        .name("Checks the account via starflask_account")
        .expect_tools(&["starflask_account"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test(
            "list-image-models",
            "Which image generation models can I use? List the available models.",
        )
        .name("Lists models via starflask_list_models")
        .expect_tools(&["starflask_list_models"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test("greeting-no-tools", "Hi there! Just saying hello.")
            .name("A plain greeting calls no tools")
            .expect_no_tools()
            .expect_no_error()
            .build(),
    ];

    // Credit-spending image generation — opt in explicitly. We assert the agent
    // successfully *creates* a generation job (the core capability); whether the
    // async job has finished rendering by the end of the turn depends on
    // generation latency, so we don't require completion here.
    if std::env::var("STARFLASK_SPICE_GENERATE").is_ok() {
        tests.push(
            test(
                "generate-image",
                "Generate an image of a tiny red fox sitting on a mushroom.",
            )
            .name("Creates an image generation job via starflask_generate_image")
            .expect_tools(&["starflask_generate_image"])
            .expect_tools_within_allowlist()
            .expect(|out| {
                // The agent can only poll a job it successfully created (the
                // job_id comes from a 200 create response), so a follow-up
                // starflask_get_job call is escaping-independent proof that
                // image-job creation succeeded.
                if out.tools_called.iter().any(|t| t == "starflask_get_job") {
                    Ok(())
                } else {
                    Err("agent never polled a job id — image job creation did not succeed".into())
                }
            })
            .build(),
        );
    } else {
        eprintln!(
            "NOTE: set STARFLASK_SPICE_GENERATE=1 to also run the credit-spending \
             image-generation case."
        );
    }

    let suite = suite("Starflask Media Studio persona", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 2,
        // Live LLM + media-API round trips: be generous vs. the 60s default.
        default_timeout: Duration::from_secs(180),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} starflask persona spice tests failed",
        report.failed, report.total
    );
}
