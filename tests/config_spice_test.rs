//! Spice test harness for the **config-agent** persona — the agent that
//! configures this agent itself (installs integration packs, manages the API
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
//!   2. `installing_a_pack_via_meta_tools_works` — always runs, no network and
//!      NO LLM. Calls the meta tools directly (exactly what the agent would do
//!      for "install X using key Y") against the `github` pack and proves the
//!      side effects actually land: the pack flips to enabled and the key is
//!      stored and reported configured. This is the deterministic proof that
//!      the meta tools *do the thing*.
//!
//!   3. `live_config_agent_installs_linear` — a real, gated [Spice] suite that
//!      drives an actual agentic loop (OpenAI LLM -> meta tools) through the
//!      `config-agent` persona with the prompt "install the linear integration
//!      using linear api key …". Asserts the model calls `pack_enable` and
//!      `key_set`, then verifies the `linear` pack is enabled and the key
//!      landed in the store. Skipped unless `OPENAI_API_KEY` is present (drop
//!      it in a crate-root `.env`); needs NO real Linear key — the key is a
//!      throwaway we only check round-trips into the store. Run:
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
use spice_framework::{suite, test, Runner, RunnerConfig};

use metalcraft::{RunOutcome, Tool};
use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::key_store::KeyStore;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{run_one_shot_task, AgentRuntimeContext, RunOneShotRequest};
use metalcraft_agent::tools::{self, meta_integration, meta_keys};
use metalcraft_agent::{integration_packs, key_store, paths, seed};

const PERSONA_SLUG: &str = "config-agent";
const ORCHESTRATOR_SLUG: &str = "orchestrator-agent";

/// Tools the config-agent must expose to configure the agent itself — kept in
/// lockstep with `seed/personas/config-agent.json`.
const EXPECTED_TOOLS: &[&str] = &[
    "pack_list",
    "pack_enable",
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
        let data_dir = std::env::temp_dir().join(format!("mc-meta-spice-{}", std::process::id()));
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
    });
}

/// Serialize tests that touch global pack/key state. All tiers share one data
/// dir per process, and `set_enabled` / `key_set` are read-modify-write against
/// `integration_packs.json` / `keys.json`, so parallel `cargo test` threads
/// would otherwise race (clobbering each other's writes, or reading a pack as
/// enabled mid-install). Held for the whole test body; poison is recovered so
/// one failing tier doesn't cascade.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_state() -> std::sync::MutexGuard<'static, ()> {
    STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reset a pack+key to the "not installed" baseline so a mutating tier doesn't
/// depend on another tier's cleanup having run first.
fn reset_pack_key(pack: &str, key: &str) {
    let _ = integration_packs::set_enabled(pack, false);
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
    assert!(
        persona.skills.iter().any(|s| s == "managing-integrations"),
        "config-agent should reference the managing-integrations skill"
    );

    // The linear pack ships installed but DISABLED — the thing the agent is
    // expected to turn on.
    assert!(
        integration_packs::list_installed()
            .iter()
            .any(|p| p.manifest.id == "linear"),
        "linear pack should be installed (seeded)"
    );
    assert!(
        !integration_packs::is_enabled("linear"),
        "linear pack should start disabled"
    );
}

// ---------------------------------------------------------------------------
// Tier 2 — offline, no LLM: the meta tools actually perform the install.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn installing_a_pack_via_meta_tools_works() {
    init();
    let _guard = lock_state();

    // Uses the `github` pack / GITHUB_TOKEN so it can't collide with the live
    // tier's global state (which uses `linear` / LINEAR_API_KEY).
    const PACK: &str = "github";
    const KEY: &str = "GITHUB_TOKEN";
    const VALUE: &str = "ghp_config_spice_test_token_value";

    reset_pack_key(PACK, KEY);
    assert!(
        !integration_packs::is_enabled(PACK),
        "github pack should start disabled"
    );

    // Step 1: enable (install) the pack — exactly what the agent's pack_enable
    // call does.
    let enable = meta_integration::PackEnableTool
        .call(serde_json::json!({ "id": PACK }))
        .await
        .expect("pack_enable should not error");
    assert_eq!(enable["enabled"], true);
    assert!(
        integration_packs::is_enabled(PACK),
        "pack_enable should have flipped github to enabled"
    );
    // The result surfaces the still-missing required key so the agent knows to set it.
    let requires = enable["pack"]["requires_env"]
        .as_array()
        .expect("requires_env should be an array");
    assert!(
        requires
            .iter()
            .any(|e| e["name"] == KEY && e["configured"] == false),
        "github pack should report {KEY} as a still-missing required key"
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
        "key_list should report {KEY} configured for the enabled github pack"
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
    let _ = integration_packs::set_enabled(PACK, false);
}

