//! Spice test harness for the **buildr.space agent pack** — the end-to-end
//! question "can an orchestrator preset agent, handed a pack it has never heard
//! of, provision a remote coding workspace and clone a repo into it?"
//!
//! The user story, in the user's own words:
//!
//! > create a buildrspace workspace and clone in https://github.com/ethereumdegen/octaweave
//!
//! Four tiers, all from one test binary (one process) so the process-global
//! `METALCRAFT_DATA_DIR` / `paths::data_dir()` `OnceLock` is set exactly once and
//! never raced — the same discipline as `config_spice_test.rs`. Nothing here
//! touches the developer's real `~/.metalcraft`: the whole harness runs against a
//! throwaway data dir, which is what makes "the pod starts without this pack" an
//! assertable fact rather than a hope.
//!
//!   1. `buildr_pack_installs_into_a_clean_pod` — always runs, no network, no
//!      LLM, no credential. Proves a clean pod has no buildr-space, installs the
//!      vendored `.agentpack` fixture, and then proves the three things the live
//!      tiers depend on: the 26 `buildr_*` tools register, a `sub_agent`
//!      delegation built AS `buildr-space-agent` can actually reach them, and the
//!      `general-agent` preset's orchestrator — which shipped long before this
//!      pack existed — now lists it as a delegation target.
//!
//!   2. `live_buildr_tools_provision_and_clone` — live, no LLM. Drives the pack's
//!      own tools directly: create a workspace, poll it to `ready`, clone, then
//!      read the git remote back off the sprite. This is the credential and
//!      service proof, and the diagnostic when tier 4 fails: it separates "the
//!      model never called the tools" from "buildr.space could not do it".
//!
//!   3. `live_orchestrator_delegates_clone_to_buildr` — the real thing. A [Spice]
//!      suite drives an actual agentic loop through the `general-agent` preset's
//!      orchestrator with the prompt above, and asserts it delegated to
//!      `buildr-space-agent`, that the delegate called create/poll/clone, and —
//!      the part that is not a transcript check — that a workspace on
//!      buildr.space really does hold an octaweave checkout afterwards.
//!
//!   4. A negative control rides in the same suite: a plain greeting delegates
//!      nothing.
//!
//! ## Running it
//!
//! Tier 1 needs nothing. Tiers 2-4 spend money — they provision a real
//! sprites.dev workspace and (tier 3) real inference — so they are opt-in and
//! skip loudly otherwise. Put these in a crate-root `.env`:
//!
//! ```text
//! BUILDR_SPICE_LIVE=1        # opt in; without it tiers 2-4 skip
//! BUILDR_API_KEY=bsk_...     # a WRITE-scoped PAT from buildr.space -> API keys
//! OPENAI_API_KEY=sk-...      # or METALCRAFT_TOKEN + OPENAI_BASE_URL
//! # BUILDR_TEST_REPO=ethereumdegen/octaweave   # the default
//! # BUILDR_AGENTPACK=/path/to/other.agentpack  # override the vendored fixture
//! ```
//!
//! `ethereumdegen/octaweave` is **private**, so the buildr.space GitHub App must
//! be installed on the `ethereumdegen` account under the same buildr.space
//! account that owns `BUILDR_API_KEY`. buildr resolves the installation itself
//! from the repo owner, so there is no id to pass — but a missing grant fails as
//! git's own 404, which names neither cause nor cure. The live tiers therefore
//! preflight scope + installation + repo access and skip with an actionable
//! message *before* provisioning anything.
//!
//! ```bash
//! cargo test --test buildr_space_spice_test -- --nocapture
//! ```
//!
//! ## Cleanup
//!
//! A workspace is a running sprite, which is a recurring bill. Every live tier
//! snapshots the account's workspace ids first and deletes whatever is new
//! afterwards — through `catch_unwind`, so a failed assertion still cleans up. It
//! deletes by *difference*, never by name, so a workspace that was already there
//! when the test started is never touched.
//!
//! [Spice]: https://crates.io/crates/spice-framework

