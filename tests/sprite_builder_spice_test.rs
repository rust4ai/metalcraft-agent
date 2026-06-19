//! Spice test harness for the **sprite_builder** integration pack.
//!
//! Two tiers, both run from one test binary (one process) so the
//! process-global `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set
//! exactly once and never raced:
//!
//!   1. `sprite_builder_pack_wires_up` — always runs, no network. Seeds the
//!      bundled packs into an isolated data dir, enables `sprite_builder`, loads
//!      the `sprite-builder-agent` persona, and asserts every tool it references
//!      resolves to a parseable api-tool config that targets
//!      `$SPRITE_BUILDER_BASE_URL` and authenticates with
//!      `$SPRITE_BUILDER_API_KEY`. Also checks the three per-facet skills resolve
//!      and the async status tools are flagged `poll`. Proves the pack is
//!      internally consistent without any network call.
//!
//!   2. `live_chat_uses_sprite_builder_persona` — a real, gated [Spice] suite
//!      that drives an actual agentic loop (OpenAI LLM -> sprite_builder HTTP
//!      tools -> a live Sprite Builder instance) through the
//!      `sprite-builder-agent` persona. Skipped unless `OPENAI_API_KEY`,
//!      `SPRITE_BUILDER_API_KEY`, and `SPRITE_BUILDER_BASE_URL` are all present
//!      (drop them in a crate-root `.env`). Run:
//!
//!          cargo test --test sprite_builder_spice_test -- --nocapture
//!
//!      The default assertions only read (list projects, list repos) — no builds
//!      or deletes. The mutating cases (trigger a build, create a docuspace)
//!      additionally require `SPRITE_BUILDER_SPICE_WRITE=1` and
//!      `SPRITE_BUILDER_SPICE_PROJECT=<project id>` pointing at a project you
//!      don't mind deploying / adding a docuspace to.
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

const PACK_ID: &str = "sprite_builder";
const PERSONA_SLUG: &str = "sprite-builder-agent";

/// The tools the persona exposes through the pack — kept in lockstep with
/// `seed/integration_packs/sprite_builder/api_tools/*.json`.
const EXPECTED_TOOLS: &[&str] = &[
    // projects (shared)
    "sprite_builder_list_projects",
    "sprite_builder_get_project",
    "sprite_builder_list_repos",
    "sprite_builder_create_project",
    // facet 1: builds + env
    "sprite_builder_create_build",
    "sprite_builder_get_build",
    "sprite_builder_list_builds",
    "sprite_builder_get_runtime_logs",
    "sprite_builder_set_build_visibility",
    "sprite_builder_list_env",
    "sprite_builder_set_env",
    "sprite_builder_delete_env",
    // facet 2: codespaces
    "sprite_builder_create_codespace",
    "sprite_builder_list_codespaces",
    "sprite_builder_get_codespace",
    "sprite_builder_delete_codespace",
    "sprite_builder_codespace_clone",
    "sprite_builder_codespace_read",
    "sprite_builder_codespace_write",
    "sprite_builder_codespace_delete_path",
    "sprite_builder_codespace_exec",
    "sprite_builder_codespace_git",
    // facet 3: docuspaces
    "sprite_builder_create_docuspace",
    "sprite_builder_list_docuspaces",
    "sprite_builder_get_docuspace",
    "sprite_builder_delete_docuspace",
    "sprite_builder_docuspace_read",
    "sprite_builder_docuspace_write",
    "sprite_builder_docuspace_delete_path",
    "sprite_builder_docuspace_create_folder",
];

/// The per-facet skills the persona consults.
const EXPECTED_SKILLS: &[&str] = &[
    "sprite-builder-builds",
    "sprite-builder-codespaces",
    "sprite-builder-docuspaces",
];

/// Async status tools that must be flagged `poll` so the loop guard doesn't
/// treat intentional polling as a runaway loop.
const POLL_TOOLS: &[&str] = &[
    "sprite_builder_get_build",
    "sprite_builder_get_codespace",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-sprite-builder-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();

        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable sprite_builder pack");
    });
}

