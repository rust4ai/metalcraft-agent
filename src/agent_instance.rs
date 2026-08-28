//! Agent instances — a live agent, created from an [`AgentPreset`], holding its own
//! identity and **many conversations**.
//!
//! The distinction that earns its keep is instance ≠ conversation. A gateway session
//! resets its whole state on idle (`workshop_api.rs`, `DEFAULT_GATEWAY_SESSION_TTL_SECS`);
//! if an instance were a single conversation, an agent would forget you between text
//! messages. Instead the idle reset ends a *conversation* and the instance carries on —
//! which is what makes per-instance memory (see `docs/AGENT_PRESETS_PLAN.md` §3) worth
//! having at all.
//!
//! **Conversations are still stored as chats.** A conversation's messages live where
//! they always have, in `<data>/chats/<id>.json`; the chat record simply gained an
//! `instance_id`. That keeps the whole turn path untouched — this module adds grouping
//! and identity, not a second copy of the transcript.
//!
//! Note the name collision: `workshop_api::SessionPreset` is the session's *I/O type*
//! (workshop chat vs gateway) and has nothing to do with an agent preset.
//!
//! [`AgentPreset`]: crate::agent_preset::AgentPreset
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

/// Where an instance came from.
///
/// Provenance only, since agents are never deleted on a timer — it answers "why
/// does this exist" (and, for `Gateway`, *who* it belongs to), not "how long does
/// it last".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstanceOrigin {
    Workshop,
    Cli,
    /// A conversation with one person on one channel.
    ///
    /// Keyed by sender, not just by channel: an agent's memory is the continuity
    /// it exists to have, and one agent per *number* meant everything it learned
    /// about everyone who texted in went into a single shared memory. `sender` is
    /// the normalized key (see `gateway_sender_key`), not the raw `From` — the
    /// same person must not get a second agent because their number arrived
    /// formatted differently.
    ///
    /// `None` on records written before this was per-sender; they keep working as
    /// the channel's catch-all agent until they age out.
    Gateway {
        channel: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
    },
    Flow {
        flow_id: String,
    },
}

impl Default for InstanceOrigin {
    fn default() -> Self {
        Self::Workshop
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentInstance {
    pub id: String,
    /// The agent preset this instance was created from. **Immutable for its life** —
    /// its memory is seeded from that preset, so swapping it mid-life is incoherent.
    /// Switching agents means starting a new instance.
    pub agent_preset: String,
    /// The agent pack that provided the preset, when it came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pack: Option<String>,
    /// Diagnostics only. Personas and skills **follow** the installed pack version
    /// (see the plan's §5.4); this records what it was born against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_from_version: Option<String>,
    pub name: String,
    /// The instance's current persona — starts at the preset's default and moves
    /// within its roster.
    pub persona: String,
    #[serde(default)]
    pub origin: InstanceOrigin,
    /// Set when the agent pack that provided this agent's preset withdrew it — on
    /// update or on a forced uninstall. The agent keeps its memory and its
    /// conversations and goes on working against a frozen copy of the preset; this
    /// records what happened, so a UI can say so rather than presenting a
    /// pack-provided agent that is quietly no longer pack-provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_from: Option<String>,
    /// Set when an update withdrew the persona this agent was using and it fell back
    /// to the preset's default. Reported rather than silent: the agent's voice
    /// changed, and nobody asked for that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_fallback_from: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub last_active_at: String,
}

impl AgentInstance {
    /// Mint an instance of `preset`. The persona is the preset's default; the name is
    /// the preset's display name until someone renames it.
    pub fn new(preset: &crate::agent_preset::AgentPreset, origin: InstanceOrigin) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: format!("inst_{}", uuid::Uuid::new_v4().simple()),
            agent_preset: preset.slug.clone(),
            agent_pack: None,
            created_from_version: preset.version.clone(),
            name: preset.name.clone(),
            persona: preset.default_persona.clone(),
            origin,
            orphaned_from: None,
            persona_fallback_from: None,
            created_at: now.clone(),
            last_active_at: now,
        }
    }

