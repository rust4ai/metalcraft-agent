//! Spice test harness for the **github** integration pack.
//!
//! Two tiers, both run from one test binary (one process) so the
//! process-global `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set
//! exactly once and never raced:
//!
//!   1. `github_pack_wires_up` — always runs, no network. Seeds the bundled
//!      packs into an isolated data dir, enables `github`, loads the
//!      `github-agent` persona, and asserts every tool it references resolves
//!      to a parseable api-tool config that targets api.github.com and
//!      authenticates with `$GITHUB_TOKEN`. Proves the pack is internally
//!      consistent without any network call.
//!
//!   2. `live_chat_uses_github_persona` — a real, gated [Spice] suite that
//!      drives an actual agentic loop (OpenAI LLM -> github HTTP tools -> live
//!      GitHub REST API) through the `github-agent` persona. Skipped unless
//!      both `OPENAI_API_KEY` and `GITHUB_TOKEN` are present (drop them in a
//!      crate-root `.env`). Run:
//!
//!          cargo test --test github_spice_test -- --nocapture
//!
//!      The default assertions only read (whoami + list repos) — no writes. The
//!      mutating case (open an issue) additionally requires `GITHUB_SPICE_WRITE=1`
//!      and `GITHUB_SPICE_REPO=owner/repo` pointing at a repo you don't mind
//!      writing a test issue to.
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

const PACK_ID: &str = "github";
const PERSONA_SLUG: &str = "github-agent";

/// The tools the persona declares — kept in lockstep with
/// `seed/integration_packs/github/personas/github-agent.json`.
const EXPECTED_TOOLS: &[&str] = &[
    "github_get_authenticated_user",
    "github_list_repos",
    "github_get_repo",
    "github_get_file_contents",
    "github_list_branches",
    "github_get_ref",
    "github_create_branch",
    "github_create_or_update_file",
    "github_list_pull_requests",
    "github_create_pull_request",
    "github_list_issues",
    "github_create_issue",
    "github_create_issue_comment",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-github-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();

        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable github pack");
    });
}

fn live_keys_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("GITHUB_TOKEN").is_ok()
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
            // Non-interactive: no TTY to approve tool calls, and the github_*
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
fn github_pack_wires_up() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "github pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("github-agent persona should resolve from the enabled pack");

    // The persona is scoped to the github pack rather than listing each tool.
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the github pack via `packs`"
    );
    // Its resolved tool set (explicit tools + pack-scoped tools) exposes every
    // github tool plus the native load_skill.
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
        persona.skills.iter().any(|s| s == "github-ops"),
        "persona should reference the github-ops skill"
    );

    // Every github tool the persona names resolves to a parseable api-tool
    // config in the pack, targets the GitHub API, and authenticates with the PAT.
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
            cfg["url"].as_str().is_some_and(|u| u.contains("api.github.com")),
            "api tool `{tool}` should target api.github.com"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$GITHUB_TOKEN")),
            "api tool `{tool}` should authenticate with $GITHUB_TOKEN"
        );
    }

    let recommended = integration_packs::recommended_env();
    assert!(
        recommended
            .iter()
            .any(|(var, packs)| var == "GITHUB_TOKEN" && packs.iter().any(|p| p == PACK_ID)),
        "github pack should recommend GITHUB_TOKEN"
    );
}

// ---------------------------------------------------------------------------
// Tier 2 — live: a real agentic loop through the persona (gated on keys).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_chat_uses_github_persona() {
    init();

    if !live_keys_present() {
        eprintln!(
            "SKIP live_chat_uses_github_persona: set OPENAI_API_KEY and GITHUB_TOKEN \
             (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    let agent =
        MetalcraftPersonaAgent::for_persona(PERSONA_SLUG).expect("build github-agent under test");

    // Read-only, non-destructive assertions.
    let mut tests = vec![
        test(
            "whoami",
            "Which GitHub account am I authenticated as? Give me the login.",
        )
        .name("Identifies the account via github_get_authenticated_user")
        .expect_tools(&["github_get_authenticated_user"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test(
            "list-repos",
            "List a few of my GitHub repositories, including any private ones.",
        )
        .name("Lists repos via github_list_repos")
        .expect_tools(&["github_list_repos"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test("greeting-no-tools", "Hi there! Just saying hello.")
            .name("A plain greeting calls no tools")
            .expect_no_tools()
            .expect_no_error()
            .build(),
    ];

    // Mutating case — opens a real issue. Opt in explicitly and point it at a
    // repo you don't mind writing to.
    if std::env::var("GITHUB_SPICE_WRITE").is_ok() {
        if let Ok(repo) = std::env::var("GITHUB_SPICE_REPO") {
            tests.push(
                test(
                    "open-issue",
                    format!(
                        "In the GitHub repository {repo}, open an issue titled \
                         \"metalcraft spice test\" with a one-line body saying it was \
                         created by an automated test."
                    ),
                )
                .name("Opens an issue via github_create_issue")
                .expect_tools(&["github_create_issue"])
                .expect_tools_within_allowlist()
                .expect_no_error()
                .build(),
            );
        } else {
            eprintln!(
                "NOTE: GITHUB_SPICE_WRITE is set but GITHUB_SPICE_REPO=owner/repo is not — \
                 skipping the issue-creation case."
            );
        }
    } else {
        eprintln!(
            "NOTE: set GITHUB_SPICE_WRITE=1 and GITHUB_SPICE_REPO=owner/repo to also run the \
             write (open-issue) case."
        );
    }

    let suite = suite("GitHub persona", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 2,
        default_timeout: Duration::from_secs(180),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} github persona spice tests failed",
        report.failed, report.total
    );
}
