//! Spice test harness for the **config-agent** persona — the agent that
//! configures this agent itself (installs integrations, manages the API
//! key store, edits personas/skills).
//!
//! Three tiers, all from one test binary (one process) so the process-global
//! `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set exactly once
//! and never raced:
//!
//!   1. `config_agent_wires_up` — always runs, no network. Seeds the bundled
//!      packs+personas into an isolated data dir and asserts the `config-agent`
//!      persona resolves and exposes the pack/key/persona meta tools.
//!
//!   2. `configuring_a_pack_via_meta_tools_works` — always runs, no network and
//!      NO LLM. Calls the meta tools directly (exactly what the agent would do
//!      for "set up X with key Y") against the `email` pack — installed by this
//!      file's fixture from `unbundled_packs/`, since it is no longer seeded —
//!      and proves the side effects actually land: the key is stored, reported configured, and never
//!      echoed back. This is the deterministic proof that the meta tools *do the
//!      thing*.
//!
//!      There used to be an enable step here. Packs are no longer enabled or
//!      disabled — an agent pack is the install unit, and an integration it
//!      vendors is present or absent (see `docs/AGENT_PACKS_PLAN.md`). What is
//!      left to configure is the key.
//!
//!   3. `live_config_agent_installs_metalcraft_email` — a real, gated [Spice]
//!      suite that drives an actual agentic loop (OpenAI LLM -> meta tools)
//!      through the `config-agent` persona with the prompt "install the
//!      metalcraft-email integration using this Metalcraft token …". Asserts
//!      the model calls `key_set` and verifies the key landed in the store —
//!      the pack itself is already installed.
//!      Skipped unless `OPENAI_API_KEY` is present (drop it in a crate-root
//!      `.env`); needs NO real token — it's a throwaway we only check
//!      round-trips into the store. Run:
//!
//!          cargo test --test config_spice_test -- --nocapture
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
use metalcraft_agent::key_store::KeyStore;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{AgentRuntimeContext, RunOneShotRequest, run_one_shot_task};
use metalcraft_agent::tools::{self, meta_integration, meta_keys};
use metalcraft_agent::{integrations, key_store, paths, seed};

const PERSONA_SLUG: &str = "config-agent";
const ORCHESTRATOR_SLUG: &str = "orchestrator-agent";

/// Tools the config-agent must expose to configure the agent itself — kept in
/// lockstep with `seed/personas/config-agent.json`.
const EXPECTED_TOOLS: &[&str] = &[
    "integration_list",
    "agentpack_list",
    "agentpack_install",
    "key_list",
    "key_set",
    "key_delete",
    "persona_list",
    "persona_read",
    "persona_write",
    "persona_delete",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-config-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();

        // Seed bundled packs + personas. Deliberately leave every pack DISABLED
        // so the install tiers below start from a clean slate and prove they do
        // the enabling themselves.
        seed::ensure_defaults();

        // The two email packs are no longer seeded — they live in
        // `unbundled_packs/` so a pod does not arrive holding mailbox access —
        // but they are still what these tiers configure, and a pack with a
        // required key is exactly the fixture the meta-tool tiers need. Install
        // them the way a user would after finding them on a registry, which also
        // proves the unbundled directory still builds into a valid archive.
        for id in ["email", "metalcraft-email"] {
            install_unbundled_pack(id);
        }
    });
}

/// Install a pack from `unbundled_packs/<id>` — the checked-in tree built into a
/// real archive by the same code a registry upload would use.
fn install_unbundled_pack(id: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("unbundled_packs")
        .join(id);
    let bytes = metalcraft_agent::agent_packs::bundle::from_dir(&dir)
        .unwrap_or_else(|e| panic!("building the '{id}' pack from {}: {e}", dir.display()));
    metalcraft_agent::agent_packs::install(&bytes, "test-fixture")
        .unwrap_or_else(|e| panic!("installing the '{id}' pack: {e}"));
}

/// Serialize tests that touch global key state. All tiers share one data dir per
/// process, and `key_set` is read-modify-write against `keys.json`, so parallel
/// `cargo test` threads would otherwise race, clobbering each other's writes.
/// Held for the whole test body; poison is recovered so one failing tier doesn't
/// cascade.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_state() -> std::sync::MutexGuard<'static, ()> {
    STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clear a pack's key so a mutating tier doesn't depend on another tier's cleanup
