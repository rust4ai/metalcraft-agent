//! Plaintext API-key / secret store.
//!
//! A flat `name → value` map persisted as JSON at `<data>/keys.json`. Values
//! are referenced from HTTP-API tool configs via `$NAME` placeholders (see
//! [`crate::tools::http_api`]); [`lookup`] resolves a name from the store and
//! falls back to a process environment variable so existing env-based config
//! (`OPENAI_API_KEY`, a pack's `$GITHUB_TOKEN`, …) keeps working.
//!
//! **Precedence exception:** for a small [`ENV_AUTHORITATIVE`] set (currently
//! `METALCRAFT_TOKEN`), a non-empty environment value *wins over* keys.json. The
//! k3s control plane injects `METALCRAFT_TOKEN` into each pod; a stale key a user
//! once pasted must never shadow the freshly injected one. For every other key,
//! resolution stays store-first (so a user can override env-based defaults).
//!
//! Stored in **plaintext** — protection relies on OS file permissions, the same
//! as the app's other on-disk state. Never log values; the workshop API only
//! ever exposes [`mask`]ed previews.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::paths;

/// Flat secret map. Serialized as a top-level JSON object of `name → value`
/// (via `serde(flatten)`), e.g. `{ "SOLARABASE_API_KEY": "sb_live_…" }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyStore {
    #[serde(flatten)]
    keys: HashMap<String, String>,
}

impl KeyStore {
    /// Read the store from `path`. A missing or empty file yields an empty
    /// store; a malformed file is logged and treated as empty (so a corrupt
    /// keys.json never bricks the daemon).
    pub fn load(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        if content.trim().is_empty() {
            return Self::default();
        }
        serde_json::from_str(&content).unwrap_or_else(|e| {
            log::warn!("keys.json is malformed, ignoring: {e}");
            Self::default()
        })
    }

    /// Write the store to `path` atomically (write a sibling `.tmp` then
    /// rename) so a crash mid-write can't leave a truncated keys.json.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.keys.get(name).map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.keys.contains_key(name)
    }

    /// Insert or replace a key by name.
    pub fn upsert(&mut self, name: &str, value: &str) {
        self.keys.insert(name.to_string(), value.to_string());
    }

    /// Remove a key. Returns `true` if it existed.
    pub fn delete(&mut self, name: &str) -> bool {
        self.keys.remove(name).is_some()
    }

    /// Sorted `(name, masked)` pairs for display. Never returns raw values.
    pub fn list_masked(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> =
            self.keys.iter().map(|(k, v)| (k.clone(), mask(v))).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Mask a secret for display: short values are fully redacted; longer ones
/// keep the first and last 4 characters (`sb_l…a9b2`).
pub fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let n = chars.len();
    if n <= 8 {
        return "••••".to_string();
    }
    let first: String = chars[..4].iter().collect();
    let last: String = chars[n - 4..].iter().collect();
    format!("{first}…{last}")
}

/// Keys whose value the process **environment is authoritative for** — an injected
/// env value always wins over anything in keys.json. These are provisioned by the
/// platform (the k3s control plane injects `METALCRAFT_TOKEN` into the pod), so a
/// stale stored key can never shadow the freshly minted, injected token.
pub const ENV_AUTHORITATIVE: &[&str] = &["METALCRAFT_TOKEN"];

/// Whether `name` is platform-managed (env wins, not user-settable via keys.json).
pub fn is_env_authoritative(name: &str) -> bool {
    ENV_AUTHORITATIVE.contains(&name)
}

/// Resolve `name` to a value. **Store-first, then process environment** — except
/// for [`ENV_AUTHORITATIVE`] keys, where a non-empty env value takes precedence so
/// a platform-injected token is never shadowed by a stale keys.json entry. This is
/// the single resolution point for `$VAR` expansion in HTTP-API tools. Returns
/// `None` if set in neither place.
pub fn lookup(name: &str) -> Option<String> {
    let stored = KeyStore::load(&paths::keys_file()).get(name).map(str::to_string);
    let env = std::env::var(name).ok();
    resolve(name, stored, env)
}

