//! Spice test harness proving the **workshop** persona can author and inspect
//! the metalcraft project itself (personas, skills, flows) via the meta tools —
//! the prompt-driven equivalent of the metalcraft-workshop GUI.
//!
//! Two tiers, both in one test binary (one process) so the process-global
//! `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set exactly once:
//!
//!   1. Offline (always runs, no network): the `workshop-agent` persona
//!      resolves, registers the meta tools, and each meta tool's `call()`
//!      round-trips against an isolated data dir (skill/persona write lands on
//!      disk and reloads; flow_validate flags a bad flow).
//!
//!   2. `live_workshop_authors_a_skill` — a gated [Spice] suite that drives a
//!      real agentic loop (OpenAI LLM -> workshop-agent -> skill_write). Skipped
//!      unless `OPENAI_API_KEY` is present. Run:
//!
//!          cargo test --test workshop_spice_test -- --nocapture
//!
//! [Spice]: https://crates.io/crates/spice-framework

use std::sync::Once;
use std::time::Duration;

use async_trait::async_trait;
use spice_framework::agent::{
    AgentConfig, AgentOutput, AgentUnderTest, ToolCall as SpiceToolCall, Turn as SpiceTurn,
};
use spice_framework::error::SpiceError;
use spice_framework::{Runner, RunnerConfig, suite, test};

use metalcraft::{RunOutcome, Tool};
use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{AgentRuntimeContext, RunOneShotRequest, run_one_shot_task};
use metalcraft_agent::tools::{self, meta_flow, meta_persona, meta_skill};
use metalcraft_agent::{paths, seed};

const PERSONA_SLUG: &str = "workshop-agent";

/// The meta tools the workshop persona must declare and register.
const META_TOOLS: &[&str] = &[
    "persona_list",
    "persona_read",
    "persona_write",
    "persona_delete",
    "skill_list",
    "skill_read",
    "skill_write",
    "skill_delete",
    "flow_list",
    "flow_read",
    "flow_validate",
    "flow_write",
    "flow_delete",
    "flow_run",
    "flow_templates_list",
    "flow_template_read",
    "diagnostics_list",
    "diagnostics_read",
];

static INIT: Once = Once::new();

/// Point the app at an isolated temp data dir and seed defaults. Runs once.
fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-workshop-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();
        seed::ensure_defaults();
    });
}

// ---------------------------------------------------------------------------
// Tier 1 — offline: persona + tool registration + direct tool round-trips.
// ---------------------------------------------------------------------------

#[test]
fn workshop_persona_declares_and_registers_meta_tools() {
    init();

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("workshop-agent persona should resolve");

    for tool in META_TOOLS {
        assert!(
            persona.tools.iter().any(|t| t == tool),
            "workshop-agent must declare meta tool `{tool}`"
        );
    }

    // Build the registry exactly as the runtime would and prove every meta tool
    // actually registers.
    let registry = tools::create_registry_for(&persona.resolved_tool_names());
    let registered = registry.names();
    for tool in META_TOOLS {
        assert!(
            registered.iter().any(|t| *t == *tool),
            "workshop-agent failed to register meta tool `{tool}`"
        );
    }
}

#[tokio::test]
async fn skill_write_round_trips_to_disk() {
    init();

    let out = meta_skill::SkillWriteTool
        .call(serde_json::json!({
            "slug": "greeting",
            "description": "Say hello",
            "body": "# Greeting\n\nSay hello."
        }))
        .await
        .expect("skill_write call");
    assert_eq!(out.get("saved").and_then(|v| v.as_str()), Some("greeting"));

    let path = paths::skills_dir().join("greeting.md");
    assert!(
        path.exists(),
        "skill_write should create {}",
        path.display()
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("description: Say hello"));
    assert!(content.contains("# Greeting"));

    // And it reloads through the read tool.
    let read = meta_skill::SkillReadTool
        .call(serde_json::json!({ "slug": "greeting" }))
        .await
        .unwrap();
    assert_eq!(
        read.get("description").and_then(|v| v.as_str()),
        Some("Say hello")
    );
}

/// A built-in skill's frontmatter `version:` is what lets a corrected copy reach
/// this pod on the next start. An edit through `skill_write` that dropped it
/// would reset the skill to 0.0.0 and hand the next seed bump a free overwrite,
/// so the write carries the installed version forward unless one is passed.
#[tokio::test]
async fn skill_write_preserves_the_installed_version() {
    init();

    let seeded = paths::skills_dir().join("authoring-skills.md");
    let before = std::fs::read_to_string(&seeded).expect("authoring-skills is seeded");
    assert!(
        before.contains("version:"),
        "seeded skill should carry a frontmatter version"
    );
    let version = metalcraft_agent::skill::installed_version("authoring-skills")
        .expect("seeded skill declares a version");

    meta_skill::SkillWriteTool
        .call(serde_json::json!({
            "slug": "authoring-skills",
            "description": "Format and conventions for authoring metalcraft skills",
            "body": "# Authoring Skills\n\nEdited in place."
        }))
        .await
        .expect("skill_write call");

    let after = std::fs::read_to_string(&seeded).unwrap();
    assert!(
        after.contains(&format!("version: {version}")),
        "an edit with no version must keep the installed one, got: {after}"
    );

    // An explicit version wins, so a user can version a skill of their own.
    meta_skill::SkillWriteTool
        .call(serde_json::json!({
            "slug": "authoring-skills",
            "description": "Format and conventions for authoring metalcraft skills",
            "body": "# Authoring Skills\n\nEdited again.",
            "version": "9.0.0"
        }))
        .await
        .expect("skill_write call");
    assert!(
        std::fs::read_to_string(&seeded)
            .unwrap()
            .contains("version: 9.0.0"),
        "an explicit version should be written"
    );

    // Leave the seeded skill as we found it — later tests read this data dir.
    std::fs::write(&seeded, before).unwrap();
}

