//! Spice test harness for the **sentry** integration pack.
//!
//! Two tiers, both run from one test binary (one process) so the
//! process-global `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set
//! exactly once and never raced:
//!
//!   1. `sentry_pack_wires_up` — always runs, no network. Seeds the bundled
//!      packs into an isolated data dir, enables `sentry`, loads the
//!      `sentry-agent` persona, and asserts every tool it references resolves
//!      to a parseable api-tool config that targets sentry.io and authenticates
//!      with `$SENTRY_AUTH_TOKEN`. Proves the pack is internally consistent
//!      without any network call.
//!
//!   2. `live_chat_uses_sentry_persona` — a real, gated [Spice] suite that drives
//!      an actual agentic loop (OpenAI LLM -> sentry HTTP tools -> live Sentry
//!      REST API) through the `sentry-agent` persona. Skipped unless
//!      `OPENAI_API_KEY`, `SENTRY_AUTH_TOKEN`, and `SENTRY_ORG_SLUG` are all
//!      present (drop them in a crate-root `.env`). Run:
//!
//!          cargo test --test sentry_spice_test -- --nocapture
//!
//!      The default assertions only read (list projects + query issues) — no
//!      writes. The mutating case (resolve an issue) additionally requires
//!      `SENTRY_SPICE_WRITE=1` and `SENTRY_SPICE_ISSUE=<numeric issue id>`
//!      pointing at an issue you don't mind toggling to resolved.
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

const PACK_ID: &str = "sentry";
const PERSONA_SLUG: &str = "sentry-agent";

/// The tools the persona declares — kept in lockstep with
/// `seed/integration_packs/sentry/personas/sentry-agent.json`.
const EXPECTED_TOOLS: &[&str] = &[
    "sentry_list_projects",
    "sentry_list_issues",
    "sentry_get_issue",
    "sentry_get_latest_event",
    "sentry_update_issue",
    "sentry_list_releases",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-sentry-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();

        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable sentry pack");
    });
}

fn live_keys_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok()
        && std::env::var("SENTRY_AUTH_TOKEN").is_ok()
        && std::env::var("SENTRY_ORG_SLUG").is_ok()
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
            // Non-interactive: no TTY to approve tool calls, and the sentry_*
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

// ---------------------------------------------------------------------------
// Tier 1 — offline: the pack is internally consistent.
// ---------------------------------------------------------------------------

#[test]
fn sentry_pack_wires_up() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "sentry pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("sentry-agent persona should resolve from the enabled pack");

    // The persona is scoped to the sentry pack rather than listing each tool.
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the sentry pack via `packs`"
    );
    // Its resolved tool set (explicit tools + pack-scoped tools) exposes every
    // sentry tool plus the native load_skill.
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
        persona.skills.iter().any(|s| s == "sentry-monitoring"),
        "persona should reference the sentry-monitoring skill"
    );

    // Every sentry tool the persona names resolves to a parseable api-tool
    // config in the pack, targets the Sentry API, and authenticates with the token.
    let api_tools_dir = paths::api_tools_dir();
    for tool in EXPECTED_TOOLS {
        let (path, _origin) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .unwrap_or_else(|| panic!("api tool `{tool}` should resolve from the pack"));
        let raw = std::fs::read_to_string(&path).expect("read api tool config");
        let cfg: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("api tool `{tool}` is not valid JSON: {e}"));
        assert_eq!(
            cfg["name"], *tool,
            "api tool `{tool}` config `name` should match its filename"
        );
        assert!(
            cfg["url"].as_str().is_some_and(|u| u.contains("sentry.io")),
            "api tool `{tool}` should target sentry.io"
        );
        // Org slug is baked into the URL via the configured env var, never a param.
        assert!(
            cfg["url"]
                .as_str()
                .is_some_and(|u| u.contains("$SENTRY_ORG_SLUG")),
            "api tool `{tool}` should bake in $SENTRY_ORG_SLUG"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$SENTRY_AUTH_TOKEN")),
            "api tool `{tool}` should authenticate with $SENTRY_AUTH_TOKEN"
        );
    }

    let recommended = integration_packs::recommended_env();
    for var in ["SENTRY_AUTH_TOKEN", "SENTRY_ORG_SLUG"] {
        assert!(
            recommended
                .iter()
                .any(|(v, packs)| v == var && packs.iter().any(|p| p == PACK_ID)),
            "sentry pack should recommend {var}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tier 2 — live: a real agentic loop through the persona (gated on keys).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_chat_uses_sentry_persona() {
    init();

    if !live_keys_present() {
        eprintln!(
            "SKIP live_chat_uses_sentry_persona: set OPENAI_API_KEY, SENTRY_AUTH_TOKEN and \
             SENTRY_ORG_SLUG (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    let agent =
        MetalcraftPersonaAgent::for_persona(PERSONA_SLUG).expect("build sentry-agent under test");

    // Read-only, non-destructive assertions.
    let mut tests = vec![
        test(
            "list-projects",
            "List the projects in my Sentry organization. Just give me their slugs.",
        )
        .name("Lists projects via sentry_list_projects")
        .expect_tools(&["sentry_list_projects"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test(
            "recent-issues",
            "Show me the most recent unresolved issues across my Sentry projects from the \
             last 24 hours.",
        )
        .name("Queries recent issues via sentry_list_issues")
        .expect_tools(&["sentry_list_issues"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test("greeting-no-tools", "Hi there! Just saying hello.")
            .name("A plain greeting calls no tools")
            .expect_no_tools()
            .expect_no_error()
            .build(),
    ];

    // Mutating case — resolves a real issue. Opt in explicitly and point it at an
    // issue you don't mind toggling.
    if std::env::var("SENTRY_SPICE_WRITE").is_ok() {
        if let Ok(issue_id) = std::env::var("SENTRY_SPICE_ISSUE") {
            tests.push(
                test(
                    "resolve-issue",
                    format!("Mark Sentry issue {issue_id} as resolved."),
                )
                .name("Resolves an issue via sentry_update_issue")
                .expect_tools(&["sentry_update_issue"])
                .expect_tools_within_allowlist()
                .expect_no_error()
                .build(),
            );
        } else {
            eprintln!(
                "NOTE: SENTRY_SPICE_WRITE is set but SENTRY_SPICE_ISSUE=<numeric issue id> is \
                 not — skipping the resolve-issue case."
            );
        }
    } else {
        eprintln!(
            "NOTE: set SENTRY_SPICE_WRITE=1 and SENTRY_SPICE_ISSUE=<numeric issue id> to also \
             run the write (resolve-issue) case."
        );
    }

    let suite = suite("Sentry persona", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 2,
        default_timeout: Duration::from_secs(180),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} sentry persona spice tests failed",
        report.failed, report.total
    );
}
