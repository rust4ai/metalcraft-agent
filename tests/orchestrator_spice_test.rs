//! Spice test harness proving the **orchestrator** persona can drive the
//! **starflask** integration by delegation.
//!
//! The orchestrator never calls starflask tools directly — it has only
//! read-only tools plus `sub_agent` / `load_skill`. Instead it spawns a
//! sub-agent with `tool_set: "all"`, which grants every installed integration
//! tool (the `starflask_*` media tools) on top of the full file/exec set. So
//! "use starflask to generate an image of a cloud" works under the
//! orchestrator persona via a delegated sub-agent.
//!
//! Two tiers, both in one test binary (one process) so the process-global
//! `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set exactly once:
//!
//!   1. `orchestrator_can_delegate_starflask` — always runs, no network. Seeds
//!      the bundled packs into an isolated data dir, enables `starflask`, loads
//!      the `orchestrator-agent` persona, and proves (a) the orchestrator
//!      delegates (has `sub_agent`, does not itself declare starflask tools)
//!      and (b) a sub-agent built with `tool_set: "all"` — assembled exactly
//!      as `sub_agent` assembles it — actually registers the `starflask_*`
//!      tools. That is deterministic proof the delegation path reaches
//!      starflask, without spending a cent.
//!
//!   2. `live_orchestrator_delegates_to_starflask` — a real, gated [Spice]
//!      suite that drives an actual agentic loop (OpenAI LLM -> orchestrator ->
//!      sub_agent("all") -> live starflask API). Skipped unless both
//!      `OPENAI_API_KEY` and `STARFLASK_API_KEY` are present. The default case
//!      hits only the credit-free list-models endpoint; the credit-spending
//!      image-generation case ("an image of a cloud") additionally requires
//!      `STARFLASK_SPICE_GENERATE=1`. Run:
//!
//!          cargo test --test orchestrator_spice_test -- --nocapture
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
use metalcraft_agent::tools::http_api::HttpApiTool;
use metalcraft_agent::{integration_packs, paths, seed, tools};

const PACK_ID: &str = "starflask";
const PACK_ID_GITHUB: &str = "github";
const PERSONA_SLUG: &str = "orchestrator-agent";

/// A couple of github pack tools — used to prove pack-scoping excludes other
/// integrations and includes the targeted one.
const GITHUB_TOOLS: &[&str] = &["github_list_repos", "github_create_issue"];

/// Starflask tools shipped by the pack — a delegated `tool_set: "all"`
/// sub-agent must be able to reach these.
const STARFLASK_TOOLS: &[&str] = &[
    "starflask_generate_image",
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

/// The base file/exec tools `sub_agent` grants for `tool_set` "full" and "all".
/// Kept in lockstep with `src/tools/sub_agent.rs`.
const FULL_BASE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "bash",
    "list_files",
    "grep",
    "find_files",
];

static INIT: Once = Once::new();

/// Point the app at an isolated temp data dir, load `.env`, seed the bundled
/// packs into it, and enable the starflask pack. Runs once per process.
fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-orchestrator-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();

        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable starflask pack");
        // A second integration is enabled too, so the pack-scoping test has more
        // than one set of integration tools to distinguish between.
        integration_packs::set_enabled(PACK_ID_GITHUB, true).expect("enable github pack");
    });
}

fn live_keys_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("STARFLASK_API_KEY").is_ok()
}