    pub fn dir(&self) -> PathBuf {
        instance_dir(&self.id)
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = self.dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        let path = dir.join("instance.json");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize instance: {e}"))?;
        // tmp + rename, the atomic-write idiom used by key_store and scheduled_tasks:
        // an interrupted write must never truncate a good file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("failed to finalize {}: {e}", path.display()))
    }

    pub fn touch(&mut self) {
        self.last_active_at = chrono::Utc::now().to_rfc3339();
    }
}

pub fn instances_root() -> PathBuf {
    paths::agent_instances_dir()
}

pub fn instance_dir(id: &str) -> PathBuf {
    instances_root().join(id)
}

pub fn load(id: &str) -> Result<AgentInstance, String> {
    let path = instance_dir(id).join("instance.json");
    let content =
        std::fs::read_to_string(&path).map_err(|_| format!("agent instance '{id}' not found"))?;
    serde_json::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// Every instance on the pod, newest activity first. A malformed record is skipped
/// with a warning rather than failing the listing — one bad file must not hide the rest.
pub fn list() -> Vec<AgentInstance> {
    let root = instances_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<AgentInstance> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            match load(&id) {
                Ok(i) => Some(i),
                Err(err) => {
                    log::warn!("skipping agent instance '{id}': {err}");
                    None
                }
            }
        })
        .collect();
    out.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    out
}

/// Delete an instance record, and everything only it owned.
///
/// Conversations are *not* touched — a caller that wants them gone deletes them
/// explicitly, so a mistaken delete never destroys transcripts. Its **memory** is,
/// because that is the thing an agent is: leaving it behind means a later instance
/// that happened to reuse the id would inherit a stranger's recollections, and until
/// then it is unreachable bytes.
///
/// Evicting from the resident set is not optional. Without it the deleted agent stays
/// in RAM answering recalls, holds a memory base alive, and occupies one of the eight
/// LRU slots for the life of the process.
pub fn delete(id: &str) -> Result<(), String> {
    let dir = instance_dir(id);
    if !dir.is_dir() {
        return Err(format!("agent instance '{id}' not found"));
    }
    crate::memory::instance::evict(id);
    let mem = paths::memory_instance_dir(id);
    if mem.is_dir()
        && let Err(e) = std::fs::remove_dir_all(&mem)
    {
        // The record is what makes the agent exist; orphaned memory is waste, not a
        // failure. Warn rather than refuse a delete the user asked for.
        log::warn!(
            "agent instance '{id}': could not remove {}: {e}",
            mem.display()
        );
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("failed to delete {}: {e}", dir.display()))
}

// Agents are **never** deleted on a timer.
//
// There used to be a seven-day sweep over "ephemeral" instances here, guarded by
// a `persistent` flag that naming an agent used to set. It is gone, and so is the
// flag: an agent is the memory of a relationship, and the cost of keeping one
// nobody has spoken to in a fortnight is a directory entry, while the cost of
// deleting it is everything it had learned. Only an explicit [`delete`] removes
// one.
//
// What *is* swept is sessions — `workshop_api::reap_stale_chats`, at 30 days of
// inactivity. A transcript ages out; the agent that wrote it does not.