fn live_keys_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok()
        && std::env::var("SPRITE_BUILDER_API_KEY").is_ok()
        && std::env::var("SPRITE_BUILDER_BASE_URL").is_ok()
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
            // Non-interactive: no TTY to approve tool calls, and the
            // sprite_builder_* tools classify as `Execute` (would otherwise block).
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
fn sprite_builder_pack_wires_up() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "sprite_builder pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("sprite-builder-agent persona should resolve from the enabled pack");

    // The persona is scoped to the pack rather than listing each tool.
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the sprite_builder pack via `packs`"
    );

    // Its resolved tool set (explicit tools + pack-scoped tools) exposes every
    // sprite_builder tool plus the native load_skill.
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
    for skill in EXPECTED_SKILLS {
        assert!(
            persona.skills.iter().any(|s| s == skill),
            "persona should reference the `{skill}` skill"
        );
    }

    // Every tool the persona names resolves to a parseable api-tool config that
    // targets the configured instance and authenticates with the API key.
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
        let url = cfg["url"].as_str().unwrap_or_default();
        assert!(
            url.contains("$SPRITE_BUILDER_BASE_URL"),
            "api tool `{tool}` should target $SPRITE_BUILDER_BASE_URL (got `{url}`)"
        );
        assert!(
            url.contains("/api/"),
            "api tool `{tool}` URL should hit the /api/ surface (got `{url}`)"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("Bearer $SPRITE_BUILDER_API_KEY")),
            "api tool `{tool}` should authenticate with `Bearer $SPRITE_BUILDER_API_KEY`"
        );

        // Mutating verbs send a JSON body; reads must not.
        let method = cfg["method"].as_str().unwrap_or_default();
        let mapping = cfg["body_mapping"].as_str().unwrap_or("params");
        match method {
            "GET" | "DELETE" => assert_eq!(
                mapping, "none",
                "read/delete tool `{tool}` ({method}) should use body_mapping `none`"
            ),
            "POST" | "PUT" | "PATCH" => assert_eq!(
                mapping, "params",
                "mutating tool `{tool}` ({method}) should use body_mapping `params`"
            ),
            other => panic!("api tool `{tool}` has unexpected method `{other}`"),
        }
    }

    // The async status tools must be flagged `poll`.
    for tool in POLL_TOOLS {
        let (path, _origin) =
            integration_packs::resolve_file(&api_tools_dir, "api_tools", &format!("{tool}.json"))
                .unwrap_or_else(|| panic!("poll tool `{tool}` should resolve"));
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cfg["poll"], serde_json::Value::Bool(true),
            "async status tool `{tool}` should be flagged `poll: true`"
        );
    }

    // The pack recommends both env keys.
    let recommended = integration_packs::recommended_env();
    for var in ["SPRITE_BUILDER_API_KEY", "SPRITE_BUILDER_BASE_URL"] {
        assert!(
            recommended
                .iter()
                .any(|(v, packs)| v == var && packs.iter().any(|p| p == PACK_ID)),
            "sprite_builder pack should recommend {var}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tier 2 — live: a real agentic loop through the persona (gated on keys).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_chat_uses_sprite_builder_persona() {
    init();

    if !live_keys_present() {
        eprintln!(
            "SKIP live_chat_uses_sprite_builder_persona: set OPENAI_API_KEY, \
             SPRITE_BUILDER_API_KEY and SPRITE_BUILDER_BASE_URL (e.g. in a crate-root .env) \
             to run the live suite."
        );
        return;
    }

    let agent = MetalcraftPersonaAgent::for_persona(PERSONA_SLUG)
        .expect("build sprite-builder-agent under test");

    // Read-only, non-destructive assertions — verify facet/tool routing.
    let mut tests = vec![
        test(
            "list-projects",
            "List my Sprite Builder projects. Just give me their names and ids.",
        )
        .name("Lists projects via sprite_builder_list_projects")
        .expect_tools(&["sprite_builder_list_projects"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test(
            "list-repos",
            "Which of my GitHub repositories could I turn into a new Sprite Builder project?",
        )
        .name("Discovers repos via sprite_builder_list_repos")
        .expect_tools(&["sprite_builder_list_repos"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test(
            "greeting-no-tools",
            "Hi there! Just saying hello, no need to do anything.",
        )
        .name("A plain greeting calls no tools")
        .expect_no_tools()
        .expect_no_error()
        .build(),
    ];

    // Mutating / stateful cases — opt in explicitly and point them at a project
    // you don't mind deploying and adding a docuspace to. These exercise facet
    // routing for Builds and Docuspaces against real state.
    match (
        std::env::var("SPRITE_BUILDER_SPICE_WRITE"),
        std::env::var("SPRITE_BUILDER_SPICE_PROJECT"),
    ) {
        (Ok(_), Ok(project_id)) => {
            tests.push(
                test(
                    "trigger-build",
                    format!(
                        "Deploy the latest commit of Sprite Builder project {project_id} to a \
                         live URL."
                    ),
                )
                .name("Routes to Builds — triggers via sprite_builder_create_build")
                .expect_tools(&["sprite_builder_create_build"])
                .expect_tools_within_allowlist()
                .expect_no_error()
                .build(),
            );
            tests.push(
                test(
                    "create-docuspace",
                    format!(
                        "I want an S3-backed file store (no running server) for Sprite Builder \
                         project {project_id}. Set one up."
                    ),
                )
                .name("Routes to Docuspaces — creates via sprite_builder_create_docuspace")
                .expect_tools(&["sprite_builder_create_docuspace"])
                .expect_tools_within_allowlist()
                .expect_no_error()
                .build(),
            );
        }
        _ => {
            eprintln!(
                "NOTE: set SPRITE_BUILDER_SPICE_WRITE=1 and \
                 SPRITE_BUILDER_SPICE_PROJECT=<project id> to also run the write cases \
                 (trigger-build, create-docuspace)."
            );
        }
    }

    let suite = suite("Sprite Builder persona", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 2,
        default_timeout: Duration::from_secs(240),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} sprite_builder persona spice tests failed",
        report.failed, report.total
    );
}