/// Reassemble the tool list a `sub_agent` call with `tool_set: "all"` builds:
/// the full file/exec base plus every installed integration tool.
fn all_tool_set() -> Vec<String> {
    let mut names: Vec<String> = FULL_BASE_TOOLS.iter().map(|s| s.to_string()).collect();
    for name in HttpApiTool::installed_tool_names() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
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
            // Non-interactive: no TTY to approve tool calls, and the starflask_*
            // tools classify as `Execute` (would otherwise block).
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

/// Collect the tool names a sub-agent reported using, by scanning the parent's
/// `sub_agent` tool results (`{ "result": { "tools_used": [...] } }`). This is
/// how we observe the *delegated* starflask calls — they never appear in the
/// orchestrator's own top-level `tools_called`.
fn delegated_tools_used(out: &AgentOutput) -> Vec<String> {
    let mut used = Vec::new();
    for turn in &out.turns {
        for tr in &turn.tool_results {
            if tr.get("name").and_then(|n| n.as_str()) != Some("sub_agent") {
                continue;
            }
            // `result` is a String (the sub_agent tool's serialized JSON
            // payload), so parse it before reading `tools_used`.
            let Some(payload) = tr.get("result").and_then(|r| r.as_str()) else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            if let Some(arr) = parsed.get("tools_used").and_then(|t| t.as_array()) {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        used.push(s.to_string());
                    }
                }
            }
        }
    }
    used
}

// ---------------------------------------------------------------------------
// Tier 1 — offline: the orchestrator's delegation path can reach starflask.
// ---------------------------------------------------------------------------

#[test]
fn orchestrator_can_delegate_starflask() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "starflask pack should be enabled after init()"
    );

    // The orchestrator persona resolves and is wired to delegate.
    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("orchestrator-agent persona should resolve");
    assert!(
        persona.tools.iter().any(|t| t == "sub_agent"),
        "orchestrator must have the sub_agent tool to delegate"
    );
    // It delegates rather than calling integration tools itself: no starflask_*
    // tool is declared directly on the orchestrator.
    for tool in STARFLASK_TOOLS {
        assert!(
            !persona.tools.iter().any(|t| t == tool),
            "orchestrator should NOT declare `{tool}` directly — it delegates via sub_agent"
        );
    }

    // The "all" tool set (what sub_agent grants for tool_set:"all") enumerates
    // every installed integration tool, so each starflask tool is present.
    let all = all_tool_set();
    for tool in STARFLASK_TOOLS {
        assert!(
            all.iter().any(|t| t == tool),
            "tool_set `all` is missing starflask tool `{tool}` — installed_tool_names() did not surface it"
        );
    }

    // Build the registry exactly as a tool_set:"all" sub-agent would, and prove
    // the starflask tools actually register (resolve to loadable api-tools).
    let registry = tools::create_registry_for(&all);
    let registered = registry.names();
    for tool in STARFLASK_TOOLS {
        assert!(
            registered.iter().any(|t| *t == *tool),
            "a tool_set:\"all\" sub-agent failed to register starflask tool `{tool}`"
        );
    }
}

/// Collect the `persona` argument from every `sub_agent` call the orchestrator
/// made — proof that it delegated *as a named persona* rather than via a raw
/// tool_set.
fn delegated_personas(out: &AgentOutput) -> Vec<String> {
    let mut personas = Vec::new();
    for turn in &out.turns {
        for call in &turn.tool_calls {
            if call.name != "sub_agent" {
                continue;
            }
            if let Some(p) = call.arguments.get("persona").and_then(|v| v.as_str()) {
                if !p.is_empty() {
                    personas.push(p.to_string());
                }
            }
        }
    }
    personas
}

// ---------------------------------------------------------------------------
// Tier 1b — offline: tool_set "all" can be scoped to a single pack.
// ---------------------------------------------------------------------------

