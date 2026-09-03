use std::path::{Path, PathBuf};
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
/// `integration_store` for its vendored integrations.
pub fn agent_packs_dir() -> PathBuf {
    data_dir().join("agent_packs")
}

pub fn skills_dir() -> PathBuf {
    data_dir().join("skills")
}

pub fn flows_dir() -> PathBuf {
    data_dir().join("flows")
}

/// Directory holding this agent's [scheduled flows](crate::scheduled_flows) —
/// one JSON per schedule, saying *when* a flow in `flows_dir()` runs.
///
/// Separate from the flows themselves so "what can this agent do" and "what is
/// this agent going to do" are two listings rather than one you have to read
/// carefully.
pub fn scheduled_flows_dir() -> PathBuf {
    data_dir().join("scheduled_flows")
}

/// Directory holding [projects](crate::projects) — one JSON per project, saying what
/// this pod is working towards on its own.
///
/// Separate from `scheduled_flows_dir()` because the two answer different
/// questions: a scheduled flow is a graph someone authored and armed, a project is
/// an outcome someone asked for and left running.
pub fn projects_dir() -> PathBuf {
    data_dir().join("projects")
}

/// One project's own directory: its scratchpad and that scratchpad's snapshots.
///
/// Beside the record rather than inside it because the scratchpad is a document
/// people read and diff, and burying it in a JSON string field would make it
/// neither.
pub fn project_dir(project_id: &str) -> PathBuf {
    projects_dir().join(project_id)
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

pub fn integrations_dir() -> PathBuf {
    data_dir().join("integrations")
}

pub fn integrations_state_file() -> PathBuf {
    data_dir().join("integrations.json")
}

/// Move a pre-0.30 data dir onto the post-rename paths.
///
/// "Integration pack" became "integration" in 0.30, and four paths moved with it.
/// Three of those are load-bearing on an *existing* pod, and the worst is the
/// quietest: an installed agent pack records which store entries it uses in
/// `<data>/agent_packs/<id>/integration_packs.json`, and [`store::read_refs`]
/// degrades a missing file to *no refs*. Upgrade without this and every installed
/// agent pack resolves zero integrations — the agent loses every HTTP tool it had,
/// with nothing in the logs and every file still on disk under its old name.
///
/// **Why this runs on boot**, when `AGENT_PRESETS_PLAN.md` §7 says migration never
/// should: that rule is about *restructuring* — wrapping packs into agent packs,
/// turning chats into instances — where the shape of the data changes and an
/// operator should choose the moment. This changes nothing but a filename. It is
/// idempotent, it never overwrites, and the alternative is a pod that silently
/// stops working until somebody runs a command they have no reason to know about.
///
/// [`store::read_refs`]: crate::agent_packs::store::read_refs
pub fn migrate_legacy_integration_paths() {
    let data = data_dir();

    // Directories and top-level files, first — the manifest pass below walks the
    // renamed directories, so it has to run after them.
    let mut moves: Vec<(PathBuf, PathBuf)> = vec![
        (data.join("pack_store"), data.join("integration_store")),
        (data.join("integration_packs"), data.join("integrations")),
        (
            data.join("integration_packs.json"),
            data.join("integrations.json"),
        ),
        (
            data.join("integration_packs.lock"),
            data.join("integrations.lock"),
        ),
    ];

    // Each installed agent pack's refs file. This is the one that costs an agent
    // its tools, so it is worth walking the directory for.
    if let Ok(entries) = std::fs::read_dir(agent_packs_dir()) {
        for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            moves.push((
                e.path().join("integration_packs.json"),
                e.path().join("integrations.json"),
            ));
        }
    }

    for (from, to) in moves {
        rename_if_free(&from, &to);
    }

    // Then the manifest inside each stored entry and each side-loaded integration.
    // `store::resolve` treats a missing manifest as a missing entry, so skipping
    // this would leave the refs intact and still pointing at nothing.
    //
    // The entry's directory name — its content hash — is deliberately not
    // recomputed. It is an identity assigned at install, never re-derived, and
    // rewriting it would invalidate every ref that already names it.
    for parent in [data.join("integration_store"), data.join("integrations")] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            rename_if_free(
                &e.path().join("pack.json"),
                &e.path().join("integration.json"),
            );
        }
    }
}