#[tokio::test]
async fn persona_write_round_trips_and_reloads() {
    init();

    let out = meta_persona::PersonaWriteTool
        .call(serde_json::json!({
            "slug": "note-taker",
            "persona": {
                "name": "Note Taker",
                "description": "Keeps notes",
                "tools": ["read_file", "write_file"],
                "system_prompt": "You take notes."
            }
        }))
        .await
        .expect("persona_write call");
    assert_eq!(
        out.get("saved").and_then(|v| v.as_str()),
        Some("note-taker")
    );

    let loaded =
        Persona::load("note-taker", &paths::personas_dir()).expect("written persona reloads");
    assert_eq!(loaded.name, "Note Taker");
    assert_eq!(
        loaded.tools,
        vec!["read_file".to_string(), "write_file".to_string()]
    );
}

#[tokio::test]
async fn flow_validate_flags_a_bad_flow() {
    init();

    // An invalid flow id (spaces) must surface a validation error and NOT be valid.
    let out = meta_flow::FlowValidateTool
        .call(serde_json::json!({
            "flow": {
                "spec_version": "1",
                "id": "not a valid id",
                "name": "Bad",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "flow": { "nodes": [], "edges": [] }
            }
        }))
        .await
        .expect("flow_validate call");
    assert_eq!(out.get("valid").and_then(|v| v.as_bool()), Some(false));
    let errors = out.get("errors").and_then(|v| v.as_array()).unwrap();
    assert!(
        !errors.is_empty(),
        "expected validation errors for a bad flow id"
    );
}

#[tokio::test]
async fn meta_writes_refuse_pack_owned_slugs() {
    init();
    // Install an agent pack, then prove a write to a persona *it* provides is
    // refused — the user has to choose a different slug rather than silently
    // shadowing what they installed.
    //
    // This used to enable an integration and write over the persona it
    // carried. Integrations do not carry personas any more: they are tools, and an
    // agent pack is what ships an identity. Same guard, correct owner.
    use metalcraft_agent::agent_packs::{self, bundle, manifest::*};
    use std::collections::BTreeMap;

    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert(
        "agent_presets/spice-test.json".into(),
        serde_json::to_vec(&serde_json::json!({
            "slug": "spice-test", "name": "Spice Test", "default_persona": "spice-owned",
            "personas": [{ "slug": "spice-owned", "role": "default" }],
        }))
        .unwrap(),
    );
    files.insert(
        "personas/spice-owned.json".into(),
        serde_json::to_vec(&serde_json::json!({
            "name": "Owned", "description": "provided by an agent pack",
            "tools": ["read_file"], "system_prompt": "You are owned.",
        }))
        .unwrap(),
    );
    let mut m = AgentPackManifest::new("spice-test-agent", "Spice Test", "1.0.0");
    m.presets = vec!["spice-test".into()];
    m.provides = Provides {
        personas: vec!["spice-owned".into()],
        skills: vec![],
        integrations: vec![],
    };
    let archive = bundle::write(m, files).expect("build archive");
    agent_packs::install(&archive, "bundle").expect("install");

    let out = meta_persona::PersonaWriteTool
        .call(serde_json::json!({
            "slug": "spice-owned",
            "persona": {
                "name": "x", "description": "y",
                "tools": ["read_file"], "system_prompt": "z"
            }
        }))
        .await
        .unwrap();
    assert!(
        out.get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.contains("read-only")),
        "writing a pack-owned persona slug should be refused, got: {out}"
    );
}

// ---------------------------------------------------------------------------
// Spice adapter: drive the real workshop persona agent as an AgentUnderTest.
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
            approval_mode: ApprovalMode::AutoApprove,
            diagnostics: None,
            instance_id: None,
            preset_personas: None,
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
// Tier 2 — live: the workshop persona authors a skill (gated on OPENAI_API_KEY).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_workshop_authors_a_skill() {
    init();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!(
            "SKIP live_workshop_authors_a_skill: set OPENAI_API_KEY (e.g. in a crate-root .env) \
             to run the live suite."
        );
        return;
    }

    let agent =
        MetalcraftPersonaAgent::for_persona(PERSONA_SLUG).expect("build workshop agent under test");

    let tests = vec![
        test(
            "author-greeting-skill",
            "Create a new skill with slug 'live-greeting' whose description is \
             'Greet the user' and whose body is a short markdown note that says hello.",
        )
        .name("Workshop authors a skill via skill_write")
        .expect_tools(&["skill_write"])
        .expect_no_error()
        .expect(|_out| {
            let path = paths::skills_dir().join("live-greeting.md");
            if path.exists() {
                Ok(())
            } else {
                Err(format!("skill_write did not create {}", path.display()))
            }
        })
        .build(),
        test("greeting-no-tools", "Hi there! Just saying hello.")
            .name("A plain greeting writes nothing")
            .expect_no_error()
            .build(),
    ];

    let suite = suite("Workshop authors project artifacts", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(180),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} workshop spice tests failed",
        report.failed, report.total
    );
}
