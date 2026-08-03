//! Plaintext API-key / secret store, **scoped**.
//!
//! Secrets live under a [`KeyScope`]: `Global` (the classic account-wide keys
//! like `OPENAI_API_KEY`) or `Channel(id)` (secrets owned by one gateway channel
//! instance, e.g. its `WEBHOOK_SECRET`). Because the scope disambiguates, a
//! channel's secrets get short, clean names — no transport-specific prefixes.
//!
//! Persisted as JSON at `<data>/keys.json` in the **v2 schema**:
//! ```json
//! { "version": 2,
//!   "global":   { "OPENAI_API_KEY": "sk-…" },
//!   "channels": { "<channel-id>": { "WEBHOOK_SECRET": "whsec_…" } } }
//! ```
//! A pre-v2 file (a flat top-level `name → value` object, no `version`) is read
//! transparently: every entry migrates into `Global`. The upgrade is persisted
//! on the next [`save`](KeyStore::save).
//!
//! Values are referenced from HTTP-API tool configs via `$NAME` placeholders
//! (see [`crate::tools::http_api`]); [`lookup`] resolves a **global** name and
//! falls back to a process environment variable, so existing env-based config
//! keeps working. Adapters running in a channel context use [`lookup_scoped`],
//! which checks the channel scope first, then global, then env.
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
use std::collections::BTreeMap;
use std::path::Path;

use crate::paths;

/// Which namespace a key belongs to. Extensible — `Persona`/`Pack` scopes can be
/// added without touching the on-disk layout beyond a new `channels`-like map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyScope {
    /// Account-wide keys (the classic flat namespace).
    Global,
    /// Secrets owned by a single gateway channel instance, keyed by channel id.
    Channel(String),
}

const CURRENT_VERSION: u32 = 2;

fn current_version() -> u32 {
    CURRENT_VERSION
}

/// Scoped secret store. `global` is the account-wide namespace; `channels` maps a
/// channel-instance id to that channel's own secrets. Serialized as the v2 JSON
/// object documented on the module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyStore {
    #[serde(default = "current_version")]
    version: u32,
    #[serde(default)]
    global: BTreeMap<String, String>,
    #[serde(default)]
    channels: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for KeyStore {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            global: BTreeMap::new(),
            channels: BTreeMap::new(),
        }
    }
}

