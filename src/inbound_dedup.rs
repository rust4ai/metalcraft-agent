//! Idempotency for inbound gateway messages, keyed by a stable id (the gateway's
//! `messages.id` UUID, falling back to the carrier SID).
//!
//! Both inbound transports — the webhook push (`/webhook/gateway`) and the
//! long-poll pull (`GET /inbound/next`) — funnel through one check
//! ([`crate::workshop_api::route_gateway_inbound`]), so a message delivered on
//! both paths (the gateway's `dual` mode) runs the agent only once. It is also
//! **persistent** (on the pod's data dir), so a message re-delivered after a pod
//! restart — e.g. a long-poll pull that wasn't ACKed before the pod rolled — is
//! not processed again.
//!
//! The store is a bounded rolling window: the most recent [`MAX_IDS`] ids. Inbound
//! volume is low (chat messages) and a re-delivery arrives within seconds, so the
//! window never needs to be large. `None`/empty ids can't be deduped and always
//! process (fail-open — better a rare duplicate than a dropped message).
use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

/// How many recent ids to remember. Comfortably larger than any realistic burst
/// of re-deliveries between a message and its ACK.
const MAX_IDS: usize = 2000;

struct Store {
    seen: HashSet<String>,
    /// Insertion order (oldest at the front) for bounded eviction.
    order: VecDeque<String>,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

fn load() -> Store {
    let ids: Vec<String> = std::fs::read_to_string(crate::paths::inbound_dedup_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut order = VecDeque::new();
    for id in ids {
        if seen.insert(id.clone()) {
            order.push_back(id);
        }
    }
    Store { seen, order }
}

/// Atomically write the current window (tmp + rename), like the other data-dir
/// stores. Best-effort: a failed write just means a restart might re-process one
/// message, which the caller already tolerates.
fn persist(store: &Store) {
    let ids: Vec<&String> = store.order.iter().collect();
    let Ok(json) = serde_json::to_string(&ids) else {
        return;
    };
    let path = crate::paths::inbound_dedup_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Record `id` as processed and report whether it was **already** seen.
///
/// Returns `true` for a duplicate (the caller should skip it) and `false` the
/// first time (now recorded). A `None`/empty id can't be deduped and returns
/// `false`. Recording happens up front — before the agent runs — so a duplicate
/// that arrives while the first copy is still being processed is caught.
pub fn is_duplicate(id: Option<&str>) -> bool {
    let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let store = STORE.get_or_init(|| Mutex::new(load()));
    let mut store = store.lock().unwrap_or_else(|e| e.into_inner());
    if store.seen.contains(id) {
        return true;
    }
    store.seen.insert(id.to_string());
    store.order.push_back(id.to_string());
    while store.order.len() > MAX_IDS {
        if let Some(old) = store.order.pop_front() {
            store.seen.remove(&old);
        }
    }
    persist(&store);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_and_empty_never_dedupe() {
        assert!(!is_duplicate(None));
        assert!(!is_duplicate(Some("")));
        assert!(!is_duplicate(Some("   ")));
    }

    #[test]
    fn evicts_oldest_beyond_capacity() {
        let mut store = Store {
            seen: HashSet::new(),
            order: VecDeque::new(),
        };
        for i in 0..(MAX_IDS + 10) {
            let id = format!("id-{i}");
            store.seen.insert(id.clone());
            store.order.push_back(id);
            while store.order.len() > MAX_IDS {
                if let Some(old) = store.order.pop_front() {
                    store.seen.remove(&old);
                }
            }
        }
        assert_eq!(store.order.len(), MAX_IDS);
        assert!(
            !store.seen.contains("id-0"),
            "oldest should have been evicted"
        );
        assert!(store.seen.contains(&format!("id-{}", MAX_IDS + 9)));
    }
}