#[test]
fn tool_set_all_scopes_to_one_pack() {
    init();

    // Unscoped "all" sees every installed integration tool (both packs).
    let all = HttpApiTool::installed_tool_names();
    for tool in STARFLASK_TOOLS {
        assert!(
            all.iter().any(|t| t == tool),
            "unscoped all should include starflask tool `{tool}`"
        );
    }
    for tool in GITHUB_TOOLS {
        assert!(
            all.iter().any(|t| t == tool),
            "unscoped all should include github tool `{tool}`"
        );
    }

    // Scoped to starflask: starflask tools present, github tools excluded.
    let only_starflask = HttpApiTool::installed_tool_names_for_pack(PACK_ID);
    for tool in STARFLASK_TOOLS {
        assert!(
            only_starflask.iter().any(|t| t == tool),
            "starflask-scoped set is missing `{tool}`"
        );
    }
    for tool in GITHUB_TOOLS {
        assert!(
            !only_starflask.iter().any(|t| t == tool),
            "starflask-scoped set should NOT include github tool `{tool}`"
        );
    }

    // Scoped to github: github tools present, starflask tools excluded.
    let only_github = HttpApiTool::installed_tool_names_for_pack(PACK_ID_GITHUB);
    for tool in GITHUB_TOOLS {
        assert!(
            only_github.iter().any(|t| t == tool),
            "github-scoped set is missing `{tool}`"
        );
    }
    for tool in STARFLASK_TOOLS {
        assert!(
            !only_github.iter().any(|t| t == tool),
            "github-scoped set should NOT include starflask tool `{tool}`"
        );
    }

    // An unknown or disabled pack scopes to no integration tools.
    assert!(
        HttpApiTool::installed_tool_names_for_pack("does-not-exist").is_empty(),
        "an unknown pack should yield no tools"
    );
}

// ---------------------------------------------------------------------------
// Tier 2 — live: orchestrator delegates a real starflask call (gated on keys).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_orchestrator_delegates_to_starflask() {
    init();

    if !live_keys_present() {
        eprintln!(
            "SKIP live_orchestrator_delegates_to_starflask: set OPENAI_API_KEY and \
             STARFLASK_API_KEY (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    let agent =
        MetalcraftPersonaAgent::for_persona(PERSONA_SLUG).expect("build orchestrator under test");

    // Credit-free default: list image models. The orchestrator must delegate
    // (call sub_agent), and the delegated sub-agent must call a starflask tool.
    let mut tests = vec![
        test(
            "delegate-list-models",
            "Use starflask to list the available image generation models.",
        )
        .name("Orchestrator delegates to the media-studio-agent persona")
        .expect_tools(&["sub_agent"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .expect(|out| {
            // It should delegate AS the starflask persona...
            let personas = delegated_personas(out);
            if !personas.iter().any(|p| p == "media-studio-agent") {
                return Err(format!(
                    "orchestrator did not delegate to the media-studio-agent persona \
                     (sub_agent persona args: {personas:?})"
                ));
            }
            // ...and that persona-scoped sub-agent must actually call a starflask tool.
            let used = delegated_tools_used(out);
            if used.iter().any(|t| t.starts_with("starflask_")) {
                Ok(())
            } else {
                Err(format!(
                    "no delegated starflask_* tool call observed (sub-agent tools_used: {used:?})"
                ))
            }
        })
        .build(),
        test("greeting-no-tools", "Hi there! Just saying hello.")
            .name("A plain greeting delegates nothing")
            .expect_no_tools()
            .expect_no_error()
            .build(),
    ];

    // Credit-spending image generation — the user's example. Opt in explicitly.
    if std::env::var("STARFLASK_SPICE_GENERATE").is_ok() {
        tests.push(
            test(
                "delegate-generate-cloud",
                "Use starflask to generate an image of a cloud.",
            )
            .name("Orchestrator delegates a starflask image-generation job")
            .expect_tools(&["sub_agent"])
            .expect_tools_within_allowlist()
            .expect(|out| {
                let used = delegated_tools_used(out);
                if used.iter().any(|t| t == "starflask_generate_image") {
                    Ok(())
                } else {
                    Err(format!(
                        "sub-agent never called starflask_generate_image (tools_used: {used:?})"
                    ))
                }
            })
            .build(),
        );
    } else {
        eprintln!(
            "NOTE: set STARFLASK_SPICE_GENERATE=1 to also run the credit-spending \
             'image of a cloud' generation case."
        );
    }

    let suite = suite("Orchestrator delegates to Starflask", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 2,
        // Orchestrator -> sub-agent -> live LLM + media-API round trips: be
        // generous (a delegated turn nests a full sub-agent run).
        default_timeout: Duration::from_secs(240),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} orchestrator delegation spice tests failed",
        report.failed, report.total
    );
}