/// Pure precedence rule behind [`lookup`], split out so it's unit-testable without
/// touching global env or the on-disk keys.json. For [`ENV_AUTHORITATIVE`] keys a
/// non-empty `env` value wins; otherwise `stored` wins, then `env`.
pub fn resolve(name: &str, stored: Option<String>, env: Option<String>) -> Option<String> {
    if is_env_authoritative(name) {
        if let Some(v) = env.as_deref() {
            if !v.trim().is_empty() {
                return env;
            }
        }
    }
    stored.or(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique temp path per call — no `tempfile` dep, no clashes across tests.
    fn temp_keys_path() -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "metalcraft-keys-test-{}-{}.json",
            std::process::id(),
            n
        ))
    }

    #[test]
    fn load_missing_file_is_empty() {
        let path = temp_keys_path();
        let store = KeyStore::load(&path);
        assert!(store.list_masked().is_empty());
    }

    #[test]
    fn upsert_get_delete_roundtrip() {
        let mut store = KeyStore::default();
        store.upsert("SOLARABASE_API_KEY", "sb_live_secret");
        assert_eq!(store.get("SOLARABASE_API_KEY"), Some("sb_live_secret"));
        assert!(store.contains("SOLARABASE_API_KEY"));
        store.upsert("SOLARABASE_API_KEY", "sb_live_rotated");
        assert_eq!(store.get("SOLARABASE_API_KEY"), Some("sb_live_rotated"));
        assert!(store.delete("SOLARABASE_API_KEY"));
        assert!(!store.delete("SOLARABASE_API_KEY"));
        assert_eq!(store.get("SOLARABASE_API_KEY"), None);
    }

    #[test]
    fn save_then_load_persists() {
        let path = temp_keys_path();
        let mut store = KeyStore::default();
        store.upsert("A", "alpha-value-123");
        store.upsert("B", "beta-value-456");
        store.save(&path).unwrap();

        let loaded = KeyStore::load(&path);
        assert_eq!(loaded.get("A"), Some("alpha-value-123"));
        assert_eq!(loaded.get("B"), Some("beta-value-456"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn list_masked_is_sorted_and_redacted() {
        let mut store = KeyStore::default();
        store.upsert("ZED", "sb_live_longsecretvalue");
        store.upsert("ABE", "short");
        let masked = store.list_masked();
        assert_eq!(masked[0].0, "ABE");
        assert_eq!(masked[1].0, "ZED");
        // No raw value leaks through.
        assert!(!masked[1].1.contains("longsecret"));
    }

    #[test]
    fn mask_short_values_fully_redacted() {
        assert_eq!(mask(""), "••••");
        assert_eq!(mask("12345678"), "••••");
        assert_eq!(mask("sb_live_abcd1234"), "sb_l…1234");
    }

    #[test]
    fn ordinary_key_is_store_first() {
        // A normal key: keys.json wins over env (lets a user override env defaults).
        let got = resolve("OPENAI_API_KEY", Some("stored".into()), Some("from-env".into()));
        assert_eq!(got.as_deref(), Some("stored"));
        // …and falls back to env when unstored.
        let got = resolve("OPENAI_API_KEY", None, Some("from-env".into()));
        assert_eq!(got.as_deref(), Some("from-env"));
    }

    #[test]
    fn metalcraft_token_is_env_authoritative() {
        assert!(is_env_authoritative("METALCRAFT_TOKEN"));
        // The platform-injected env token must beat a stale stored one.
        let got = resolve(
            "METALCRAFT_TOKEN",
            Some("mck_stale_stored".into()),
            Some("mck_injected_by_pod".into()),
        );
        assert_eq!(got.as_deref(), Some("mck_injected_by_pod"));
    }

    #[test]
    fn metalcraft_token_falls_back_to_store_when_env_absent_or_empty() {
        // No env → use the stored one (e.g. a self-hosted user who pasted a key).
        let got = resolve("METALCRAFT_TOKEN", Some("mck_stored".into()), None);
        assert_eq!(got.as_deref(), Some("mck_stored"));
        // Empty/blank env must not shadow a real stored token.
        let got = resolve("METALCRAFT_TOKEN", Some("mck_stored".into()), Some("   ".into()));
        assert_eq!(got.as_deref(), Some("mck_stored"));
    }
}