use std::sync::{Once, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::{Value, json};
use spice_framework::agent::{
    AgentConfig, AgentOutput, AgentUnderTest, ToolCall as SpiceToolCall, Turn as SpiceTurn,
};
use spice_framework::error::SpiceError;
use spice_framework::{Runner, RunnerConfig, suite, test};

use metalcraft::{RunOutcome, Tool};
use metalcraft_agent::agent_packs::{self, InstallReport};
use metalcraft_agent::agent_preset::AgentPreset;
use metalcraft_agent::approval::ApprovalMode;
use metalcraft_agent::key_store::KeyStore;
use metalcraft_agent::persona::Persona;
use metalcraft_agent::runtime::{AgentRuntimeContext, RunOneShotRequest, run_one_shot_task};
use metalcraft_agent::tools::{self, http_api::HttpApiTool, meta_integration, meta_keys};
use metalcraft_agent::{integrations, key_store, paths, seed};

// ---------------------------------------------------------------------------
// What is under test
// ---------------------------------------------------------------------------

/// The pack, vendored rather than fetched. See `tests/fixtures/README.md`.
const FIXTURE: &[u8] = include_bytes!("fixtures/buildr-space-0.2.0.agentpack");

const PACK_ID: &str = "buildr-space";
const PACK_VERSION: &str = "0.2.0";
const PACK_PRESET: &str = "buildr-space";
const BUILDR_PERSONA: &str = "buildr-space-agent";
const BUILDR_SKILL: &str = "buildr-space";
const BUILDR_KEY: &str = "BUILDR_API_KEY";

/// The preset the request arrives at: the out-of-the-box orchestrator.
const PRESET_SLUG: &str = "general-agent";
const ORCHESTRATOR_SLUG: &str = "orchestrator-agent";

const DEFAULT_TEST_REPO: &str = "ethereumdegen/octaweave";

/// Every tool the pack must vendor. Spelled out rather than counted so a pack
/// that silently drops one fails on the name, not on an arithmetic mismatch.
const EXPECTED_BUILDR_TOOLS: &[&str] = &[
    "buildr_whoami",
    "buildr_list_installations",
    "buildr_list_repos",
    "buildr_list_workspaces",
    "buildr_create_workspace",
    "buildr_get_workspace",
    "buildr_delete_workspace",
    "buildr_hibernate_workspace",
    "buildr_wake_workspace",
    "buildr_clone",
    "buildr_read_file",
    "buildr_write_file",
    "buildr_list_dir",
    "buildr_delete_path",
    "buildr_exec",
    "buildr_git",
    "buildr_build",
    "buildr_test",
    "buildr_configure_actions",
    "buildr_list_runs",
    "buildr_get_run",
    "buildr_serve",
    "buildr_serve_stop",
    "buildr_serve_logs",
    "buildr_expose",
    "buildr_fetch",
];

/// The subset the delegate must actually call to satisfy the user story.
const REQUIRED_CALLS: &[&str] = &[
    "buildr_create_workspace",
    "buildr_get_workspace",
    "buildr_clone",
];

// ---------------------------------------------------------------------------
// Process setup — one throwaway pod, set up exactly once
// ---------------------------------------------------------------------------

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| {
        let data_dir = std::env::temp_dir().join(format!("mc-buildr-spice-{}", std::process::id()));
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

/// Serialize everything that touches global pod state. All tiers share one data
/// dir per process, and `key_set` is read-modify-write against `keys.json`, so
/// parallel `cargo test` threads would otherwise clobber each other. Poison is
/// recovered so one failing tier doesn't cascade.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_state() -> std::sync::MutexGuard<'static, ()> {
    STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Installing the pack — observed once, asserted by tier 1, relied on by 2-4
// ---------------------------------------------------------------------------

/// What installing the pack into a clean pod looked like.
///
/// The before-state is captured *inside* the install so tier 1 can assert "the
/// pod started without this" no matter which test `cargo test` happens to run
/// first. A plain `assert!(find(..).is_none())` in the test body would be a race
/// against whichever live tier installed it already.
struct FirstInstall {
    pack_absent_before: bool,
    persona_absent_before: bool,
    tools_absent_before: bool,
    key_configured_before: bool,
    report: InstallReport,
}

fn install_once() -> &'static FirstInstall {
    static INSTALLED: OnceLock<FirstInstall> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        init();
        let pack_absent_before = agent_packs::find(PACK_ID).is_none();
        let persona_absent_before = Persona::load(BUILDR_PERSONA, &paths::personas_dir()).is_err();
        let tools_absent_before = HttpApiTool::try_load("buildr_clone").is_none();
        // Whether the credential was already reachable (a developer's `.env`
        // supplies it for the live tiers), so tier 1 can tell "install left it
        // unconfigured" apart from "the environment already had it".
        let key_configured_before = key_store::lookup(BUILDR_KEY).is_some();

        let report = agent_packs::install(&fixture_bytes(), "fixture:tests/fixtures")
            .expect("the vendored buildr-space agentpack should verify, validate and install");

        FirstInstall {
            pack_absent_before,
            persona_absent_before,
            tools_absent_before,
            key_configured_before,
            report,
        }
    })
}

