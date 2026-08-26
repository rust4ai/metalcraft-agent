//! Delegating to a persona an **agent pack** installed.
//!
//! The orchestrator reaches a specialist through `sub_agent { persona }`, which
//! refuses up front if the persona declares an integration the pod does not have.
//! An agent pack vendors its integrations into the content store, never into
//! `<data>/integrations/` — so a check that knew only the legacy layout reported
//! every one of them missing, and delegating to any installed agent's persona
//! failed with "install the agent pack that provides them" for a pack that was
//! already installed. Its tools, resolved through the pack layers, were fine the
//! whole time; only the guard disagreed.
//!
//! One `#[test]`: `METALCRAFT_DATA_DIR` is process-global.

use metalcraft_agent::agent_packs::{self, bundle, manifest::*};
use metalcraft_agent::persona::Persona;
use metalcraft_agent::tools::http_api::HttpApiTool;
use metalcraft_agent::tools::sub_agent::missing_integrations;
use metalcraft_agent::{integrations, paths};
use std::collections::BTreeMap;

const PACK: &str = "acme-crm-agent";
const INTEGRATION: &str = "acme-crm";
const PERSONA: &str = "acme-crm-persona";

fn json(v: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}

fn files() -> BTreeMap<String, Vec<u8>> {
    let mut f = BTreeMap::new();
    f.insert(
        format!("agent_presets/{PACK}.json"),
        json(serde_json::json!({
            "slug": PACK,
            "name": "Acme CRM Agent",
            "description": "Works Acme CRM.",
            "default_persona": PERSONA,
            "personas": [{ "slug": PERSONA, "role": "default" }],
            "integrations": [INTEGRATION],
            "version": "1.0.0",
        })),
    );
    f.insert(
        format!("personas/{PERSONA}.json"),
        json(serde_json::json!({
            "name": "Acme CRM Persona",
            "description": "Reads and writes Acme CRM records.",
            "tools": ["load_skill"],
            "packs": [INTEGRATION],
            "system_prompt": "You work Acme CRM.",
        })),
    );
    f.insert(
        format!("integrations/{INTEGRATION}/integration.json"),
        json(serde_json::json!({
            "id": INTEGRATION,
            "name": "Acme CRM",
            "description": "Acme CRM over HTTP.",
            "version": "1.0.0",
            "requires_env": ["ACME_API_KEY"],
        })),
    );
    for (tool, method, url) in [
        ("acme_list_contacts", "GET", "https://acme.example/contacts"),
        (
            "acme_create_contact",
            "POST",
            "https://acme.example/contacts",
        ),
    ] {
        f.insert(
            format!("integrations/{INTEGRATION}/api_tools/{tool}.json"),
            json(serde_json::json!({
                "name": tool, "description": tool, "method": method, "url": url,
                "headers": { "Authorization": "Bearer $ACME_API_KEY" },
                "parameters": { "type": "object", "properties": {} },
            })),
        );
    }
    f
}

fn manifest() -> AgentPackManifest {
    let mut m = AgentPackManifest::new(PACK, "Acme CRM Agent", "1.0.0");
    m.presets = vec![PACK.into()];
    m.provides = Provides {
        personas: vec![PERSONA.into()],
        skills: vec![],
        integrations: vec![IntegrationRef {
            id: INTEGRATION.into(),
            version: "1.0.0".into(),
            content_sha256: None,
            source: None,
        }],
    };
    m
}

#[test]
fn an_agent_packs_persona_is_delegable_once_the_pack_is_installed() {
    let data_dir = std::env::temp_dir().join(format!("mc-delegation-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).unwrap();
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let archive = bundle::write(manifest(), files()).expect("write archive");
    let report = agent_packs::install(&archive, "test").expect("install");
    assert_eq!(report.personas, vec![PERSONA.to_string()]);

    // The integration lives in the content store, not the legacy directory.
    assert!(
        !paths::integrations_dir().join(INTEGRATION).exists(),
        "an agent pack must not write into the legacy integrations dir"
    );
    assert!(
        integrations::is_enabled(INTEGRATION),
        "a vendored integration is installed and must count as such"
    );

    // The orchestrator can see the persona to delegate to it at all.
    let visible = Persona::list_summaries(&paths::personas_dir());
    let summary = visible
        .iter()
        .find(|s| s.slug == PERSONA)
        .expect("the pack's persona is visible pod-wide");
    assert_eq!(summary.pack_id.as_deref(), Some(PACK));

    // …and `sub_agent { persona }` admits it rather than refusing with
    // "install the agent pack that provides them".
    let persona = Persona::load(PERSONA, &paths::personas_dir()).expect("load persona");
    assert!(
        missing_integrations(&persona).is_empty(),
        "delegation guard rejected a persona whose pack is installed"
    );

    // The tools it delegates *for* resolve through the pack layers. The pack id
    // and the integration id are deliberately different strings here: they were
    // the same in the pack this was found on, which is the only reason its tools
    // resolved at all.
    assert_ne!(PACK, INTEGRATION);
    let tools = persona.resolved_tool_names();
    for expected in ["load_skill", "acme_list_contacts", "acme_create_contact"] {
        assert!(tools.contains(&expected.to_string()), "missing {expected}");
    }

    // The orchestrator's other route to the same tools — `sub_agent { tool_set:
    // "all", pack }` — scopes by integration id and must find them too.
    let scoped = HttpApiTool::installed_tool_names_for_integration(INTEGRATION);
    assert_eq!(scoped.len(), 2, "scoped to one integration: {scoped:?}");
    assert!(HttpApiTool::installed_tool_names_for_integration(PACK).is_empty());

    let _ = std::fs::remove_dir_all(&data_dir);
}