/// Rename `from` to `to`, unless `to` already exists.
///
/// Both existing is the case worth refusing rather than resolving: renaming over a
/// live file to tidy up a name is not a trade worth making silently, so say so and
/// leave the operator with both.
fn rename_if_free(from: &Path, to: &Path) {
    if !from.exists() {
        return;
    }
    if to.exists() {
        log::warn!(
            "both {} and {} exist; leaving them alone",
            from.display(),
            to.display()
        );
        return;
    }
    match std::fs::rename(from, to) {
        Ok(()) => log::info!("migrated {} -> {}", from.display(), to.display()),
        Err(e) => log::warn!("could not migrate {}: {e}", from.display()),
    }
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
/// integrations. See [`crate::gateway_channels`].
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

/// Pod-wide preferences at `<data>/pod_settings.json` — see
/// [`crate::pod_settings`]. Distinct from `keys.json`, which holds secrets, and
/// from the environment, which holds deployment facts: this is what the person
/// using the pod chose.
pub fn pod_settings_file() -> PathBuf {
    data_dir().join("pod_settings.json")
}

/// Where each scheduled flow last fired: `{ "<scheduled flow id>": "<rfc3339>" }`
/// at `<data>/schedule_state.json`. See [`crate::schedule_timing`].
///
/// On disk rather than in the daemon's memory because a restart used to erase
/// it, and a schedule with no record of a previous run fires immediately — so
/// "every 24 hours" ran again on every pod roll.
pub fn schedule_state_file() -> PathBuf {
    data_dir().join("schedule_state.json")
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
    memory_dir()
        .join("presets")
        .join(format!("{slug}@{version}"))
}

/// This instance's own memories — the writable delta over its preset base.
pub fn memory_instance_dir(instance_id: &str) -> PathBuf {
    memory_dir().join("instances").join(instance_id)
}

pub fn memory_dir() -> PathBuf {
    data_dir().join("memory")
}

/// Raw turn material awaiting distillation,
/// `<data>/memory/instances/<id>/capture.jsonl`. Written at turn time (one
/// appended line, no LLM call).
///
/// Per-instance, like everything else under `memory/`: the material is *about*
/// the agent that produced it, and a distillation pass would have to split a
/// shared file back apart before it could use a single line of it. See
/// [`crate::memory::capture`].
pub fn memory_capture_file(instance_id: &str) -> PathBuf {
    memory_instance_dir(instance_id).join("capture.jsonl")
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

/// Where one agent's dream journals live,
/// `<data>/memory/instances/<id>/dreams/`. One JSON file per run.
///
/// Per-instance, because a dream is per-instance: the nightly loop iterates
/// active agents and each gets its own run, its own counts, and its own errors.
/// A pod-wide journal would have to be de-interleaved before it could answer
/// "what did *this* agent consolidate last night?", which is the only question
/// anyone asks of it. See [`crate::memory::dream`].
pub fn memory_dreams_dir(instance_id: &str) -> PathBuf {
    memory_instance_dir(instance_id).join("dreams")
}

/// The nightly dream's schedule bookmark, `<data>/memory/dream_state.json`.
///
/// Pod-global rather than per-instance because the *schedule* is pod-global —
/// one cron fires one sweep over every active agent. Persisted for the same
/// reason flow schedules are (see [`schedule_state_file`]): a bookmark held only
/// in RAM makes every pod roll look like a first sighting, and pods roll on
/// every image upgrade.
pub fn memory_dream_state_file() -> PathBuf {
    memory_dir().join("dream_state.json")
}
