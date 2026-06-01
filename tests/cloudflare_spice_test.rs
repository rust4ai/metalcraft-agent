//! Spice test harness for the **cloudflare** integration pack.
//!
//! Two tiers, both run from one test binary (one process) so the
//! process-global `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set
//! exactly once and never raced:
//!
//!   1. `cloudflare_pack_wires_up` — always runs, no network. Seeds the bundled
//!      packs into an isolated data dir, enables `cloudflare`, loads the
//!      `cloudflare-agent` persona, and asserts every tool it references
//!      resolves to a parseable api-tool config that targets
//!      api.cloudflare.com and authenticates with `$CLOUDFLARE_API_TOKEN`.
//!      Proves the pack is internally consistent without any network call.
//!
//!   2. `live_chat_uses_cloudflare_persona` — a real, gated [Spice] suite that
//!      drives an actual agentic loop (OpenAI LLM -> cloudflare HTTP tools ->
//!      live Cloudflare API) through the `cloudflare-agent` persona. Skipped
//!      unless both `OPENAI_API_KEY` and `CLOUDFLARE_API_TOKEN` are present
//!      (drop them in a crate-root `.env`). Run:
//!
//!          cargo test --test cloudflare_spice_test -- --nocapture
//!
//!      The default assertions only read (verify token + list zones) — no
//!      writes. The mutating case (create then delete a TXT record) additionally
//!      requires `CLOUDFLARE_SPICE_WRITE=1` and `CLOUDFLARE_SPICE_ZONE=example.com`
//!      pointing at a zone you don't mind writing a throwaway record to.
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

const PACK_ID: &str = "cloudflare";
const PERSONA_SLUG: &str = "cloudflare-agent";

/// The tools the persona exposes via its pack scope — kept in lockstep with
/// `seed/integration_packs/cloudflare/api_tools/`.
const EXPECTED_TOOLS: &[&str] = &[
    "cloudflare_verify_token",
    "cloudflare_list_zones",
    "cloudflare_list_dns_records",
    "cloudflare_get_dns_record",
    "cloudflare_create_dns_record",
    "cloudflare_update_dns_record",
    "cloudflare_patch_dns_record",
    "cloudflare_delete_dns_record",
];

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir =
            std::env::temp_dir().join(format!("mc-cloudflare-spice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        // SAFETY: set before any other thread touches the environment or
        // paths::data_dir(); guarded by `Once` so it happens exactly once.
        unsafe {
            std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
        }
        dotenvy::dotenv().ok();

        seed::ensure_defaults();
        integration_packs::set_enabled(PACK_ID, true).expect("enable cloudflare pack");
    });
}

fn live_keys_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("CLOUDFLARE_API_TOKEN").is_ok()
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
            // Non-interactive: no TTY to approve tool calls, and the cloudflare_*
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
fn cloudflare_pack_wires_up() {
    init();

    assert!(
        integration_packs::is_enabled(PACK_ID),
        "cloudflare pack should be enabled after init()"
    );

    let persona = Persona::load(PERSONA_SLUG, &paths::personas_dir())
        .expect("cloudflare-agent persona should resolve from the enabled pack");

    // The persona is scoped to the cloudflare pack rather than listing each tool.
    assert!(
        persona.packs.iter().any(|p| p == PACK_ID),
        "persona should be scoped to the cloudflare pack via `packs`"
    );
    // Its resolved tool set (explicit tools + pack-scoped tools) exposes every
    // cloudflare tool plus the native load_skill.
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
        persona.skills.iter().any(|s| s == "cloudflare-dns"),
        "persona should reference the cloudflare-dns skill"
    );

    // Every cloudflare tool the persona names resolves to a parseable api-tool
    // config in the pack, targets the Cloudflare API, and authenticates with the
    // scoped token.
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
            cfg["url"]
                .as_str()
                .is_some_and(|u| u.contains("api.cloudflare.com")),
            "api tool `{tool}` should target api.cloudflare.com"
        );
        assert!(
            cfg["headers"]["Authorization"]
                .as_str()
                .is_some_and(|h| h.contains("$CLOUDFLARE_API_TOKEN")),
            "api tool `{tool}` should authenticate with $CLOUDFLARE_API_TOKEN"
        );
    }

    let recommended = integration_packs::recommended_env();
    assert!(
        recommended.iter().any(|(var, packs)| var == "CLOUDFLARE_API_TOKEN"
            && packs.iter().any(|p| p == PACK_ID)),
        "cloudflare pack should recommend CLOUDFLARE_API_TOKEN"
    );
}

// ---------------------------------------------------------------------------
// Tier 2 — live: a real agentic loop through the persona (gated on keys).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_chat_uses_cloudflare_persona() {
    init();

    if !live_keys_present() {
        eprintln!(
            "SKIP live_chat_uses_cloudflare_persona: set OPENAI_API_KEY and CLOUDFLARE_API_TOKEN \
             (e.g. in a crate-root .env) to run the live suite."
        );
        return;
    }

    let agent = MetalcraftPersonaAgent::for_persona(PERSONA_SLUG)
        .expect("build cloudflare-agent under test");

    // Read-only, non-destructive assertions.
    let mut tests = vec![
        test(
            "verify-token",
            "Is my Cloudflare API token valid and active?",
        )
        .name("Verifies the token via cloudflare_verify_token")
        .expect_tools(&["cloudflare_verify_token"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test(
            "list-zones",
            "List the Cloudflare zones (domains) I can manage.",
        )
        .name("Lists zones via cloudflare_list_zones")
        .expect_tools(&["cloudflare_list_zones"])
        .expect_tools_within_allowlist()
        .expect_no_error()
        .build(),
        test("greeting-no-tools", "Hi there! Just saying hello.")
            .name("A plain greeting calls no tools")
            .expect_no_tools()
            .expect_no_error()
            .build(),
    ];

    // Mutating case — creates then deletes a throwaway TXT record. Opt in
    // explicitly and point it at a zone you don't mind writing to.
    if std::env::var("CLOUDFLARE_SPICE_WRITE").is_ok() {
        if let Ok(zone) = std::env::var("CLOUDFLARE_SPICE_ZONE") {
            tests.push(
                test(
                    "create-and-delete-txt",
                    format!(
                        "In the Cloudflare zone {zone}, create a TXT record named \
                         \"_metalcraft-spice-test.{zone}\" with the value \
                         \"metalcraft spice test\", then delete that same record."
                    ),
                )
                .name("Creates and deletes a TXT record")
                .expect_tools(&["cloudflare_create_dns_record", "cloudflare_delete_dns_record"])
                .expect_tools_within_allowlist()
                .expect_no_error()
                .build(),
            );
        } else {
            eprintln!(
                "NOTE: CLOUDFLARE_SPICE_WRITE is set but CLOUDFLARE_SPICE_ZONE=example.com is not \
                 — skipping the record create/delete case."
            );
        }
    } else {
        eprintln!(
            "NOTE: set CLOUDFLARE_SPICE_WRITE=1 and CLOUDFLARE_SPICE_ZONE=example.com to also run \
             the write (create/delete TXT record) case."
        );
    }

    let suite = suite("Cloudflare DNS persona", tests);

    let runner = Runner::new(RunnerConfig {
        concurrency: 2,
        default_timeout: Duration::from_secs(180),
        console_output: true,
        ..Default::default()
    });

    let report = runner.run(suite, std::sync::Arc::new(agent)).await;

    assert_eq!(
        report.failed, 0,
        "{}/{} cloudflare persona spice tests failed",
        report.failed, report.total
    );
}