/// having run first. (The pack itself has no state to reset — it is installed.)
fn reset_pack_key(_pack: &str, key: &str) {
    let mut store = KeyStore::load(&paths::keys_file());
    if store.delete(key) {
        let _ = store.save(&paths::keys_file());
    }
}

/// Collect the `persona` arg from every `sub_agent` call — proof of *who* the
/// orchestrator delegated to.
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

/// Collect the tool names a delegated sub-agent reported using, by scanning the
/// parent's `sub_agent` tool results (`{ "result": "<json with tools_used>" }`).
/// The delegated meta-tool calls never appear in the orchestrator's own
/// top-level `tools_called` — only inside the sub_agent result payload.
fn delegated_tools_used(out: &AgentOutput) -> Vec<String> {
    let mut used = Vec::new();
    for turn in &out.turns {
        for tr in &turn.tool_results {
            if tr.get("name").and_then(|n| n.as_str()) != Some("sub_agent") {
                continue;
            }
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
            // Non-interactive: no TTY to approve tool calls, and the meta tools
            // classify as `Execute` (would otherwise block).
            approval_mode: ApprovalMode::AutoApprove,
            diagnostics: None,
            instance_id: None,
            preset_personas: None,
        project_brief: None,
        project_id: None,
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
// Tier 1 — offline: the config-agent persona is wired up with the meta tools.
// ---------------------------------------------------------------------------

#[test]
fn config_agent_wires_up() {
    init();
    let _guard = lock_state();

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("config-agent persona should resolve from the seed");

    let resolved = persona.resolved_tool_names();
    for tool in EXPECTED_TOOLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "config-agent is missing expected meta tool `{tool}`"
        );
    }
    // Regression guard: `pack_enable` is retired. A persona still listing it would
    // resolve to nothing and the agent would keep reaching for a tool that no longer
    // exists — silently, since unknown names are dropped from the registry.
    assert!(
        !resolved
            .iter()
            .any(|t| t == "integration_enable" || t == "integration_disable"),
        "config-agent still lists a retired enable/disable tool: {resolved:?}"
    );

    assert!(
        persona.skills.iter().any(|s| s == "managing-integrations"),
        "config-agent should reference the managing-integrations skill"
    );

    // An installed integration is simply present — there is no off state to turn on.
    assert!(
        integrations::list_installed()
            .iter()
            .any(|p| p.manifest.id == "metalcraft-packs"),
        "metalcraft-packs pack should be installed (seeded)"
    );
    assert!(
        integrations::list_installed()
            .iter()
            .any(|p| p.manifest.id == "metalcraft-email"),
        "metalcraft-email pack should be installed (by this file's fixture, \
         from unbundled_packs/ — it is not seeded)"
    );
}

// ---------------------------------------------------------------------------
// Tier 2 — offline, no LLM: the meta tools actually perform the install.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn configuring_a_pack_via_meta_tools_works() {
    init();
    let _guard = lock_state();

    // Uses the `email` pack / IMAP_PASSWORD so it can't collide with the live
    // tier's global state (which uses `metalcraft-email` / METALCRAFT_TOKEN).
    const PACK: &str = "email";
    const KEY: &str = "IMAP_PASSWORD";
    const VALUE: &str = "imap_config_spice_test_password_value";

    reset_pack_key(PACK, KEY);

    // Step 1: read the pack — what the agent does to learn what it still needs.
    // The pack is already there; what's missing is the key.
    let read = meta_integration::IntegrationReadTool
        .call(serde_json::json!({ "id": PACK }))
        .await
        .expect("pack_read should not error");
    // The result surfaces the still-missing required key so the agent knows to set it.
    let requires = read["requires_env"]
        .as_array()
        .expect("requires_env should be an array");
    assert!(
        requires
            .iter()
            .any(|e| e["name"] == KEY && e["configured"] == false),
        "email pack should report {KEY} as a still-missing required key"
    );

    // Step 2: set the API key — exactly what the agent's key_set call does.
    let set = meta_keys::KeySetTool
        .call(serde_json::json!({ "name": KEY, "value": VALUE }))
        .await
        .expect("key_set should not error");
    assert_eq!(set["saved"], KEY);
    // The raw secret must never be echoed back.
    assert_ne!(
        set["masked"], VALUE,
        "key_set must return a masked value, not the raw secret"
    );

    // The key actually landed in the store and resolves for `$NAME` expansion.
    assert_eq!(
        KeyStore::load(&paths::keys_file()).get(KEY),
        Some(VALUE),
        "key_set should have persisted {KEY} into the key store"
    );
    assert_eq!(key_store::lookup(KEY).as_deref(), Some(VALUE));

    // Step 3: key_list now reports the required key configured (no raw leak).
    let list = meta_keys::KeyListTool
        .call(serde_json::json!({}))
        .await
        .expect("key_list should not error");
    let recommended = list["recommended"]
        .as_array()
        .expect("recommended should be an array");
    assert!(
        recommended
            .iter()
            .any(|e| e["name"] == KEY && e["configured"] == true),
        "key_list should report {KEY} configured for the enabled email pack"
    );
    let raw = serde_json::to_string(&list).unwrap();
    assert!(
        !raw.contains(VALUE),
        "key_list must never expose the raw secret value"
    );

    // Cleanup so this tier leaves no state for the live tier.
    let _ = meta_keys::KeyDeleteTool
        .call(serde_json::json!({ "name": KEY }))
        .await;
}

