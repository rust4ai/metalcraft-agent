use std::path::PathBuf;
use std::sync::OnceLock;

const APP_NAME: &str = "metalcraft-agent";

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Returns the app data root, resolved **once per process** (memoized) so every
/// subsystem — seeding, the flow runtime, the workshop API, the event listener
/// — agrees on a single location. Resolved in order:
/// 1. `METALCRAFT_DATA_DIR` env var (explicit override)
/// 2. OS app data dir via `dirs::data_dir()` (~/.local/share/metalcraft-agent on Linux)
/// 3. `./data` fallback (useful in containers where HOME may not be set)
///
/// IMPORTANT: load your `.env` (`dotenvy::dotenv()`) before the first call, or a
/// file-provided `METALCRAFT_DATA_DIR` won't be seen and seeding will land in
/// the fallback dir while the runtime reads from the override. The binaries'
/// `main()` load dotenv as their first statement for exactly this reason.
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            let (dir, source) = if let Ok(custom) = std::env::var("METALCRAFT_DATA_DIR") {
                (PathBuf::from(custom), "METALCRAFT_DATA_DIR")
            } else if let Some(os_dir) = dirs::data_dir() {
                (os_dir.join(APP_NAME), "OS data dir")
            } else {
                (PathBuf::from("data"), "./data fallback")
            };
            log::info!("metalcraft data dir: {} (via {source})", dir.display());
            dir
        })
        .clone()
}

pub fn personas_dir() -> PathBuf {
    data_dir().join("personas")
}

/// User-authored agent presets. Pack-provided presets live under each pack's
/// `agent_presets/` subdir and are layered on top of this one.
pub fn agent_presets_dir() -> PathBuf {
    data_dir().join("agent_presets")
}

/// Agent instances — one directory per live agent, holding its record (and, later,
/// its memory namespace). Conversations stay in `chats_dir()`; an instance groups them.
pub fn agent_instances_dir() -> PathBuf {
    data_dir().join("agent_instances")
}

/// Installed agent packs — the unit of installation. Each holds its manifest, the
/// presets/personas/skills it provides, and a map into the content-addressed
/// `pack_store` for its vendored integration packs.
pub fn agent_packs_dir() -> PathBuf {
    data_dir().join("agent_packs")
}

pub fn skills_dir() -> PathBuf {
    data_dir().join("skills")
}

pub fn flows_dir() -> PathBuf {
    data_dir().join("flows")
}