/// Find the agent that talks to `sender` on `channel`, or mint one.
///
/// This is what gives the relationship continuity across conversations: a session
/// ends after a quiet gap, the agent does not. Everything it learns about this
/// person accumulates in one memory (`memory/instances/{id}`) and is shared by
/// every session it has ever had with them.
///
/// `label` is what to call the agent when minting — the sender's display name if
/// the channel gave us one, otherwise their address.
pub fn for_gateway_sender(
    channel: &str,
    sender: &str,
    label: &str,
    preset_slug: &str,
) -> Result<AgentInstance, String> {
    let origin = InstanceOrigin::Gateway {
        channel: channel.to_string(),
        sender: Some(sender.to_string()),
    };
    // An agent minted before this was per-sender answered for the whole channel.
    // Adopt it for the first sender that comes back rather than stranding a real
    // memory beside a brand-new empty one; later senders get their own.
    let legacy = InstanceOrigin::Gateway {
        channel: channel.to_string(),
        sender: None,
    };
    let existing = list()
        .into_iter()
        .find(|i| i.origin == origin)
        .or_else(|| list().into_iter().find(|i| i.origin == legacy));
    if let Some(mut found) = existing {
        // The existing agent wins even if the channel has since been pointed at a
        // different preset — its memories are the continuity the channel exists to
        // have, and silently swapping them out is not a re-point, it is amnesia. Say
        // so rather than ignoring the argument, which is what this used to do.
        if found.agent_preset != preset_slug {
            log::warn!(
                "channel '{channel}' is configured for agent '{preset_slug}' but {} already \
                 has an agent of '{}' ({}). Keeping it — delete that agent to start fresh.",
                sender,
                found.agent_preset,
                found.id
            );
        }
        if found.origin == legacy {
            found.origin = origin;
            found.save()?;
        }
        return Ok(found);
    }
    let preset = crate::agent_preset::AgentPreset::load(preset_slug, &paths::agent_presets_dir())?;
    preset.ensure_spawnable()?;
    let mut instance = AgentInstance::new(&preset, origin);
    instance.name = format!("{} — {label}", preset.name);
    instance.save()?;
    Ok(instance)
}

/// The agent a flow runs as, minting one the first time the flow needs it.
///
/// **One agent per flow, and it exists as soon as the flow has run once.** It
/// used to be arming that created it, which meant a flow you only ever pressed
/// Run on had no agent, left no conversation, and appeared nowhere — the work
/// happened and the pod's own home screen showed nothing for it. A run is the
/// act that says this flow does something; that is the honest moment for its
/// agent to exist.
///
/// Its runs then accumulate in one memory, which is the point of the agent
/// rather than a side effect: the flow that ran this morning can notice it said
/// the same thing yesterday. Schedules of the same flow land here too
/// ([`crate::scheduled_flows::arm`]), so arming a flow somebody had already run
/// by hand adopts the agent it already had instead of minting a second.
///
/// `label` is what to call it when minting — the schedule's name if it has one,
/// otherwise the flow's.
pub fn for_flow(flow_id: &str, label: &str, preset_slug: &str) -> Result<AgentInstance, String> {
    let origin = InstanceOrigin::Flow {
        flow_id: flow_id.to_string(),
    };
    // `list()` is newest-active first, so a flow that somehow has two agents
    // (one minted before this existed, one after) resolves to the one being used.
    if let Some(found) = list().into_iter().find(|i| i.origin == origin) {
        return Ok(found);
    }
    let preset = crate::agent_preset::AgentPreset::load(preset_slug, &paths::agent_presets_dir())?;
    preset.ensure_spawnable()?;
    let mut instance = AgentInstance::new(&preset, origin);
    instance.name = format!("{} — {label}", preset.name);
    instance.save()?;
    Ok(instance)
}

/// One-time backfill: give every legacy chat an instance so nothing on an upgraded pod
/// is orphaned.
///
/// Chats predate presets, so they bind to whichever preset declares their persona, or
/// to the default.
pub fn backfill_from_chats(chats_dir: &Path) -> Result<BackfillReport, String> {
    let mut report = BackfillReport::default();
    let Ok(entries) = std::fs::read_dir(chats_dir) else {
        return Ok(report);
    };

    let presets_dir = paths::agent_presets_dir();
    let summaries = crate::agent_preset::AgentPreset::list_summaries(&presets_dir);

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&content) else {
            report.skipped += 1;
            continue;
        };
        if doc.get("instance_id").and_then(|v| v.as_str()).is_some() {
            report.already_bound += 1;
            continue;
        }

        let persona = doc
            .get("persona_slug")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Prefer a preset that actually declares this persona; fall back to the default.
        // A library preset can be the one that declares it — a pack's specialist has
        // to live somewhere — but binding a chat to one would mint the instance the
        // library exists not to have.
        let preset_slug = summaries
            .iter()
            .filter(|s| !s.library)
            .find(|s| s.default_persona == persona)
            .map(|s| s.slug.clone())
            .unwrap_or_else(|| crate::agent_preset::DEFAULT_PRESET.to_string());

        let preset = match crate::agent_preset::AgentPreset::load(&preset_slug, &presets_dir) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("backfill: {e}");
                report.skipped += 1;
                continue;
            }
        };

        let mut instance = AgentInstance::new(&preset, InstanceOrigin::Workshop);
        if !persona.is_empty() {
            instance.persona = persona.to_string();
        }
        if let Some(created) = doc.get("created_at").and_then(|v| v.as_str()) {
            instance.created_at = created.to_string();
            instance.last_active_at = created.to_string();
        }
        instance.save()?;

        doc["instance_id"] = serde_json::Value::String(instance.id.clone());
        let json = serde_json::to_string_pretty(&doc)
            .map_err(|e| format!("failed to serialize chat: {e}"))?;
        std::fs::write(&path, json)
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        report.migrated += 1;
    }
    Ok(report)
}