// ---------------------------------------------------------------------------
// Tier 3 — live: a real agentic loop drives the install through the persona.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_config_agent_sets_up_metalcraft_email() {
    init();
    let _guard = lock_state();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!(
            "SKIP live_config_agent_sets_up_metalcraft_email: set OPENAI_API_KEY \
             (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    // Throwaway key — we never call the service, only verify it round-trips into
    // the store. Use a recognizable sentinel so the post-run assertion is precise.
    const TEST_KEY_VALUE: &str = "mck_configspice_TESTKEY_do_not_use";

    // Precondition: metalcraft-email must start unkeyed, so the agent has to store
    // the token itself rather than find one already there.
    reset_pack_key("metalcraft-email", "METALCRAFT_TOKEN");

    let agent =
        MetalcraftPersonaAgent::for_persona(PERSONA_SLUG).expect("build config-agent under test");

    let tests = vec![
        test(
            "setup-metalcraft-email",
            format!(
                "Set up the metalcraft-email integration for me — store this Metalcraft token so its tools can authenticate: {TEST_KEY_VALUE}"
            ),
        )
        .name("Stores METALCRAFT_TOKEN for the metalcraft-email pack")
        .expect_tools(&["key_set"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test("greeting-no-tools", "Hi there! Just saying hello.")
            .name("A plain greeting calls no tools")
            .expect_no_tools()
            .expect_no_error()
            .build(),
    ];

    let suite = suite("Config-agent install", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 1,
        default_timeout: Duration::from_secs(180),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} config-agent spice tests failed",
        report.failed, report.total
    );

    // The proof it actually worked: the side effect landed in real state.
    assert_eq!(
        key_store::lookup("METALCRAFT_TOKEN").as_deref(),
        Some(TEST_KEY_VALUE),
        "after the setup prompt, METALCRAFT_TOKEN should be stored verbatim"
    );

    // Leave no state behind.
    let mut store = KeyStore::load(&paths::keys_file());
    if store.delete("METALCRAFT_TOKEN") {
        let _ = store.save(&paths::keys_file());
    }
}

// ---------------------------------------------------------------------------
// Tier 4 — offline: the orchestrator's delegation path can reach the config
// tools. This closes the "the orchestrator might not know to route a self-
// configuration request" hole: it proves (a) the orchestrator is told to
// delegate such tasks to `config-agent`, (b) it delegates rather than holding
// the meta tools itself, and (c) a sub-agent built AS `config-agent` — exactly
// how `sub_agent` assembles a persona delegation — actually registers the
// config meta tools. No network, no spend.
// ---------------------------------------------------------------------------

#[test]
fn orchestrator_can_delegate_config() {
    init();
    let _guard = lock_state();

    let orchestrator = Persona::load(ORCHESTRATOR_SLUG, &paths::personas_dir())
        .expect("orchestrator-agent persona should resolve");

    // It delegates (has sub_agent) ...
    assert!(
        orchestrator.tools.iter().any(|t| t == "sub_agent"),
        "orchestrator must have sub_agent to delegate"
    );
    // ... and routes self-configuration to the config persona DYNAMICALLY.
    // Per ADR-0001, the prompt must not hardcode the `config-agent` slug; it
    // references the `{{available_personas}}` placeholder, which is substituted
    // with the live persona list (slug + description) at assembly time.
    assert!(
        !orchestrator.system_prompt.contains("config-agent"),
        "orchestrator's raw prompt must NOT hardcode the config-agent slug (ADR-0001) — use {{available_personas}}"
    );
    assert!(
        orchestrator
            .system_prompt
            .contains("{{available_personas}}"),
        "orchestrator's prompt should inject the live persona list via {{available_personas}}"
    );
    // The ASSEMBLED prompt (what the model actually sees) must surface
    // config-agent through that injection, so configuration tasks can be routed.
    let assembled = orchestrator.build_system_prompt(&paths::skills_dir(), ".");
    assert!(
        assembled.contains("config-agent"),
        "assembled orchestrator prompt should list config-agent via {{available_personas}}"
    );
    // It must NOT carry the config meta tools itself — those belong to the
    // delegated persona, not the router.
    for tool in [
        "agentpack_install",
        "key_set",
        "integration_list",
        "key_list",
    ] {
        assert!(
            !orchestrator.tools.iter().any(|t| t == tool),
            "orchestrator should NOT declare `{tool}` directly — it delegates to config-agent"
        );
    }

    // Reassemble exactly what `sub_agent` builds for `persona: "config-agent"`:
    // a registry from that persona's resolved tool names. Prove the config meta
    // tools actually register through that path.
    let config_persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("config-agent persona should resolve");
    let registry = tools::create_registry_for(&config_persona.resolved_tool_names());
    let registered = registry.names();
    for tool in EXPECTED_TOOLS {
        assert!(
            registered.iter().any(|t| *t == *tool),
            "a config-agent sub-agent failed to register `{tool}` — delegation can't reach it"
        );
    }
}

// ---------------------------------------------------------------------------
// Tier 5 — live: the orchestrator, given the user's exact phrasing, delegates
// setting a pack's credential to config-agent, and the config tools fire through
// the sub-agent. The ask is "store this token", not "install this pack": a pack
// arrives inside an agent pack the operator installs, and no tool on this pod
// fetches one.
// Gated on OPENAI_API_KEY; uses a throwaway metalcraft-email key.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_orchestrator_delegates_setup_to_config() {
    init();
    let _guard = lock_state();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!(
            "SKIP live_orchestrator_delegates_setup_to_config: set OPENAI_API_KEY \
             (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    const TEST_KEY_VALUE: &str = "mck_orchspice_TESTKEY_do_not_use";

    reset_pack_key("metalcraft-email", "METALCRAFT_TOKEN");

    let agent = MetalcraftPersonaAgent::for_persona(ORCHESTRATOR_SLUG)
        .expect("build orchestrator under test");

    let tests = vec![
        test(
            "delegate-setup-metalcraft-email",
            format!(
                "Set up the metalcraft-email integration for me — store this Metalcraft token so its tools can authenticate: {TEST_KEY_VALUE}"
            ),
        )
        .name("Orchestrator delegates the setup to config-agent")
        .expect_tools(&["sub_agent"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .expect(|out| {
            // It delegated AS config-agent ...
            let personas = delegated_personas(out);
            if !personas.iter().any(|p| p == "config-agent") {
                return Err(format!(
                    "orchestrator did not delegate to config-agent (sub_agent persona args: {personas:?})"
                ));
            }
            // ... and that sub-agent actually ran the config tools.
            let used = delegated_tools_used(out);
            if used.iter().any(|t| t == "key_set") {
                Ok(())
            } else {
                Err(format!(
                    "delegated config-agent did not call key_set (tools_used: {used:?})"
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

    let suite = suite("Orchestrator delegates install to Config", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 1,
        // Orchestrator -> sub_agent(config-agent) -> nested LLM loop: be generous.
        default_timeout: Duration::from_secs(240),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} orchestrator-delegates-setup spice tests failed",
        report.failed, report.total
    );

    // The proof the delegated setup actually landed in real state.
    assert_eq!(
        key_store::lookup("METALCRAFT_TOKEN").as_deref(),
        Some(TEST_KEY_VALUE),
        "after delegating the install, METALCRAFT_TOKEN should be stored verbatim"
    );

    // Leave no state behind.
    reset_pack_key("metalcraft-email", "METALCRAFT_TOKEN");
}