fn fixture_bytes() -> Vec<u8> {
    match std::env::var("BUILDR_AGENTPACK") {
        Ok(path) if !path.trim().is_empty() => std::fs::read(path.trim())
            .unwrap_or_else(|e| panic!("BUILDR_AGENTPACK={path} could not be read: {e}")),
        _ => FIXTURE.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Tier 1 — offline: a clean pod gains a whole agent from one archive
// ---------------------------------------------------------------------------

/// What the installed persona declares for the delegation guard.
fn buildr_persona_max_run_secs() -> u64 {
    Persona::load(BUILDR_PERSONA, &paths::personas_dir())
        .expect("the pack's persona should resolve after install")
        .max_run_secs
        .unwrap_or(0)
}

#[test]
fn buildr_pack_installs_into_a_clean_pod() {
    let _guard = lock_state();
    let installed = install_once();

    // ── the pod really did start without any of this ────────────────────────
    assert!(
        installed.pack_absent_before,
        "the throwaway pod already had {PACK_ID} installed — tier 1 would be testing nothing"
    );
    assert!(
        installed.persona_absent_before,
        "`{BUILDR_PERSONA}` resolved before the pack was installed"
    );
    assert!(
        installed.tools_absent_before,
        "`buildr_clone` loaded before the pack was installed"
    );

    // ── what the archive said it was ────────────────────────────────────────
    let report = &installed.report;
    assert_eq!(report.id, PACK_ID);
    assert_eq!(
        report.version, PACK_VERSION,
        "the vendored fixture has drifted from PACK_VERSION — refresh it \
         (tests/fixtures/README.md) or update the constant"
    );
    assert!(
        report.presets.iter().any(|p| p == PACK_PRESET),
        "install should report the {PACK_PRESET} preset: {:?}",
        report.presets
    );
    assert!(
        report.personas.iter().any(|p| p == BUILDR_PERSONA),
        "install should report the {BUILDR_PERSONA} persona: {:?}",
        report.personas
    );
    assert!(
        report.skills.iter().any(|s| s == BUILDR_SKILL),
        "install should report the {BUILDR_SKILL} skill: {:?}",
        report.skills
    );
    assert!(
        report.preset_collisions.is_empty(),
        "the fixture collided with an already-installed preset: {:?}",
        report.preset_collisions
    );

    // Consent is computed from the archive's bytes, so this is env-independent:
    // the pack wants one credential and can reach one host.
    assert!(
        report
            .consent
            .requires_env
            .iter()
            .any(|e| e.name == BUILDR_KEY && e.required),
        "the pack must declare {BUILDR_KEY} as required: {:?}",
        report.consent.requires_env
    );
    assert!(
        report.consent.domains.iter().any(|d| d == "buildr.space"),
        "the pack's tools should reach buildr.space and nothing else: {:?}",
        report.consent.domains
    );
    // `missing_env` reads the live environment, so it only means something when
    // the developer has not already supplied the key for the live tiers.
    if !installed.key_configured_before {
        assert!(
            report.missing_env.iter().any(|e| e == BUILDR_KEY),
            "with no {BUILDR_KEY} anywhere, install should warn that it is missing: {:?}",
            report.missing_env
        );
    }

    // ── the integration and its tools actually landed ───────────────────────
    assert!(
        integrations::list_installed()
            .iter()
            .any(|p| p.manifest.id == PACK_ID),
        "the {PACK_ID} integration should be installed"
    );

    let vendored = HttpApiTool::installed_tool_names_for_integration(PACK_ID);
    for tool in EXPECTED_BUILDR_TOOLS {
        assert!(
            vendored.iter().any(|t| t == tool),
            "the pack should vendor `{tool}`; it vendored {vendored:?}"
        );
    }
    assert_eq!(
        vendored.len(),
        EXPECTED_BUILDR_TOOLS.len(),
        "the pack vendored tools this test does not know about: {:?}",
        vendored
            .iter()
            .filter(|t| !EXPECTED_BUILDR_TOOLS.contains(&t.as_str()))
            .collect::<Vec<_>>()
    );

    // ── the long ops declare a timeout that outlasts the server's own ──────
    // Every api-tool used to run on one hard-coded 30s client timeout, while
    // buildr.space allows itself 300s for a clone and 600s for a build. The tool
    // gave up first, so the agent saw a failure for work that was still
    // succeeding — and for `build`, the run it was told about no longer existed.
    // These are the ops where that gap was real; the number must clear the
    // server's own bound, or the client is still the thing that gives up.
    for (tool, server_bound) in [
        ("buildr_clone", 300u64),
        ("buildr_exec", 120),
        ("buildr_git", 120),
        ("buildr_configure_actions", 120),
        ("buildr_build", 240),
        ("buildr_test", 240),
        ("buildr_serve", 60),
    ] {
        let loaded = HttpApiTool::try_load(tool).unwrap_or_else(|| panic!("`{tool}` should load"));
        let declared = loaded.config().timeout_secs.unwrap_or(0);
        assert!(
            declared > server_bound,
            "`{tool}` must declare a timeout past buildr.space's own {server_bound}s bound, \
             or the tool gives up on work the server is still doing; it declared {declared}s"
        );
    }

    // A build that outlives `wait_secs` answers `running` and is finished by
    // polling. Both halves have to be declared or the agent cannot follow it: the
    // knob to bound the wait, and the poll flag that stops the step guard reading
    // a deliberate poll loop as a runaway.
    for tool in ["buildr_build", "buildr_test"] {
        let loaded = HttpApiTool::try_load(tool).unwrap();
        let props = loaded.config().parameters.get("properties");
        assert!(
            props.and_then(|p| p.get("wait_secs")).is_some(),
            "`{tool}` should expose `wait_secs` so the agent can stop holding the request open"
        );
        assert!(
            loaded.config().body_defaults.contains_key("wait_secs"),
            "`{tool}` should default `wait_secs`, so a model that ignores it still gets a bounded wait"
        );
    }
    for tool in ["buildr_get_run", "buildr_get_workspace"] {
        let loaded = HttpApiTool::try_load(tool).unwrap();
        assert!(
            loaded.config().poll,
            "`{tool}` is called repeatedly on purpose and must be flagged `poll`, or the step \
             guard reads waiting as a runaway loop"
        );
    }

    // The delegation guard defaults to 120s, and this persona spends the first
    // one to two minutes waiting for a sprite to reach `ready`. Without its own
    // declared bound, every delegation to it dies mid-wait and reads as an agent
    // that failed the task.
    assert!(
        buildr_persona_max_run_secs() >= 600,
        "`{BUILDR_PERSONA}` must declare a `max_run_secs` long enough to outlive provisioning"
    );

    // ── a `sub_agent` delegation can reach them ─────────────────────────────
    // This is the hole worth closing: a persona can list an integration whose
    // tools never register, and the delegation then fails at call time with the
    // model reaching for a tool that does not exist. Reassemble exactly what
    // `sub_agent` builds for `persona: "buildr-space-agent"`.
    let buildr = Persona::load(BUILDR_PERSONA, &paths::personas_dir())
        .expect("the pack's persona should resolve after install");
    let resolved = buildr.resolved_tool_names();
    let registry = tools::create_registry_for(&resolved);
    let registered = registry.names();
    for tool in REQUIRED_CALLS {
        assert!(
            resolved.iter().any(|t| t == tool),
            "`{BUILDR_PERSONA}` should resolve `{tool}` through its integration"
        );
        assert!(
            registered.contains(tool),
            "a `{BUILDR_PERSONA}` sub-agent failed to register `{tool}` — delegation cannot reach it"
        );
    }
    assert!(
        buildr.skills.iter().any(|s| s == BUILDR_SKILL),
        "`{BUILDR_PERSONA}` should reference the {BUILDR_SKILL} skill"
    );

    // ── the orchestrator, which predates this pack, can now delegate to it ───
    let preset = AgentPreset::load(PRESET_SLUG, &paths::agent_presets_dir())
        .expect("the seeded general-agent preset should resolve");
    assert_eq!(preset.default_persona, ORCHESTRATOR_SLUG);
    assert!(
        preset.delegates_to_any_persona,
        "{PRESET_SLUG} must delegate outside its declared roster, or a pack installed \
         later is unreachable no matter how good it is"
    );
    let roster = preset.delegation_roster(&paths::personas_dir());
    assert!(
        roster.iter().any(|p| p == BUILDR_PERSONA),
        "{BUILDR_PERSONA} should have joined the delegation roster: {roster:?}"
    );

    let orchestrator = Persona::load(ORCHESTRATOR_SLUG, &paths::personas_dir())
        .expect("orchestrator-agent should resolve");
    assert!(
        orchestrator.tools.iter().any(|t| t == "sub_agent"),
        "the orchestrator must have sub_agent to delegate at all"
    );
    // It routes work; it does not hold the specialist's tools itself.
    for tool in EXPECTED_BUILDR_TOOLS {
        assert!(
            !orchestrator.tools.iter().any(|t| t == tool),
            "the orchestrator should not declare `{tool}` directly — it delegates"
        );
    }
    // And the assembled prompt — what the model actually sees — names the
    // specialist, via `{{available_personas}}` rather than a hardcoded slug.
    let assembled = orchestrator.build_system_prompt(&paths::skills_dir(), ".");
    assert!(
        assembled.contains(BUILDR_PERSONA),
        "the assembled orchestrator prompt should list {BUILDR_PERSONA} so the model \
         knows the specialist exists"
    );

    // ── the credential the pack asked for can be configured ─────────────────
    const SENTINEL: &str = "bsk_buildrspice_TESTKEY_do_not_use";
    let before = read_env_requirement(BUILDR_KEY);
    if !installed.key_configured_before {
        assert_eq!(
            before,
            Some(false),
            "with no {BUILDR_KEY} anywhere, the integration should report it unconfigured"
        );
    }

    let set = futures_lite_block_on(meta_keys::KeySetTool.call(json!({
        "name": BUILDR_KEY, "value": SENTINEL
    })))
    .expect("key_set should not error");
    assert_eq!(set["saved"], BUILDR_KEY);
    assert_ne!(
        set["masked"], SENTINEL,
        "key_set must return a masked value, not the raw secret"
    );
    assert_eq!(
        KeyStore::load(&paths::keys_file()).get(BUILDR_KEY),
        Some(SENTINEL),
        "key_set should have persisted {BUILDR_KEY} into the pod's key store"
    );
    assert_eq!(
        read_env_requirement(BUILDR_KEY),
        Some(true),
        "after key_set the integration should report {BUILDR_KEY} configured"
    );

    // Leave no state behind: the live tiers store the real key themselves, and
    // the process environment still supplies it either way.
    let _ = futures_lite_block_on(meta_keys::KeyDeleteTool.call(json!({ "name": BUILDR_KEY })));
}

/// Whether the integration reports `name` configured, or `None` if it doesn't
/// require it at all.
fn read_env_requirement(name: &str) -> Option<bool> {
    let read =
        futures_lite_block_on(meta_integration::IntegrationReadTool.call(json!({ "id": PACK_ID })))
            .expect("integration_read should not error");
    read["requires_env"]
        .as_array()?
        .iter()
        .find(|e| e["name"] == name)
        .and_then(|e| e["configured"].as_bool())
}

/// Run one future to completion from a sync test. Tier 1 is a `#[test]` on
/// purpose — it must not need a runtime to say what a clean pod looks like —
/// but the meta tools are async.
fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a current-thread runtime")
        .block_on(fut)
}

// ---------------------------------------------------------------------------
// Live plumbing: calling the pack's own tools, and cleaning up after them
// ---------------------------------------------------------------------------

fn live_enabled() -> bool {
    matches!(
        std::env::var("BUILDR_SPICE_LIVE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn test_repo() -> String {
    std::env::var("BUILDR_TEST_REPO")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_TEST_REPO.to_string())
}

/// Stores `BUILDR_API_KEY` in the pod's key store for the duration of a live
/// tier — what a provisioned pod looks like — and removes it afterwards. The
/// process environment still supplies it, so nothing downstream breaks.
struct StoredKey;

impl StoredKey {
    fn set(value: &str) -> Self {
        let mut store = KeyStore::load(&paths::keys_file());
        store.upsert(BUILDR_KEY, value);
        store
            .save(&paths::keys_file())
            .expect("persisting the buildr key into the throwaway pod");
        Self
    }
}

impl Drop for StoredKey {
    fn drop(&mut self) {
        let mut store = KeyStore::load(&paths::keys_file());
        if store.delete(BUILDR_KEY) {
            let _ = store.save(&paths::keys_file());
        }
    }
}

/// Call one of the pack's HTTP api-tools exactly as the agent would.
async fn call_tool(name: &str, args: Value) -> Value {
    let tool = HttpApiTool::try_load(name)
        .unwrap_or_else(|| panic!("api tool `{name}` should have been installed by the pack"));
    tool.call(args)
        .await
        .unwrap_or_else(|e| panic!("`{name}` failed: {e}"))
}

/// The `data` of a successful call, or a panic naming the status and body. The
/// envelope is `{ "status": u16, "data": … }` (see `HttpApiTool::call`).
fn ok_data(name: &str, envelope: &Value) -> Value {
    let status = envelope["status"].as_u64().unwrap_or(0);
    assert!(
        (200..300).contains(&status),
        "`{name}` returned {status}: {envelope}"
    );
    envelope["data"].clone()
}

/// A soft variant for the ground-truth probes, where a workspace that is no
/// longer usable is an answer rather than a crash.
fn maybe_data(envelope: &Value) -> Option<Value> {
    let status = envelope["status"].as_u64().unwrap_or(0);
    (200..300)
        .contains(&status)
        .then(|| envelope["data"].clone())
}

async fn workspace_ids() -> Vec<String> {
    let list = call_tool("buildr_list_workspaces", json!({})).await;
    let Some(data) = maybe_data(&list) else {
        return Vec::new();
    };
    data.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|w| w["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Poll a workspace to `ready`, the way the persona tells the agent to.
async fn wait_ready(id: &str, budget: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        let got = call_tool("buildr_get_workspace", json!({ "id": id })).await;
        let data = ok_data("buildr_get_workspace", &got);
        last = data["status"].as_str().unwrap_or_default().to_string();
        match last.as_str() {
            "ready" => return Ok(()),
            "failed" | "deleted" => {
                return Err(format!(
                    "workspace {id} went to `{last}`: {}",
                    data["error"].as_str().unwrap_or("(no error recorded)")
                ));
            }
            _ => tokio::time::sleep(Duration::from_secs(5)).await,
        }
    }
    Err(format!(
        "workspace {id} never reached `ready` within {}s (last status: `{last}`)",
        budget.as_secs()
    ))
}

/// Ground truth: does this workspace actually hold a checkout of `repo`?
///
/// Reads the clone's own git remote off the sprite. The clone script points the
/// remote at `https://github.com/<repo>.git` and authenticates through a
/// credential helper, so no token can be sitting in the URL this prints.
async fn workspace_holds(id: &str, repo: &str) -> bool {
    let out = call_tool(
        "buildr_exec",
        json!({
            "id": id,
            "cmd": "git -C /workspace/app config --get remote.origin.url",
        }),
    )
    .await;
    maybe_data(&out)
        .and_then(|d| d["output"].as_str().map(str::to_lowercase))
        .is_some_and(|o| o.contains(&repo.to_lowercase()))
}

/// Delete every workspace that did not exist when the tier started.
///
/// By difference, never by name: the account running this may hold real
/// workspaces, and a sweep that guessed from names would eventually eat one.
async fn sweep_new(before: &[String]) {
    for id in workspace_ids().await {
        if before.iter().any(|b| b == &id) {
            continue;
        }
        eprintln!("cleanup: deleting workspace {id} created by this test");
        let _ = call_tool("buildr_delete_workspace", json!({ "id": id })).await;
    }
}

/// Run a live tier with cleanup that survives a failed assertion. The body gets
/// the pre-run workspace ids so it can tell which workspaces it caused.
async fn with_cleanup<F, Fut>(body: F)
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let before = workspace_ids().await;
    let outcome = std::panic::AssertUnwindSafe(body(before.clone()))
        .catch_unwind()
        .await;
    sweep_new(&before).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Everything the live tiers need, or the reason they cannot run.
enum Preflight {
    Ready { repo: String, key: String },
    Skip(String),
}

/// Check scope, installation and repo access *before* provisioning anything.
///
/// A private repo with no GitHub App grant fails as git's own 404 four minutes
/// into a paid workspace, naming neither the cause nor the cure. Each of these
/// is one cheap read.
async fn preflight() -> Preflight {
    if !live_enabled() {
        return Preflight::Skip(
            "set BUILDR_SPICE_LIVE=1 to run the live tiers (they \
             provision a real sprites.dev workspace)"
                .to_string(),
        );
    }
    let Some(key) = key_store::lookup_present(BUILDR_KEY) else {
        return Preflight::Skip(format!(
            "set {BUILDR_KEY} (a write-scoped bsk_ PAT from buildr.space) in a crate-root .env"
        ));
    };
    let repo = test_repo();
    let Some((owner, _)) = repo.split_once('/') else {
        return Preflight::Skip(format!("BUILDR_TEST_REPO={repo} is not `owner/repo`"));
    };

    // 1. The token, and whether it may do anything but read. `scopes` is null for
    //    a full-access owner session and a list for a PAT.
    let who = call_tool("buildr_whoami", json!({})).await;
    let Some(who) = maybe_data(&who) else {
        return Preflight::Skip(format!("{BUILDR_KEY} was rejected by buildr.space: {who}"));
    };
    let writes = match &who["scopes"] {
        Value::Null => true,
        Value::Array(scopes) => scopes.iter().any(|s| s == "write"),
        _ => false,
    };
    if !writes {
        return Preflight::Skip(format!(
            "{BUILDR_KEY} has no `write` scope, so it cannot create a workspace. Mint a \
             write-scoped PAT at buildr.space -> account menu -> API keys (a PAT can \
             never mint a PAT, so this needs a browser session)."
        ));
    }

    // 2. An installation for the repo's owner — the credential that opens a
    //    private repo. buildr picks it by owner, so none is a hard stop.
    let installs = call_tool("buildr_list_installations", json!({})).await;
    let installs = maybe_data(&installs).unwrap_or(Value::Null);
    let has_owner = installs.as_array().is_some_and(|rows| {
        rows.iter().any(|i| {
            i["account_login"]
                .as_str()
                .is_some_and(|l| l.eq_ignore_ascii_case(owner))
        })
    });
    if !has_owner {
        return Preflight::Skip(format!(
            "no GitHub App installation for `{owner}`. {repo} is private, so the clone \
             would 404: install the buildr.space GitHub App on `{owner}` from \
             buildr.space -> Connect GitHub, under the account that owns {BUILDR_KEY}."
        ));
    }

    // 3. And that the grant actually covers this repo — an installation can be
    //    scoped to selected repositories.
    let repos = call_tool("buildr_list_repos", json!({})).await;
    let granted = maybe_data(&repos)
        .and_then(|d| d["repos"].as_array().cloned())
        .unwrap_or_default();
    let covered = granted.iter().any(|r| {
        r["full_name"]
            .as_str()
            .is_some_and(|f| f.eq_ignore_ascii_case(&repo))
    });
    if !covered {
        return Preflight::Skip(format!(
            "the GitHub App installation on `{owner}` does not grant {repo} — add it to \
             the installation's selected repositories"
        ));
    }

    Preflight::Ready { repo, key }
}

// ---------------------------------------------------------------------------
// Tier 2 — live, no LLM: the pack's tools really do provision and clone
// ---------------------------------------------------------------------------

/// The live tiers hold a `std::sync::Mutex` across `await`s on purpose: they
/// serialize *whole* test bodies against one another (shared data dir, shared
/// key store, shared buildr.space account), and an async mutex would not make
/// that any safer — each `#[tokio::test]` is its own runtime, and nothing else
/// contends for this lock.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn live_buildr_tools_provision_and_clone() {
    install_once();
    let _guard = lock_state();

    let (repo, key) = match preflight().await {
        Preflight::Ready { repo, key } => (repo, key),
        Preflight::Skip(why) => {
            eprintln!("SKIP live_buildr_tools_provision_and_clone: {why}");
            return;
        }
    };
    let _key = StoredKey::set(&key);

    with_cleanup(|_before| async move {
        let created = call_tool(
            "buildr_create_workspace",
            json!({
                "name": format!("mc-spice-{}-tools", std::process::id()),
                "repo_full_name": repo,
            }),
        )
        .await;
        let ws = ok_data("buildr_create_workspace", &created);
        let id = ws["id"]
            .as_str()
            .unwrap_or_else(|| panic!("create returned no workspace id: {ws}"))
            .to_string();

        wait_ready(&id, Duration::from_secs(300))
            .await
            .unwrap_or_else(|e| panic!("{e}"));

        let cloned = call_tool(
            "buildr_clone",
            json!({ "id": id, "repo_full_name": repo.clone() }),
        )
        .await;
        let clone = ok_data("buildr_clone", &cloned);
        assert_eq!(
            clone["exit_code"],
            0,
            "cloning {repo} failed: {}",
            clone["output"].as_str().unwrap_or_default()
        );

        assert!(
            workspace_holds(&id, &repo).await,
            "the workspace does not hold a {repo} checkout after a successful clone"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// Spice adapter: the real preset agent as an AgentUnderTest
// ---------------------------------------------------------------------------

/// Drives the actual persona agent, bound to a preset's delegation roster —
/// which is what makes this the *preset* agent rather than a bare persona.
struct MetalcraftPresetAgent {
    context: AgentRuntimeContext,
    persona_slug: String,
    preset_personas: Option<Vec<String>>,
    available_tools: Vec<String>,
    model_name: String,
    cwd: String,
    display_name: String,
}

impl MetalcraftPresetAgent {
    fn for_preset(preset_slug: &str) -> Result<Self, String> {
        let context = AgentRuntimeContext::from_environment().map_err(|e| e.to_string())?;
        let preset = AgentPreset::load(preset_slug, &paths::agent_presets_dir())?;
        let persona = Persona::load(&preset.default_persona, &context.personas_dir)?;
        Ok(Self {
            persona_slug: preset.default_persona.clone(),
            preset_personas: Some(preset.delegation_roster(&paths::personas_dir())),
            available_tools: persona.resolved_tool_names(),
            model_name: metalcraft_agent::runtime::DEFAULT_MODEL.to_string(),
            cwd: ".".to_string(),
            display_name: format!("metalcraft:{preset_slug}"),
            context,
        })
    }
}

#[async_trait]
impl AgentUnderTest for MetalcraftPresetAgent {
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
            // Non-interactive: no TTY to approve tool calls, and delegation plus
            // the buildr tools classify as `Execute` (would otherwise block).
            approval_mode: ApprovalMode::AutoApprove,
            diagnostics: None,
            instance_id: None,
            preset_personas: self.preset_personas.clone(),
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
                    .map(|r| json!({ "name": r.name, "result": r.result }))
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

/// The `persona` argument of every `sub_agent` call — proof of *who* the
/// orchestrator handed the work to.
fn delegated_personas(out: &AgentOutput) -> Vec<String> {
    let mut personas = Vec::new();
    for turn in &out.turns {
        for call in &turn.tool_calls {
            if call.name != "sub_agent" {
                continue;
            }
            if let Some(p) = call.arguments.get("persona").and_then(|v| v.as_str())
                && !p.is_empty()
            {
                personas.push(p.to_string());
            }
        }
    }
    personas
}

/// The `task` text handed to each delegate — proof of *what* was passed through.
fn delegated_tasks(out: &AgentOutput) -> Vec<String> {
    let mut tasks = Vec::new();
    for turn in &out.turns {
        for call in &turn.tool_calls {
            if call.name != "sub_agent" {
                continue;
            }
            if let Some(t) = call.arguments.get("task").and_then(|v| v.as_str()) {
                tasks.push(t.to_string());
            }
        }
    }
    tasks
}

/// The tools a delegated sub-agent reported using, read out of the parent's
/// `sub_agent` tool results (`{ "result": "<json with tools_used>" }`). A
/// delegate's own calls never appear in the orchestrator's `tools_called`.
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
            let Ok(parsed) = serde_json::from_str::<Value>(payload) else {
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
// Tier 3 — live: the orchestrator preset agent does the user's actual request
// ---------------------------------------------------------------------------

/// The live tiers hold a `std::sync::Mutex` across `await`s on purpose: they
/// serialize *whole* test bodies against one another (shared data dir, shared
/// key store, shared buildr.space account), and an async mutex would not make
/// that any safer — each `#[tokio::test]` is its own runtime, and nothing else
/// contends for this lock.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn live_orchestrator_delegates_clone_to_buildr() {
    install_once();
    let _guard = lock_state();

    let (repo, key) = match preflight().await {
        Preflight::Ready { repo, key } => (repo, key),
        Preflight::Skip(why) => {
            eprintln!("SKIP live_orchestrator_delegates_clone_to_buildr: {why}");
            return;
        }
    };
    if AgentRuntimeContext::from_environment().is_err() {
        eprintln!(
            "SKIP live_orchestrator_delegates_clone_to_buildr: no inference credential. \
             Set OPENAI_API_KEY, or METALCRAFT_TOKEN + OPENAI_BASE_URL, in a crate-root .env."
        );
        return;
    }
    let _key = StoredKey::set(&key);

    with_cleanup(|before| async move {
        let agent = MetalcraftPresetAgent::for_preset(PRESET_SLUG)
            .expect("build the general-agent preset agent under test");

        let prompt = format!("create a buildrspace workspace and clone in https://github.com/{repo}");
        let repo_for_expect = repo.clone();

        let tests = vec![
            test("create-workspace-and-clone", prompt)
                .name("Orchestrator delegates the workspace + clone to buildr-space-agent")
                .expect_tools(&["sub_agent"])
                .expect_tools_within_allowlist()
                .expect_no_error()
                .expect(move |out| {
                    // It delegated, and to the specialist the pack brought ...
                    let personas = delegated_personas(out);
                    if !personas.iter().any(|p| p == BUILDR_PERSONA) {
                        return Err(format!(
                            "did not delegate to {BUILDR_PERSONA} (sub_agent persona args: {personas:?})"
                        ));
                    }
                    // ... carrying the target through rather than paraphrasing it away.
                    let tasks = delegated_tasks(out);
                    let repo_name = repo_for_expect
                        .split('/')
                        .next_back()
                        .unwrap_or(&repo_for_expect);
                    if !tasks
                        .iter()
                        .any(|t| t.to_lowercase().contains(&repo_name.to_lowercase()))
                    {
                        return Err(format!(
                            "the delegated task never mentions `{repo_name}`: {tasks:?}"
                        ));
                    }
                    // ... and the delegate actually provisioned and cloned.
                    let used = delegated_tools_used(out);
                    let missing: Vec<_> = REQUIRED_CALLS
                        .iter()
                        .filter(|t| !used.iter().any(|u| u == *t))
                        .collect();
                    if !missing.is_empty() {
                        return Err(format!(
                            "the delegate never called {missing:?} (tools_used: {used:?})"
                        ));
                    }
                    Ok(())
                })
                .build(),
            test("greeting-no-tools", "Hi there! Just saying hello.")
                .name("A plain greeting delegates nothing")
                .expect_no_tools()
                .expect_no_error()
                .build(),
        ];

        let runner = Runner::new(RunnerConfig {
            concurrency: 1,
            // Orchestrator -> sub_agent -> provision (1-2 min) -> clone. Generous
            // on purpose: a timeout here reads as a failed agent.
            default_timeout: Duration::from_secs(900),
            console_output: true,
            ..Default::default()
        });

        let report = runner
            .run(suite("Orchestrator provisions a buildr.space workspace", tests), std::sync::Arc::new(agent))
            .await;

        assert_eq!(
            report.failed, 0,
            "{}/{} buildr-space spice tests failed",
            report.failed, report.total
        );

        // The proof that is not a transcript: a workspace on buildr.space really
        // holds an octaweave checkout. Prefer the ones this run created; fall
        // back to any workspace targeting the repo, since the persona is told to
        // reuse a ready workspace rather than provision a second sprite.
        let after = workspace_ids().await;
        let mut candidates: Vec<String> = after
            .iter()
            .filter(|id| !before.contains(id))
            .cloned()
            .collect();
        if candidates.is_empty() {
            candidates = after;
        }
        let mut found = false;
        for id in &candidates {
            if workspace_holds(id, &repo).await {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "the agent reported success, but no workspace ({} checked) holds a {repo} checkout",
            candidates.len()
        );
    })
    .await;
}