impl KeyStore {
    /// Read the store from `path`. A missing or empty file yields an empty store;
    /// a malformed file is logged and treated as empty (so a corrupt keys.json
    /// never bricks the daemon). A **pre-v2 flat file** (no `version` field) is
    /// migrated in-memory into the `Global` scope — persisted on the next
    /// [`save`](Self::save).
    pub fn load(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        if content.trim().is_empty() {
            return Self::default();
        }
        let value: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("keys.json is malformed, ignoring: {e}");
                return Self::default();
            }
        };
        // v2 files carry a numeric `version`; anything else is the legacy flat map.
        if value.get("version").and_then(|v| v.as_u64()).is_some() {
            match serde_json::from_value::<KeyStore>(value) {
                Ok(store) => store,
                Err(e) => {
                    log::warn!("keys.json (v2) is malformed, ignoring: {e}");
                    Self::default()
                }
            }
        } else {
            Self::from_legacy(value)
        }
    }

    /// Migrate a pre-v2 flat `{ name: value }` object into a v2 store with every
    /// entry under `Global`. Non-string values are skipped defensively.
    fn from_legacy(value: serde_json::Value) -> Self {
        let mut global = BTreeMap::new();
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    global.insert(k.clone(), s.to_string());
                }
            }
        }
        log::info!("keys.json: migrated {} legacy key(s) into the global scope", global.len());
        Self { version: CURRENT_VERSION, global, channels: BTreeMap::new() }
    }

    /// Write the store to `path` atomically (write a sibling `.tmp` then rename)
    /// so a crash mid-write can't leave a truncated keys.json. Always writes the
    /// current schema version.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    // ── Global scope (classic API — callers unchanged) ───────────────────────

    pub fn get(&self, name: &str) -> Option<&str> {
        self.global.get(name).map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.global.contains_key(name)
    }

    /// Insert or replace a **global** key by name.
    pub fn upsert(&mut self, name: &str, value: &str) {
        self.global.insert(name.to_string(), value.to_string());
    }

    /// Remove a **global** key. Returns `true` if it existed.
    pub fn delete(&mut self, name: &str) -> bool {
        self.global.remove(name).is_some()
    }

    /// Sorted `(name, masked)` pairs for the **global** scope. Never returns raw
    /// values. (The scope-aware Keys UI uses [`list_scoped`](Self::list_scoped).)
    pub fn list_masked(&self) -> Vec<(String, String)> {
        self.global.iter().map(|(k, v)| (k.clone(), mask(v))).collect()
    }

    // ── Channel scope ────────────────────────────────────────────────────────

    /// Read a channel-scoped secret's raw value.
    pub fn get_channel(&self, channel_id: &str, name: &str) -> Option<&str> {
        self.channels.get(channel_id).and_then(|m| m.get(name)).map(String::as_str)
    }

    /// Insert or replace a channel-scoped secret.
    pub fn upsert_channel(&mut self, channel_id: &str, name: &str, value: &str) {
        self.channels
            .entry(channel_id.to_string())
            .or_default()
            .insert(name.to_string(), value.to_string());
    }

    /// Remove a single channel-scoped secret. Returns `true` if it existed.
    pub fn delete_channel_key(&mut self, channel_id: &str, name: &str) -> bool {
        match self.channels.get_mut(channel_id) {
            Some(m) => {
                let removed = m.remove(name).is_some();
                if m.is_empty() {
                    self.channels.remove(channel_id);
                }
                removed
            }
            None => false,
        }
    }

    /// Remove **all** secrets for a channel (cascade on channel delete). Returns
    /// `true` if the channel had any.
    pub fn delete_channel(&mut self, channel_id: &str) -> bool {
        self.channels.remove(channel_id).is_some()
    }

    /// Sorted secret names configured for a channel (no values).
    pub fn channel_secret_names(&self, channel_id: &str) -> Vec<String> {
        self.channels
            .get(channel_id)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    // ── Scoped resolution + listing ──────────────────────────────────────────

    /// Read a raw value in a given scope (no env fallback). Prefer
    /// [`lookup_scoped`] for adapters that want the env/global fallback chain.
    pub fn get_scoped(&self, scope: &KeyScope, name: &str) -> Option<&str> {
        match scope {
            KeyScope::Global => self.get(name),
            KeyScope::Channel(id) => self.get_channel(id, name),
        }
    }

    /// Every stored key as `(scope, name, masked)`, global first then channels,
    /// each group sorted by name. For the scope-aware Keys UI. Never raw values.
    pub fn list_scoped(&self) -> Vec<(KeyScope, String, String)> {
        let mut out: Vec<(KeyScope, String, String)> = self
            .global
            .iter()
            .map(|(k, v)| (KeyScope::Global, k.clone(), mask(v)))
            .collect();
        for (id, m) in &self.channels {
            for (k, v) in m {
                out.push((KeyScope::Channel(id.clone()), k.clone(), mask(v)));
            }
        }
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

/// Resolve a **global** `name` to a value. **Store-first, then process
/// environment** — except for [`ENV_AUTHORITATIVE`] keys, where a non-empty env
/// value takes precedence so a platform-injected token is never shadowed by a
/// stale keys.json entry. This is the single resolution point for `$VAR`
/// expansion in HTTP-API tools. Returns `None` if set in neither place.
pub fn lookup(name: &str) -> Option<String> {
    let stored = KeyStore::load(&paths::keys_file()).get(name).map(str::to_string);
    let env = std::env::var(name).ok();
    resolve(name, stored, env)
}

/// Resolve `name` for a channel context: **channel scope, then global, then
/// process env**. When `channel_id` is `None` this is exactly [`lookup`].
/// Env-authoritative keys keep their global precedence rule.
pub fn lookup_scoped(channel_id: Option<&str>, name: &str) -> Option<String> {
    if let Some(id) = channel_id {
        let store = KeyStore::load(&paths::keys_file());
        if let Some(v) = store.get_channel(id, name).filter(|s| !s.is_empty()) {
            return Some(v.to_string());
        }
    }
    lookup(name)
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
        assert!(store.list_scoped().is_empty());
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
        store.upsert_channel("chan-1", "WEBHOOK_SECRET", "whsec_channel_scoped_value");
        store.save(&path).unwrap();

        let loaded = KeyStore::load(&path);
        assert_eq!(loaded.get("A"), Some("alpha-value-123"));
        assert_eq!(loaded.get("B"), Some("beta-value-456"));
        assert_eq!(loaded.get_channel("chan-1", "WEBHOOK_SECRET"), Some("whsec_channel_scoped_value"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn channel_scope_is_isolated_from_global_and_other_channels() {
        let mut store = KeyStore::default();
        store.upsert("API_KEY", "global-key");
        store.upsert_channel("chan-1", "API_KEY", "chan1-key");
        store.upsert_channel("chan-2", "API_KEY", "chan2-key");
        assert_eq!(store.get("API_KEY"), Some("global-key"));
        assert_eq!(store.get_channel("chan-1", "API_KEY"), Some("chan1-key"));
        assert_eq!(store.get_channel("chan-2", "API_KEY"), Some("chan2-key"));
        // Global list never leaks channel scope.
        assert_eq!(store.list_masked().len(), 1);
        assert_eq!(store.list_scoped().len(), 3);
    }

    #[test]
    fn delete_channel_key_prunes_empty_channel() {
        let mut store = KeyStore::default();
        store.upsert_channel("chan-1", "ONLY", "v-longenough");
        assert!(store.delete_channel_key("chan-1", "ONLY"));
        // The now-empty channel map is pruned, so a whole-channel delete is a no-op.
        assert!(!store.delete_channel("chan-1"));
    }

    #[test]
    fn delete_channel_cascades_all_secrets() {
        let mut store = KeyStore::default();
        store.upsert_channel("chan-1", "API_KEY", "a-longenough");
        store.upsert_channel("chan-1", "WEBHOOK_SECRET", "b-longenough");
        assert!(store.delete_channel("chan-1"));
        assert!(store.channel_secret_names("chan-1").is_empty());
    }

    #[test]
    fn legacy_flat_file_migrates_into_global() {
        let path = temp_keys_path();
        // Old schema: bare top-level name→value, no "version".
        std::fs::write(&path, r#"{ "OPENAI_API_KEY": "sk-legacy-value-123", "FOO": "barbazqux" }"#).unwrap();
        let store = KeyStore::load(&path);
        assert_eq!(store.get("OPENAI_API_KEY"), Some("sk-legacy-value-123"));
        assert_eq!(store.get("FOO"), Some("barbazqux"));
        assert!(store.channels_is_empty_for_test());
        // Re-saving upgrades the on-disk format to v2.
        store.save(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\""));
        assert!(raw.contains("\"global\""));
        std::fs::remove_file(&path).ok();
    }

    // Test-only helper to assert no channel scopes exist.
    impl KeyStore {
        fn channels_is_empty_for_test(&self) -> bool {
            self.channels.is_empty()
        }
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