/// Directory holding in-flight and finished flow runs (one JSON per run),
/// used by the v2 executor for pause/resume checkpointing.
pub fn runs_dir() -> PathBuf {
    data_dir().join("runs")
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// OpenTelemetry trace output, one `<session>/otlp-trace.json` per chat
/// session. Sits beside [`sessions_dir`] (the bespoke diagnostics logs) and
/// shares the same `<session>` directory name, so a diagnostics session and
/// its OTLP trace line up 1:1.
pub fn traces_dir() -> PathBuf {
    data_dir().join("traces")
}

pub fn api_tools_dir() -> PathBuf {
    data_dir().join("api_tools")
}

pub fn flow_templates_dir() -> PathBuf {
    data_dir().join("flow_templates")
}

pub fn chats_dir() -> PathBuf {
    data_dir().join("chats")
}

pub fn integration_packs_dir() -> PathBuf {
    data_dir().join("integration_packs")
}

pub fn integration_packs_state_file() -> PathBuf {
    data_dir().join("integration_packs.json")
}

/// One-shot marker written after the daemon auto-enables the Metalcraft
/// ecosystem packs on a managed pod (gated by `ENABLE_METALCRAFT_PACKS`). Its
/// presence is what stops the seed from re-running on later boots — the env var
/// stays set forever, this file makes the *action* fire exactly once. Lives on
/// the pod's persistent volume, so the one-shot holds across restarts; only a
/// wiped/reprovisioned volume (a genuinely fresh pod) re-seeds.
pub fn ecosystem_packs_seeded_marker() -> PathBuf {
    data_dir().join(".metalcraft_packs_seeded")
}

/// Directory of installed gateway *channel types* — declarative JSON manifests
/// (one `<id>/channel_type.json` per type), seeded from the binary like
/// integration packs. See [`crate::gateway_channels`].
pub fn gateway_channels_dir() -> PathBuf {
    data_dir().join("gateway_channels")
}

/// Persisted gateway *channel instances* — user-created, named configurations
/// of a channel type (WhatsApp number, target persona, enabled flag). A JSON
/// array at `<data>/gateway_channels.json`.
pub fn gateway_channels_state_file() -> PathBuf {
    data_dir().join("gateway_channels.json")
}

/// Outbound **channels** — the simple connection model `{ slug, name, url }`
/// that outbound sends resolve against (see [`crate::channels`]). The default
/// `metalcraft` channel is synthesized, not stored; this file holds only the
/// user-added custom channels. A JSON array at `<data>/channels.json`.
pub fn channels_state_file() -> PathBuf {
    data_dir().join("channels.json")
}

/// Plaintext API-key / secret store (see [`crate::key_store`]).
pub fn keys_file() -> PathBuf {
    data_dir().join("keys.json")
}

/// Persistent record of recently-processed inbound message ids (gateway dedup).
/// See [`crate::inbound_dedup`].
pub fn inbound_dedup_file() -> PathBuf {
    data_dir().join("inbound_dedup.json")
}

/// Persisted scheduled follow-up tasks — deferred subagent jobs the agent
/// arms via `schedule_followup`, fired by the daemon when due. A JSON array at
/// `<data>/scheduled_tasks.json`. See [`crate::scheduled_tasks`].
pub fn scheduled_tasks_file() -> PathBuf {
    data_dir().join("scheduled_tasks.json")
}

/// Root for the agent's long-term memory: `<data>/memory/`. See
/// [`crate::memory`] and `docs/MEMORY_SYSTEM_PLAN.md`.
/// Shared, immutable base memory for one published agent-preset version. Built once
/// at install and read by every instance of that preset, so twenty agents cost one
/// copy on disk and one in RAM.
pub fn memory_preset_dir(slug: &str, version: &str) -> PathBuf {
    memory_dir().join("presets").join(format!("{slug}@{version}"))
}

/// This instance's own memories — the writable delta over its preset base.
pub fn memory_instance_dir(instance_id: &str) -> PathBuf {
    memory_dir().join("instances").join(instance_id)
}

pub fn memory_dir() -> PathBuf {
    data_dir().join("memory")
}

/// Periodic full-state file, `<data>/memory/snapshot.json`. Written atomically
/// (tmp + rename) by the compaction pass; read first on boot.
pub fn memory_snapshot_file() -> PathBuf {
    memory_dir().join("snapshot.json")
}

/// Append-only event log, `<data>/memory/wal.jsonl`. Holds everything written
/// since the last snapshot; replayed on boot and folded back into the snapshot by
/// compaction. Append-only because recall bumps access times on every turn, and
/// rewriting the whole store for that would be O(n) per turn.
pub fn memory_wal_file() -> PathBuf {
    memory_dir().join("wal.jsonl")
}

/// Embedding sidecar, `<data>/memory/vectors.bin`. Append-only fixed-shape
/// binary records rather than JSON, because a 384-dim `f32` vector is 1.5 KB of
/// data no human reads and base64 in the log would roughly double it. Rewritten
/// (compacted) alongside the snapshot.
pub fn memory_vectors_file() -> PathBuf {
    memory_dir().join("vectors.bin")
}

/// Raw turn material awaiting distillation, `<data>/memory/capture.jsonl`.
/// Written at turn time (one appended line, no LLM call) and drained by the
/// nightly dream. See [`crate::memory::capture`].
pub fn memory_capture_file() -> PathBuf {
    memory_dir().join("capture.jsonl")
}

/// Root directory that document-upload tools (multipart HTTP-API tools) may
/// read local files from. The multipart body builder refuses any path that
/// resolves outside this tree, so a tool-calling model can't be steered into
/// uploading arbitrary local files (SSH keys, `.env`, …). Override with
/// `METALCRAFT_UPLOAD_ROOT`; defaults to `<data>/uploads`.
pub fn upload_root() -> PathBuf {
    std::env::var("METALCRAFT_UPLOAD_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir().join("uploads"))
}
