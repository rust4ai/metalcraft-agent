//! Proves the *real* credential path behind `GET /api/v1/inference` — paths →
//! keys.json → env — and, specifically, that it disagrees with the key store in
//! exactly the case that matters.
//!
//! A provisioned pod is given `OPENAI_API_KEY` and `OPENAI_BASE_URL` as container
//! env. `GET /api/v1/keys` lists keys.json, so that pod reports **no keys at all**
//! while being perfectly able to think. Clients that inferred "no key, cannot
//! think" from the key store told people their working pod was dead; this endpoint
//! exists because the pod is the only thing that can tell them otherwise.
//!
//! Its own test binary so the process-global env / data-dir mutation is isolated
//! and single-threaded (data_dir() caches via OnceCell on first use).
use metalcraft_agent::key_store::KeyStore;
use metalcraft_agent::runtime::{InferenceCredential, inference_credential};
use std::fs;

const GATEWAY: &str = "https://inference.metalcraftai.com/v1";

#[test]
fn the_pod_reports_a_credential_its_key_store_cannot_show() {
    let dir = std::env::temp_dir().join(format!("mc-agent-inference-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let keys_file = dir.join("keys.json");
    fs::write(&keys_file, "{}").unwrap();

    // SAFETY: single-threaded test binary; set the data dir before the first
    // paths::data_dir() call (cached via OnceCell) so lookups read our keys.json.
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &dir);
        std::env::set_var("OPENAI_BASE_URL", GATEWAY);
        std::env::set_var("OPENAI_API_KEY", "mck_injected_by_provisioning");
    }

    // The bug, in two assertions. The key store is empty…
    assert!(
        KeyStore::load(&keys_file).list_masked().is_empty(),
        "a provisioned pod's credential is env, so its key store is empty",
    );
    // …and the pod can think anyway, which only the pod can report.
    let (key, credential) = inference_credential().expect("a provisioned pod can think");
    assert_eq!(key, "mck_injected_by_provisioning");
    assert_eq!(credential, InferenceCredential::Environment);
    assert_eq!(credential.as_str(), "environment");

    // Binding a key through the API overrides the injected one — store-first — and
    // is reported as the user's own, which is what makes the Bind/Change
    // distinction in a client truthful.
    fs::write(&keys_file, r#"{"OPENAI_API_KEY":"sk-the-users-own"}"#).unwrap();
    let (key, credential) = inference_credential().expect("a bound key still thinks");
    assert_eq!(key, "sk-the-users-own");
    assert_eq!(credential, InferenceCredential::Stored);
    assert_eq!(credential.as_str(), "stored");

    // No provider key anywhere: the pod falls back to its own ecosystem identity,
    // which the gateway accepts. Asking this owner to paste a credential they had
    // already given the ecosystem was the older version of the same mistake.
    fs::write(&keys_file, "{}").unwrap();
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::set_var("METALCRAFT_TOKEN", "mck_the_pods_own");
    }
    let (key, credential) = inference_credential().expect("the pod authenticates as itself");
    assert_eq!(key, "mck_the_pods_own");
    assert_eq!(credential, InferenceCredential::PodToken);
    assert_eq!(credential.as_str(), "pod_token");

    // …but only at the gateway. Pointed anywhere else, that token is not offered
    // and the pod correctly reports that it cannot think.
    unsafe { std::env::set_var("OPENAI_BASE_URL", "https://api.openai.com/v1") }
    assert!(
        inference_credential().is_none(),
        "this pod's account token must never be sent to another provider",
    );

    // And with nothing at all, `ready: false` is the honest answer — the state the
    // "cannot think" warning was always meant for.
    unsafe {
        std::env::remove_var("METALCRAFT_TOKEN");
        std::env::remove_var("OPENAI_BASE_URL");
    }
    assert!(inference_credential().is_none());

    fs::remove_dir_all(&dir).ok();
}
