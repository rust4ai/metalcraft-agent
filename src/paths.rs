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

pub fn skills_dir() -> PathBuf {
    data_dir().join("skills")
}

pub fn flows_dir() -> PathBuf {
    data_dir().join("flows")
}

/// Root for pod-native "agent OS" apps (Plan B). Each app owns
/// `<data>/apps/<id>/` for its SQLite database, blob store, and scratch —
/// isolated from the agent's own JSON state and from other apps.
pub fn apps_dir() -> PathBuf {
    data_dir().join("apps")
}

/// This app's private data directory, `<data>/apps/<id>/`.
pub fn app_data_dir(app_id: &str) -> PathBuf {
    apps_dir().join(app_id)
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