// ---------------------------------------------------------------------------
// Tier 3 — live: a real agentic loop drives the install through the persona.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_config_agent_installs_linear() {
    init();
    let _guard = lock_state();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!(
            "SKIP live_config_agent_installs_linear: set OPENAI_API_KEY \
             (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    // Throwaway key — we never call Linear, only verify it round-trips into the
    // store. Use a recognizable sentinel so the post-run assertion is precise.
    const TEST_KEY_VALUE: &str = "lin_api_metaspice_TESTKEY_do_not_use";

    // Preconditions: linear must start disabled and unkeyed so the agent has to
    // do both steps itself.
    reset_pack_key("linear", "LINEAR_API_KEY");
    assert!(
        !integration_packs::is_enabled("linear"),
        "linear should start disabled for the live install test"
    );

    let agent =
        MetalcraftPersonaAgent::for_persona(PERSONA_SLUG).expect("build config-agent under test");

    let tests = vec![
        test(
            "install-linear",
            format!(
                "Install the linear integration for me. Use this Linear API key: {TEST_KEY_VALUE}"
            ),
        )
        .name("Enables the linear pack and stores LINEAR_API_KEY")
        .expect_tools(&["pack_enable", "key_set"])
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

    // The proof it actually worked: the side effects landed in real state.
    assert!(
        integration_packs::is_enabled("linear"),
        "after the install prompt, the linear pack should be enabled"
    );
    assert_eq!(
        key_store::lookup("LINEAR_API_KEY").as_deref(),
        Some(TEST_KEY_VALUE),
        "after the install prompt, LINEAR_API_KEY should be stored verbatim"
    );

    // Leave no state behind.
    let _ = integration_packs::set_enabled("linear", false);
    let mut store = KeyStore::load(&paths::keys_file());
    if store.delete("LINEAR_API_KEY") {
        let _ = store.save(&paths::keys_file());
    }
}

// ---------------------------------------------------------------------------
// Tier 4 — offline: the orchestrator's delegation path can reach the config
// tools. This closes the "the orchestrator might not know to route a self-
// configuration request" hole: it proves (a) the orchestrator is told to
// delegate such tasks to `config-agent`, (b) it delegates rather than holding
// the meta tools itself, and (c) a sub-agent built AS `config-agent` — exactly
// how `sub_agent` assembles a persona delegation — actually registers
// pack_enable / key_set. No network, no spend.
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
        orchestrator.system_prompt.contains("{{available_personas}}"),
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
    for tool in ["pack_enable", "key_set", "pack_list", "key_list"] {
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
// the install to config-agent and the config tools fire through the sub-agent.
// Gated on OPENAI_API_KEY; uses a throwaway linear key.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_orchestrator_delegates_install_to_config() {
    init();
    let _guard = lock_state();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!(
            "SKIP live_orchestrator_delegates_install_to_config: set OPENAI_API_KEY \
             (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    const TEST_KEY_VALUE: &str = "lin_api_orchspice_TESTKEY_do_not_use";

    reset_pack_key("linear", "LINEAR_API_KEY");
    assert!(
        !integration_packs::is_enabled("linear"),
        "linear should start disabled for the orchestrator install test"
    );

    let agent = MetalcraftPersonaAgent::for_persona(ORCHESTRATOR_SLUG)
        .expect("build orchestrator under test");

    let tests = vec![
        test(
            "delegate-install-linear",
            format!(
                "Install the linear integration for me. Use this Linear API key: {TEST_KEY_VALUE}"
            ),
        )
        .name("Orchestrator delegates the install to config-agent")
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
            let enabled = used.iter().any(|t| t == "pack_enable");
            let keyed = used.iter().any(|t| t == "key_set");
            if enabled && keyed {
                Ok(())
            } else {
                Err(format!(
                    "delegated config-agent did not call both pack_enable and key_set (tools_used: {used:?})"
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
        "{}/{} orchestrator-delegates-install spice tests failed",
        report.failed, report.total
    );

    // The proof the delegated install actually landed in real state.
    assert!(
        integration_packs::is_enabled("linear"),
        "after delegating the install, the linear pack should be enabled"
    );
    assert_eq!(
        key_store::lookup("LINEAR_API_KEY").as_deref(),
        Some(TEST_KEY_VALUE),
        "after delegating the install, LINEAR_API_KEY should be stored verbatim"
    );

    // Leave no state behind.
    reset_pack_key("linear", "LINEAR_API_KEY");
}