#[derive(Debug, Default, Clone, Serialize, utoipa::ToSchema)]
pub struct BackfillReport {
    pub migrated: usize,
    pub already_bound: usize,
    pub skipped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_preset::AgentPreset;

    fn preset() -> AgentPreset {
        serde_json::from_str(
            r#"{"slug":"amy-kitchen","name":"Amy's Kitchen Agent","default_persona":"amy",
                "version":"1.4.0",
                "personas":[{"slug":"amy","role":"default"},{"slug":"amy-shopper"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_new_instance_takes_the_presets_identity() {
        let i = AgentInstance::new(&preset(), InstanceOrigin::Workshop);
        assert_eq!(i.agent_preset, "amy-kitchen");
        assert_eq!(i.persona, "amy", "starts as the preset's default persona");
        assert_eq!(i.name, "Amy's Kitchen Agent");
        assert_eq!(i.created_from_version.as_deref(), Some("1.4.0"));
        assert!(i.id.starts_with("inst_"));
    }

    #[test]
    fn an_agent_has_no_lifetime_flag_to_get_wrong() {
        // The property that replaced `persistent`: nothing about where an agent
        // came from shortens its life, because nothing deletes one on a timer.
        // Sessions age out (`workshop_api::reap_stale_chats`); agents do not.
        for origin in [
            InstanceOrigin::Workshop,
            InstanceOrigin::Cli,
            InstanceOrigin::Gateway {
                channel: "sms".into(),
                sender: Some("gw-sms-15550001234".into()),
            },
            InstanceOrigin::Flow {
                flow_id: "brief".into(),
            },
        ] {
            let json = serde_json::to_value(AgentInstance::new(&preset(), origin.clone())).unwrap();
            assert!(
                json.get("persistent").is_none(),
                "no lifetime flag on the wire for {origin:?}"
            );
        }
    }

    #[test]
    fn origin_round_trips_through_json() {
        let o = InstanceOrigin::Gateway {
            channel: "sms-amy".into(),
            sender: Some("gw-sms-amy-15550001234".into()),
        };
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<InstanceOrigin>(&json).unwrap(), o);
        // An old record with no origin still loads.
        let legacy: InstanceOrigin = serde_json::from_str(r#"{"kind":"workshop"}"#).unwrap();
        assert_eq!(legacy, InstanceOrigin::Workshop);
        // So does a gateway agent minted before they were per-sender: it stays
        // the channel's catch-all until a sender adopts it.
        let legacy: InstanceOrigin =
            serde_json::from_str(r#"{"kind":"gateway","channel":"sms-amy"}"#).unwrap();
        assert_eq!(
            legacy,
            InstanceOrigin::Gateway {
                channel: "sms-amy".into(),
                sender: None,
            }
        );
    }

    #[test]
    fn instances_are_distinct_per_mint() {
        let a = AgentInstance::new(&preset(), InstanceOrigin::Workshop);
        let b = AgentInstance::new(&preset(), InstanceOrigin::Workshop);
        assert_ne!(a.id, b.id, "every chat gets its own agent");
    }
}
