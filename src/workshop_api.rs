use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, patch, post, put},
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use crate::approval::ApprovalMode;
use crate::diagnostics::{DiagnosticsLogger, SessionInfo};
use crate::diagnostics_browse::{
    DiagnosticsSessionSummary, list_diagnostics_sessions, read_diagnostics_session,
};
use crate::factory_reset::{ResetFailure, ResetReport, ResetScope, RestartExpectation};
use crate::flows;
use crate::paths;
use crate::persona::{Persona, PersonaSummary};
use crate::runtime::{AgentRuntimeContext, RuntimeOptions};
use crate::session_io::SessionPreset;
use crate::skill::{Skill, SkillSummary, list_skill_summaries, load_skill, save_skill};
use crate::tools::http_api::HttpApiToolConfig;
use crate::trace::TraceLogger;
use metalcraft::{AgentMessage, AgentState, GuardAction, RunOutcome, StepGuard};

/// Configuration for the workshop API server.
pub struct WorkshopApiConfig {
    pub port: u16,
    pub api_key: String,
}

/// Active chat sessions, keyed by chat id. Lost on restart — chats live for
/// the lifetime of the daemon process.
type ChatStore = Arc<Mutex<HashMap<String, Arc<Mutex<ChatSession>>>>>;

/// Process-global chat store, shared by the HTTP handlers and the daemon's
/// scheduled-follow-up runner (which lives outside any request but must reach
/// the same sessions to deliver a fired follow-up into its chat). Rehydrated
/// from disk on first access.
fn chat_store() -> ChatStore {
    static STORE: std::sync::OnceLock<ChatStore> = std::sync::OnceLock::new();
    STORE
        .get_or_init(|| {
            // Give every pre-instance chat an agent before anything reads them, so
            // nothing on an upgraded pod is orphaned. Idempotent and best-effort:
            // a failure here must not stop the API from serving.
            match crate::agent_instance::backfill_from_chats(&paths::chats_dir()) {
                Ok(r) if r.migrated > 0 => log::info!(
                    "Bound {} legacy chat(s) to new agent instances ({} already bound, {} skipped)",
                    r.migrated,
                    r.already_bound,
                    r.skipped
                ),
                Ok(_) => {}
                Err(e) => log::warn!("agent-instance backfill failed: {e}"),
            }
            let persisted = load_persisted_chats();
            if !persisted.is_empty() {
                log::info!("Loaded {} persisted chat(s) from disk", persisted.len());
            }
            Arc::new(Mutex::new(persisted))
        })
        .clone()
}

/// Per-chat live event bus. A subscriber (`GET /chats/{id}/events`) receives
/// events from *agent-initiated* turns — today, scheduled follow-ups the daemon
/// fires — so an open chat surfaces them without the user sending a message.
/// Normal user-initiated turns still stream over their own `POST .../turn`
/// response and don't need this.
type ChatBroadcasters = Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<ChatEvent>>>>;

fn chat_broadcasters() -> ChatBroadcasters {
    static B: std::sync::OnceLock<ChatBroadcasters> = std::sync::OnceLock::new();
    B.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

/// Get (or create) the broadcast sender for a chat. Kept alive in the registry
/// so a subscriber that connects before a follow-up fires still receives it.
async fn chat_event_sender(chat_id: &str) -> tokio::sync::broadcast::Sender<ChatEvent> {
    let reg = chat_broadcasters();
    let mut map = reg.lock().await;
    map.entry(chat_id.to_string())
        .or_insert_with(|| tokio::sync::broadcast::channel(64).0)
        .clone()
}

// ── A flow firing's conversation ────────────────────────────────────────
//
// A scheduled flow already runs *as* an agent: `flow_bindings::arm` mints a
// persistent instance and the executor threads its id through every turn, so the
// memory is real. What was missing is the **record** — a firing left a
// diagnostics session and nothing a person would read, so a flow-born agent
// listed zero conversations and opened onto an empty transcript. An agent that
// has never visibly done anything is indistinguishable from one that does not
// work.
//
// So a firing writes itself into a chat: the same `chats/<id>.json` a typed
// conversation uses, carrying the same `instance_id`. Two things fall out of
// that choice rather than needing to be built:
//
//   * the chat's live bus already exists, so a client watching
//     `GET /chats/{id}/events` sees a 3am cron replay in real time — the same
//     path scheduled follow-ups use (`deliver_followup_to_chat`);
//   * the agent's conversation list, turn counts, and every transcript UI start
//     working on flow runs with no client change at all.
//
// **The conversation is the record, not the execution context.** Nodes still run
// as independent one-shots through `run_one_shot_task`, passing values by the
// flow's own variables; they do not see each other's transcript. Making the chat
// the context would change what a flow *is* — a graph with explicit data flow —
// into an ever-growing thread, which is a different product.
//
// See `docs/FLOWS_AS_AGENTS_PLAN.md` §4.

/// The conversation each agent's flow runs are currently recording into, with
/// the time of the last turn written. Process-global, like the chat store.
type FlowThreads = Arc<Mutex<HashMap<String, (String, chrono::DateTime<chrono::Utc>)>>>;

fn flow_threads() -> FlowThreads {
    static T: std::sync::OnceLock<FlowThreads> = std::sync::OnceLock::new();
    T.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

/// How long after its last turn a firing still joins the previous conversation.
///
/// "A new conversation per firing" is right for a daily briefer and wrong for a
/// five-minute cron, which would mint 288 threads a day and bury the agent's real
/// ones. This is the rule the pod uses everywhere else to decide whether something
/// is still the same conversation — a gateway sender's next message
/// ([`gateway_session_for`]) and the follow-up policy — so flows are consistent
/// with the rest rather than special. A fast cron produces one rolling thread,
/// which is what anyone would want to read; a daily one produces a thread per day.
///
/// Note what this is *not*: it never clears a context in place. Past the window a
/// firing starts a new session under the same agent, which has no context to
/// clear — and the agent's memory, which lives on the instance rather than the
/// session, carries into it. Resetting a live conversation is a separate,
/// explicit act (`POST /chats/{id}/reset`).
const FLOW_CONVERSATION_TTL_SECS: i64 = DEFAULT_GATEWAY_SESSION_TTL_SECS as i64;

/// The conversation this agent's next flow turn belongs in, creating one if the
/// last is stale or gone.
///
/// `pub` (like [`deliver_followup_to_chat`]) because it is chat plumbing driven
/// from outside the HTTP handlers — here, by the flow executor.
///
/// In memory rather than on disk on purpose: the window asks "is this still the
/// same working session", which is a live-process question. After a restart the
/// next firing starts a fresh conversation, which is honest — the pod is not
/// mid-thought any more.
pub async fn flow_conversation(
    instance_id: &str,
    persona: &str,
    model: &str,
    cwd: &str,
) -> Option<String> {
    let store = chat_store();
    let threads = flow_threads();
    let mut open = threads.lock().await;

    if let Some((chat_id, last)) = open.get(instance_id) {
        let fresh = (chrono::Utc::now() - *last).num_seconds() < FLOW_CONVERSATION_TTL_SECS;
        // A chat deleted from under us must not resurrect as a ghost id.
        if fresh && store.lock().await.contains_key(chat_id) {
            return Some(chat_id.clone());
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let session = ChatSession {
        id: id.clone(),
        instance_id: Some(instance_id.to_string()),
        persona_slug: persona.to_string(),
        model_name: model.to_string(),
        cwd: cwd.to_string(),
        preset: SessionPreset::Workshop,
        state: None,
        archived: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        // The flow run has its own diagnostics session (`flow_exec` builds one,
        // tagged with `flow_id` + `instance_id`); a second logger here would
        // split one run's trace across two session dirs.
        diagnostics: None,
        trace: None,
        busy: false,
        interrupt: Arc::new(AtomicBool::new(false)),
        pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        plan: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    store
        .lock()
        .await
        .insert(id.clone(), Arc::new(Mutex::new(session)));
    open.insert(instance_id.to_string(), (id.clone(), chrono::Utc::now()));
    Some(id)
}

/// Append one flow node's turn to a conversation and publish it live.
///
/// `prompt` lands as the user message because that is what it is from the
/// agent's side: the instruction it was given. The flow's own marker (`▶ …`) is
/// prefixed by the caller on the first turn of a run, so a rolling thread still
/// shows where each firing began.
pub async fn record_flow_turn(chat_id: &str, prompt: &str, answer: &str) {
    let Some(session) = chat_store().lock().await.get(chat_id).cloned() else {
        return;
    };

    let sender = chat_event_sender(chat_id).await;
    let turn_index = {
        let s = session.lock().await;
        s.state
            .as_ref()
            .map(|st| {
                st.messages
                    .iter()
                    .filter(|m| matches!(m, AgentMessage::User(_)))
                    .count()
            })
            .unwrap_or(0)
    };
    // A send error only means nobody is listening; the turn is persisted below.
    let _ = sender.send(ChatEvent::TurnStarted {
        turn_index,
        user_message: prompt.to_string(),
        session_id: None,
    });

    {
        let mut s = session.lock().await;
        let mut state = match s.state.take() {
            Some(prev) => prev.continue_with(prompt.to_string()),
            None => AgentState::new(prompt.to_string()),
        };
        state
            .messages
            .push(AgentMessage::Assistant(answer.to_string()));
        // The node has already finished by the time it is recorded, so the turn
        // is complete the moment it lands.
        state.is_done = true;
        s.state = Some(state);
    }
    persist_chat(&session).await;

    let _ = sender.send(ChatEvent::Reply {
        content: answer.to_string(),
        awaiting_reply: false,
        options: Vec::new(),
    });
    let _ = sender.send(ChatEvent::Done {
        status: "completed".into(),
        reason: None,
    });

    if let Some(instance_id) = session.lock().await.instance_id.clone() {
        flow_threads()
            .lock()
            .await
            .insert(instance_id, (chat_id.to_string(), chrono::Utc::now()));
    }
}

struct ChatSession {
    id: String,
    /// The agent instance this conversation belongs to. `None` only for records
    /// written before instances existed (backfilled at startup).
    instance_id: Option<String>,
    persona_slug: String,
    model_name: String,
    cwd: String,
    /// The session's I/O type — workshop chat vs. a bound gateway conversation.
    /// Decides where `say_to_user` replies are delivered.
    preset: SessionPreset,
    /// The context a model still sees. `None` after a reset, until the next turn.
    ///
    /// Deliberately **not** the conversation: `transcript` is. These were one list
    /// until reset needed to end a context while keeping the history, which is
    /// what made the difference between the two worth naming.
    state: Option<AgentState>,
    /// The conversation *before* the current context: every message closed off by
    /// a reset, each run ending in the divider that closed it.
    ///
    /// Frozen by construction — nothing appends here except a reset, and nothing
    /// ever rewrites it. That is what lets a reset keep the history it is hiding
    /// from the model, which `/clear` could not do when there was only one list.
    archived: Vec<ChatMessageWire>,
    created_at: String,
    diagnostics: Option<Arc<DiagnosticsLogger>>,
    /// OTLP trace writer, parallel to `diagnostics`. Shares its session id.
    trace: Option<Arc<TraceLogger>>,
    /// True while a turn is mid-flight. Prevents two concurrent turns from
    /// stomping on the same state.
    busy: bool,
    /// Set by `POST /chats/{id}/interrupt` to ask the running turn to stop.
    /// An `Arc<AtomicBool>` rather than a plain flag because the only place that
    /// can act on it is the executor's step guard, which is a sync closure and
    /// cannot take this session's async lock. Cleared when a turn starts, so a
    /// stop pressed after the turn already ended can never kill the next one.
    interrupt: Arc<AtomicBool>,
    /// Messages that arrived while a turn was already running, drained FIFO.
    ///
    /// A `std::sync::Mutex` behind an `Arc` rather than a plain field, for the
    /// same reason [`Self::interrupt`] is an `Arc<AtomicBool>`: the executor's
    /// mailbox is a sync closure and cannot take this session's async lock. That
    /// mailbox is what lets a message join the turn already running instead of
    /// waiting for it, so the queue has to be reachable from inside the run.
    pending: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// The plan of the turn running right now, as `update_plan` last wrote it.
    ///
    /// Held here so a client that arrives mid-turn — reconnecting, or opening
    /// the same chat on a second device — can be shown the plan it missed the
    /// frames for. Sync-locked for the same reason as [`Self::pending`]: the
    /// plan sink is a sync closure.
    ///
    /// Deliberately **not** persisted to disk. A plan belongs to one turn, and a
    /// restart ends every turn it had — so a plan reloaded from disk would
    /// describe work that is no longer happening, which is the exact thing the
    /// empty-plan frame at turn start exists to prevent.
    plan: Arc<std::sync::Mutex<Vec<crate::turn_plan::PlanStep>>>,
}

/// Build a [`TraceLogger`] keyed to a diagnostics logger's session-dir name, so
/// `traces/<id>` and `sessions/<id>` share the same `<id>`. Returns `None` if
/// there is no diagnostics logger or the trace dir can't be created — tracing
/// is strictly best-effort and never blocks a chat turn.
fn trace_for(diagnostics: Option<&DiagnosticsLogger>, model: &str) -> Option<Arc<TraceLogger>> {
    let session_id = diagnostics?
        .session_dir()
        .file_name()
        .and_then(|n| n.to_str())?
        .to_string();
    TraceLogger::new(&session_id, model).ok().map(Arc::new)
}

struct ApiState {
    api_key: String,
    chats: ChatStore,
    /// `cwd` to run chats and flow-runs from. Captured at startup so chats
    /// don't pick up the daemon's later cwd changes.
    cwd: String,
}

// ── Response types ──────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
struct ErrorResponse {
    error: String,
}

fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorResponse { error: msg.into() })).into_response()
}

// ── Snapshot types ──────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
struct ProjectSnapshot {
    personas: Vec<PersonaSummary>,
    skills: Vec<SkillSummary>,
    /// From the external `metalcraft-flows` crate, which has no `ToSchema`; the
    /// web app doesn't consume this field, so expose it as an opaque object array.
    #[schema(value_type = Vec<Object>)]
    flows: Vec<metalcraft_flows::FlowSummary>,
    sessions: Vec<DiagnosticsSessionSummary>,
    api_tools: Vec<ApiToolSummary>,
    keys: Vec<KeySummary>,
    /// Agents this pod can be. Both Workshop clients paint their agent picker from
    /// the snapshot, so leaving these out costs an extra round-trip on every load.
    agent_presets: Vec<crate::agent_preset::PresetSummary>,
    /// Agents that actually exist. Ephemeral ones are excluded — an unfiltered list
    /// is one row per chat ever started, which is noise, not information.
    agent_instances: Vec<crate::agent_instance::AgentInstance>,
    default_agent_preset: String,
    layout: ProjectLayout,
}

#[derive(Serialize, utoipa::ToSchema)]
struct ProjectLayout {
    data_dir: String,
    personas_dir: String,
    skills_dir: String,
    flows_dir: String,
    sessions_dir: String,
    api_tools_dir: String,
    agent_presets_dir: String,
    agent_instances_dir: String,
}

#[derive(Serialize, utoipa::ToSchema)]
struct ApiToolSummary {
    name: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    read_only: bool,
}

/// A stored API key, exposed to the workshop with its value masked — the
/// raw secret is never sent over the wire. Used by the load-time snapshot and
/// the sidebar (global scope only).
#[derive(Serialize, utoipa::ToSchema)]
struct KeySummary {
    name: String,
    masked: String,
}

/// A stored key with its **scope** and whether it is managed (platform- or
/// connection-owned → read-only in the UI). Returned by `GET /api/v1/keys` so
/// the Keys page can group global keys and per-channel secrets, and lock the
/// managed ones. `channel_id`/`channel_name` are present only for channel scope.
#[derive(Serialize, utoipa::ToSchema)]
struct KeyEntry {
    name: String,
    masked: String,
    /// `"global"` or `"channel"`.
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_name: Option<String>,
    managed: bool,
}

/// The raw value of a key, returned only by the explicit reveal endpoint.
#[derive(Serialize, utoipa::ToSchema)]
struct KeyRevealResponse {
    value: String,
}

/// A key recommended by one or more *enabled* integrations (from their
/// `requires_env`), with whether it currently resolves (key store or env) and
/// which packs declare it. Drives the "keys these packs still need" list in
/// the key store UI — `configured: false` is the hint to add it.
#[derive(Serialize, utoipa::ToSchema)]
struct RecommendedKey {
    name: String,
    configured: bool,
    packs: Vec<String>,
    /// Platform-managed (env-authoritative, e.g. `METALCRAFT_TOKEN` injected into a
    /// provisioned pod). The UI should show it as provided/read-only rather than
    /// prompting the user to paste a value.
    managed: bool,
}

// ── Auth middleware ─────────────────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();

    // Two accepted credentials, resolved to the same "allow" outcome:
    //   1. the static WORKSHOP_API_KEY (legacy; also the /token exchange input), or
    //   2. a Metalcraft ID token (`mck_…`) that resolves to this pod's owner or is
    //      audience-scoped to this pod (e.g. a connection token). See `hub_auth`.
    // The static key is checked first (cheap, no network); the hub path only runs
    // for `mck_` bearers.
    let ok = (!state.api_key.is_empty() && provided == state.api_key)
        || crate::hub_auth::verify_pod_bearer(provided).await;
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
            }),
        )
            .into_response();
    }

    next.run(request).await
}

// ── OpenAPI ──────────────────────────────────────────────────────────────

/// The machine-readable description of the pod's `/api/v1` surface, served at
/// `GET /api/v1/openapi.json` and rendered by Scalar at `GET /api/v1/docs`.
///
/// This is the single source of truth for the wire shapes: it is *derived* from
/// the same Rust structs the handlers serialize (via `#[derive(ToSchema)]` and
/// the `#[utoipa::path]` annotations on each handler), so it cannot drift from
/// what the pod actually sends. The workshop clients generate their TypeScript
/// types from this document — see `metalcraft-workshop-web`'s `gen:types`.
#[derive(utoipa::OpenApi)]
#[openapi(
    modifiers(&BearerAuthAddon),
    security(("bearer" = [])),
    info(
        title = "Metalcraft Agent API",
        version = env!("CARGO_PKG_VERSION"),
        description = "The pod's workshop admin API. Manages personas, skills, flows, keys, \
            chats, diagnostics sessions, integrations, and gateway channels. Authenticate \
            with a Bearer token: the static WORKSHOP_API_KEY or a Metalcraft ID token (mck_…) \
            scoped to this pod.",
    ),
    paths(
        agent_info, get_snapshot,
        list_agent_packs, get_agent_pack, post_install_agent_pack, delete_agent_pack,
        post_export_agent_pack,
        list_agent_presets, get_agent_preset, put_agent_preset, delete_agent_preset,
        list_agent_instances, get_agent_instance, post_create_agent_instance,
        get_agent_instance_flows,
        patch_agent_instance, delete_agent_instance,
        get_agent_instance_memory, post_instance_conversation, list_instance_conversations,
        list_personas, get_persona, put_persona, delete_persona,
        list_skills, get_skill, put_skill, delete_skill,
        list_flows, get_flow, put_flow, post_validate_flow, delete_flow, post_run_flow,
        post_install_flow,
        list_scheduled_flows, get_scheduled_flow, post_scheduled_flow,
        put_scheduled_flow, delete_scheduled_flow, post_schedule_preview,
        get_flow_binding, put_flow_binding,
        post_inspect_agent_pack, get_agent_pack_registries, post_update_agent_pack,
        get_octaweave_status,
        get_registry_status, post_registry_connect, post_registry_disconnect,
        get_registry_search, get_registry_manifest,
        post_check_flow_dependencies,
        list_flow_runs, get_flow_run, post_resume_flow_run,
        list_flow_templates, get_flow_template,
        list_diagnostics, get_diagnostics_session, get_diagnostics_trace,
        list_api_tools, get_api_tool, put_api_tool, delete_api_tool,
        list_keys, list_recommended_keys, put_key, delete_key, reveal_key,
        get_inference_status,
        list_chats, post_create_chat, get_chat, delete_chat, post_chat_turn, get_chat_events,
        post_chat_reset,
        get_chat_context, post_chat_compact, post_chat_interrupt,
        list_scheduled_tasks, delete_scheduled_task,
        list_integrations, get_integration, put_integration_enabled,
        get_lockfile, post_lockfile_restore,
        list_gateway_activity,
        list_channels, create_channel, update_channel, delete_channel, list_channel_events,
        gateway_metalcraft_status, gateway_metalcraft_register,
        gateway_metalcraft_connect, gateway_metalcraft_disconnect,
        gateway_metalcraft_unregister,
        post_factory_reset,
    ),
    components(schemas(
        FactoryResetRequest, ResetReport, ResetScope, ResetFailure, RestartExpectation,
        ErrorResponse, ProjectSnapshot, ProjectLayout, ApiToolSummary,
        KeySummary, KeyEntry, KeyRevealResponse, RecommendedKey, KeyValueBody, KeyScopeQuery,
        InferenceStatus, ChatContext, ChatCompacted, ChatInterrupt, ChatQueued,
        FlowTemplateSummary, FlowTemplate, RunFlowRequest, RunFlowResponse, RunFlowOutput, ResumeFlowRunRequest,
        InstallFlowRequest, InstallDependenciesResponse,
        crate::scheduled_flows::SchedulePreview,
        FlowList, FlowListItem, FlowValidation,
        // The graph itself, from `metalcraft-flows` (its `schema` feature). Without
        // these a client cannot type a flow at all — which is why both clients
        // stopped at `node_count` and neither could draw one.
        metalcraft_flows::SavedFlow, metalcraft_flows::FlowDefinition,
        metalcraft_flows::FlowNode, metalcraft_flows::FlowEdge,
        metalcraft_flows::FlowNodeType,
        ScheduledFlowList, ScheduledFlowRow, CreateScheduledFlowRequest,
        UpdateScheduledFlowRequest, PreviewScheduleRequest,
        crate::flow_exec::FlowRunSummary, crate::flow_exec::FlowStep,
        crate::flow_runs::FlowRun, crate::flow_runs::PauseInfo,
        crate::flow_install::InstallResult, crate::flow_install::InstalledFlow,
        crate::flow_install::DependencyReport, crate::flow_install::PackInstallOutcome,
        crate::lockfile::Lock, crate::lockfile::LockEntry, RestoreOutcome, RestoreResult,
        ChatSummary, ChatDetail, ChatMessageWire, CreateChatRequest, ChatTurnRequest, ChatEvent,
        IntegrationSummary, IntegrationDetail, SetEnabledRequest,
        MgRegisterRequest, MgConnectRequest,
        crate::channels::Channel, CreateChannelRequest, UpdateChannelRequest,
        crate::persona::Persona, crate::persona::PersonaSummary,
        crate::agent_preset::AgentPreset, crate::agent_preset::PresetSummary,
        crate::agent_preset::PresetPersona, crate::agent_preset::PersonaRole,
        crate::agent_preset::ModelFloor, crate::agent_preset::MemoriesRef,
        crate::agent_instance::AgentInstance, crate::agent_instance::InstanceOrigin,
        crate::agent_packs::InstalledAgentPack, crate::agent_packs::InstallReport,
        crate::agent_packs::UninstallReport, crate::agent_packs::AgentPackManifest,
        crate::agent_packs::UpdateReport, crate::agent_packs::PersonaFallback,
        crate::agent_packs::Orphaned,
        crate::agent_packs::ConsentSummary, crate::agent_packs::manifest::Provides,
        crate::agent_packs::manifest::IntegrationRef, crate::agent_packs::manifest::Author,
        crate::agent_packs::manifest::Parent, crate::agent_packs::manifest::EnvRequirement,
        ExportAgentPackRequest,
        crate::memory::InstanceMemoryView, crate::memory::MemorySample,
        InstanceDetail, CreateInstanceRequest, PatchInstanceRequest, NewConversationRequest,
        InstanceFlows, PresetDetail, RosterPersona, InstanceList, InstanceListItem,
        AgentPackPreview, Registries, RegistryView, crate::agent_registry::Trust,
        crate::agent_registry::Connection, crate::agent_registry::ConnectionState,
        crate::agent_registry::SearchHit,
        crate::octaweave::OctaweaveConnection, crate::octaweave::OctaweaveConnectionState,
        FlowBindingView, FlowPersonaCheck, ArmedSchedule, ArmConsent,
        BindFlowRequest,
        crate::skill::Skill, crate::skill::SkillSummary,
        crate::gateway_activity::GatewayEvent,
        crate::metalcraft_gateway::GatewayStatus,
        crate::tools::http_api::HttpApiToolConfig, crate::tools::http_api::MultipartConfig,
        crate::flows::FlowPromptResult,
        crate::diagnostics_browse::DiagnosticsSessionSummary,
        crate::diagnostics_browse::DiagnosticsSession, crate::diagnostics_browse::TimelineEvent,
    )),
    tags(
        (name = "agent", description = "Agent identity + project snapshot"),
        (name = "personas", description = "Persona definitions"),
        (name = "skills", description = "Skill definitions"),
        (name = "flows", description = "Flows, flow runs, and flow templates"),
        (name = "diagnostics", description = "Diagnostics session browsing"),
        (name = "api-tools", description = "HTTP-API tool configs"),
        (name = "keys", description = "The agent key/secret store"),
        (name = "chats", description = "Interactive chat sessions"),
        (name = "scheduled-tasks", description = "Scheduled follow-ups"),
        (name = "integrations", description = "Installable integrations"),
        (name = "agent-packs", description = "Installable agent packs — an agent plus every persona, skill and integration it needs"),
        (name = "agent-presets", description = "Agents this pod can be — a default persona, its callable roster, and the skills and packs they need"),
        (name = "agent-instances", description = "Agents that exist — each with its own memory and conversations"),
        (name = "gateway", description = "Messaging gateway channels + Metalcraft connect"),
    ),
)]
pub struct ApiDoc;

/// Registers the `bearer` security scheme so the docs show that `/api/v1/*`
/// requires a Bearer token (WORKSHOP_API_KEY or an `mck_…` pod token).
struct BearerAuthAddon;
impl utoipa::Modify for BearerAuthAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("token")
                        .build(),
                ),
            );
        }
    }
}

/// Serve the OpenAPI document. Unauthenticated (like `/health`) so client build
/// tooling and browsers can fetch it without a pod token.
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(<ApiDoc as utoipa::OpenApi>::openapi())
}

// ── Router + server ────────────────────────────────────────────────────

/// Build the workshop API router. Callable from any binary that wants to
/// host the admin API — `metalcraft-agent --api` runs it stand-alone while
/// `metalcraft-daemon --api` mounts it alongside the event listener and the
/// flow scheduler.
pub fn build_router(api_key: String) -> Router {
    // Brings `Scalar::with_url` (a `Servable` trait method) into scope.
    use utoipa_scalar::Servable as _;
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());
    // Use the process-global chat store (rehydrated from disk on first access)
    // so the daemon's scheduled-follow-up runner and these handlers operate on
    // the same sessions.
    let state = Arc::new(ApiState {
        api_key,
        chats: chat_store(),
        cwd,
    });

    // Inbound is now delivered by the gateway PUSHING to this pod's signed
    // `/webhook/gateway` endpoint (route resolved live from k3 — see the gateway's
    // PUSH_VIA_K3_ROUTE_PLAN.md), so the pod no longer holds a long-poll by default. The
    // legacy Inbound Pull loop stays available for a dual-transport bake: set
    // `GATEWAY_INBOUND_PULL=1` to re-enable it.
    if gateway_inbound_pull_enabled() {
        let state = state.clone();
        tokio::spawn(async move { inbound_pull_loop(state).await });
    }

    Router::new()
        .route("/api/v1/info", get(agent_info))
        .route("/api/v1/snapshot", get(get_snapshot))
        .route("/api/v1/agent-packs", get(list_agent_packs))
        .route("/api/v1/agent-packs/install", post(post_install_agent_pack))
        .route("/api/v1/agent-packs/inspect", post(post_inspect_agent_pack))
        // What this pod is to Octaweave: a read, because there is nothing to configure
        // — the credential is the one the pod already holds.
        .route("/api/v1/services/octaweave", get(get_octaweave_status))
        .route(
            "/api/v1/agent-packs/registries",
            get(get_agent_pack_registries),
        )
        // A registry is browsable and connectable, not just an origin the pod will
        // fetch from. Everything here is proxied rather than called from a browser:
        // the origin check, the pod's own credential and the redirect refusal all
        // live on this side, and a client that called the host directly would have
        // none of them.
        .route(
            "/api/v1/agent-packs/registries/{name}/status",
            get(get_registry_status),
        )
        .route(
            "/api/v1/agent-packs/registries/{name}/connect",
            post(post_registry_connect),
        )
        .route(
            "/api/v1/agent-packs/registries/{name}/disconnect",
            post(post_registry_disconnect),
        )
        .route(
            "/api/v1/agent-packs/registries/{name}/search",
            get(get_registry_search),
        )
        .route(
            "/api/v1/agent-packs/registries/{name}/packs/{id}/manifest",
            get(get_registry_manifest),
        )
        .route("/api/v1/agent-packs/export", post(post_export_agent_pack))
        .route("/api/v1/agent-packs/{id}", get(get_agent_pack))
        .route("/api/v1/agent-packs/{id}", delete(delete_agent_pack))
        .route(
            "/api/v1/agent-packs/{id}/update",
            post(post_update_agent_pack),
        )
        .route("/api/v1/agents/instances", get(list_agent_instances))
        .route("/api/v1/agents/instances", post(post_create_agent_instance))
        .route("/api/v1/agents/instances/{id}", get(get_agent_instance))
        .route("/api/v1/agents/instances/{id}", patch(patch_agent_instance))
        .route(
            "/api/v1/agents/instances/{id}/memory",
            get(get_agent_instance_memory),
        )
        .route(
            "/api/v1/agents/instances/{id}/flows",
            get(get_agent_instance_flows),
        )
        .route(
            "/api/v1/agents/instances/{id}/conversations",
            get(list_instance_conversations).post(post_instance_conversation),
        )
        .route(
            "/api/v1/agents/instances/{id}",
            delete(delete_agent_instance),
        )
        .route("/api/v1/agent-presets", get(list_agent_presets))
        .route("/api/v1/agent-presets/{slug}", get(get_agent_preset))
        .route("/api/v1/agent-presets/{slug}", put(put_agent_preset))
        .route("/api/v1/agent-presets/{slug}", delete(delete_agent_preset))
        .route("/api/v1/personas", get(list_personas))
        .route("/api/v1/skills", get(list_skills))
        .route("/api/v1/personas/{slug}", get(get_persona))
        .route("/api/v1/personas/{slug}", put(put_persona))
        .route("/api/v1/personas/{slug}", delete(delete_persona))
        .route("/api/v1/skills/{slug}", get(get_skill))
        .route("/api/v1/skills/{slug}", put(put_skill))
        .route("/api/v1/skills/{slug}", delete(delete_skill))
        .route("/api/v1/flows", get(list_flows))
        // Static `/install` before the `{id}` param route (matchit prefers the
        // literal) — install a registry flow onto this agent.
        .route("/api/v1/flows/install", post(post_install_flow))
        // Literal before `{id}`, like `/install` above.
        .route("/api/v1/flows/validate", post(post_validate_flow))
        .route("/api/v1/flows/{id}", get(get_flow))
        .route("/api/v1/flows/{id}", put(put_flow))
        .route("/api/v1/flows/{id}", delete(delete_flow))
        .route("/api/v1/flows/{id}/run", post(post_run_flow))
        .route("/api/v1/flows/{id}/binding", get(get_flow_binding))
        .route("/api/v1/flows/{id}/binding", put(put_flow_binding))
        // Scheduled flows — *when* a flow runs. The literal `preview` is
        // registered before the `{id}` param so matchit prefers the static segment.
        .route("/api/v1/scheduled-flows", get(list_scheduled_flows))
        .route("/api/v1/scheduled-flows", post(post_scheduled_flow))
        .route(
            "/api/v1/scheduled-flows/preview",
            post(post_schedule_preview),
        )
        .route("/api/v1/scheduled-flows/{id}", get(get_scheduled_flow))
        .route("/api/v1/scheduled-flows/{id}", put(put_scheduled_flow))
        .route("/api/v1/scheduled-flows/{id}", delete(delete_scheduled_flow))
        .route(
            "/api/v1/flows/{id}/check-dependencies",
            post(post_check_flow_dependencies),
        )
        .route("/api/v1/flow-runs", get(list_flow_runs))
        .route("/api/v1/flow-runs/{run_id}", get(get_flow_run))
        .route(
            "/api/v1/flow-runs/{run_id}/resume",
            post(post_resume_flow_run),
        )
        .route("/api/v1/flow-templates", get(list_flow_templates))
        .route("/api/v1/flow-templates/{slug}", get(get_flow_template))
        .route("/api/v1/diagnostics", get(list_diagnostics))
        .route("/api/v1/diagnostics/{id}", get(get_diagnostics_session))
        .route("/api/v1/diagnostics/{id}/trace", get(get_diagnostics_trace))
        .route("/api/v1/api-tools", get(list_api_tools))
        .route("/api/v1/api-tools/{name}", get(get_api_tool))
        .route("/api/v1/api-tools/{name}", put(put_api_tool))
        .route("/api/v1/api-tools/{name}", delete(delete_api_tool))
        .route("/api/v1/keys", get(list_keys))
        .route("/api/v1/inference", get(get_inference_status))
        .route("/api/v1/keys/recommended", get(list_recommended_keys))
        .route("/api/v1/keys/{name}", put(put_key))
        .route("/api/v1/keys/{name}", delete(delete_key))
        .route("/api/v1/keys/{name}/reveal", get(reveal_key))
        .route("/api/v1/chats", get(list_chats).post(post_create_chat))
        .route("/api/v1/chats/{id}", get(get_chat).delete(delete_chat))
        .route("/api/v1/chats/{id}/turn", post(post_chat_turn))
        .route("/api/v1/chats/{id}/context", get(get_chat_context))
        .route("/api/v1/chats/{id}/compact", post(post_chat_compact))
        .route("/api/v1/chats/{id}/reset", post(post_chat_reset))
        // `/clear` predates `/reset` and now does the same non-destructive thing.
        // Kept because shipped clients (the phone's `/clear` command, the desktop
        // menu) still call it, and a 404 there would read as the pod being broken
        // rather than as a rename.
        .route("/api/v1/chats/{id}/clear", post(post_chat_reset))
        .route("/api/v1/chats/{id}/interrupt", post(post_chat_interrupt))
        .route("/api/v1/chats/{id}/events", get(get_chat_events))
        .route("/api/v1/scheduled-tasks", get(list_scheduled_tasks))
        .route(
            "/api/v1/scheduled-tasks/{id}",
            delete(delete_scheduled_task),
        )
        .route("/api/v1/integrations", get(list_integrations))
        .route("/api/v1/integrations/{id}", get(get_integration))
        .route(
            "/api/v1/integrations/{id}/enabled",
            put(put_integration_enabled),
        )
        .route("/api/v1/lockfile", get(get_lockfile))
        .route("/api/v1/lockfile/restore", post(post_lockfile_restore))
        // Gateway activity feed (inbound/outbound across all channels).
        .route("/api/v1/gateway/activity", get(list_gateway_activity))
        // Channels — the simple {slug, name, url, secret} connection model. The
        // built-in `metalcraft` channel is always present; these manage customs.
        .route("/api/v1/channels", get(list_channels).post(create_channel))
        .route(
            "/api/v1/channels/{slug}",
            put(update_channel).delete(delete_channel),
        )
        .route("/api/v1/channels/{slug}/events", get(list_channel_events))
        // Metalcraft Gateway — zero-copy connect (status / inline register / connect).
        .route(
            "/api/v1/gateway/metalcraft/status",
            get(gateway_metalcraft_status),
        )
        .route(
            "/api/v1/gateway/metalcraft/register",
            post(gateway_metalcraft_register),
        )
        .route(
            "/api/v1/gateway/metalcraft/connect",
            post(gateway_metalcraft_connect),
        )
        .route(
            "/api/v1/gateway/metalcraft/disconnect",
            post(gateway_metalcraft_disconnect),
        )
        .route(
            "/api/v1/gateway/metalcraft/unregister",
            post(gateway_metalcraft_unregister),
        )
        // Authenticated like everything else: the workshop key is already a
        // full-admin credential, so a separate gate would be theatre. The guard
        // that matters is the typed confirmation phrase in the body.
        .route("/api/v1/factory-reset", post(post_factory_reset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Health check — registered after the auth layer so it stays
        // unauthenticated (for Railway / DO App Platform probes).
        .route("/health", get(health))
        // OpenAPI document + Scalar docs UI. Unauthenticated (like /health) so
        // client build tooling and browsers can read the API contract.
        .route("/api/v1/openapi.json", get(openapi_json))
        .merge(utoipa_scalar::Scalar::with_url(
            "/api/v1/docs",
            <ApiDoc as utoipa::OpenApi>::openapi(),
        ))
        // Public landing page. Also after the auth layer, so hitting the pod's
        // ingress host in a browser shows a friendly status page instead of a
        // 401 or a bare JSON blob. No secrets — just "this agent is alive".
        .route("/", get(landing))
        // Inbound gateway webhook — unauthenticated like /health; provenance is
        // verified by the per-channel HMAC signature on the request.
        .route("/webhook/gateway", post(handle_gateway_webhook))
        .with_state(state)
}

/// Liveness/readiness probe. Returns 200 with a small JSON body. Not behind
/// the auth middleware so platform health checks succeed without a key. The
/// `version` field lets an operator confirm which build is live with a bare
/// `curl <host>/health` — the same value the Workshop's Settings tab shows.
async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

/// Public landing page served at `/`. Unauthenticated (registered after the
/// auth layer, like `/health`) so a person visiting the pod's ingress host in a
/// browser sees a simple "agent is running" card rather than a 401 or raw JSON.
/// Intentionally static and secret-free — the real UI is the Workshop, which
/// talks to `/api/v1/*` with a key.
async fn landing() -> impl IntoResponse {
    let name = env!("CARGO_PKG_NAME");
    let version = env!("CARGO_PKG_VERSION");
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Metalcraft Agent</title>
<style>
  :root {{ color-scheme: dark; }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0; min-height: 100vh; display: flex; align-items: center;
    justify-content: center; padding: 1.5rem;
    font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
    background: radial-gradient(1200px 600px at 50% -10%, #17324f 0%, #0b1220 55%, #070b13 100%);
    color: #e6edf6;
  }}
  .card {{
    width: 100%; max-width: 30rem; text-align: center;
    background: rgba(20, 30, 46, 0.75); border: 1px solid #24344b;
    border-radius: 18px; padding: 2.25rem 2rem;
    box-shadow: 0 20px 60px rgba(0,0,0,0.45);
  }}
  .logo {{
    height: 3rem; width: 3rem; margin: 0 auto 1.1rem; border-radius: 12px;
    background: linear-gradient(135deg, #2f83f5, #1e5fbf);
    display: flex; align-items: center; justify-content: center;
    font-weight: 800; font-size: 1.4rem; color: #fff;
  }}
  h1 {{ margin: 0 0 .35rem; font-size: 1.35rem; }}
  .status {{
    display: inline-flex; align-items: center; gap: .45rem;
    color: #93b4d6; font-size: .9rem; margin-bottom: 1rem;
  }}
  .dot {{
    height: .55rem; width: .55rem; border-radius: 50%;
    background: #4ade80; box-shadow: 0 0 0 4px rgba(74,222,128,.18);
  }}
  p {{ color: #9db2c9; font-size: .92rem; line-height: 1.5; margin: .4rem 0 0; }}
  a {{ color: #6aa8ff; text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  code {{ background: #0d1524; border: 1px solid #24344b; border-radius: 6px;
    padding: .1rem .4rem; font-size: .82rem; color: #cfe0f5; }}
</style>
</head>
<body>
  <div class="card">
    <div class="logo">M</div>
    <h1>Metalcraft Agent</h1>
    <div class="status"><span class="dot"></span>running · v{version}</div>
    <p>Your always-on agent pod is live.</p>
    <p>Manage it from the control plane at
       <a href="https://pods.metalcraftai.com">pods.metalcraftai.com</a>.</p>
    <p style="margin-top:1.1rem;font-size:.78rem;color:#66788f">
       {name} · API at <code>/api/v1</code> · health at <code>/health</code></p>
  </div>
</body>
</html>"#
    );
    Html(html)
}

/// Agent identity/version + config the Workshop reads. `version` drives the
/// Settings tab's "which build is live" check; `default_persona` is the persona
/// the Workshop's New Chat modal defaults to (set `METALCRAFT_DEFAULT_PERSONA`
/// to override; falls back to the orchestrator, which delegates to specialists).
#[utoipa::path(
    get,
    path = "/api/v1/info",
    tag = "agent",
    responses((status = 200, description = "Agent name, version, and default persona")),
)]
async fn agent_info() -> impl IntoResponse {
    let default_persona = crate::runtime::configured_default_persona();
    Json(serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "default_persona": default_persona,
    }))
}

/// Bind and serve the router on the given port. Blocks until ctrl-c.
pub async fn serve(port: u16, router: Router) {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("Failed to bind workshop API listener");

    log::info!("Workshop API serving on 0.0.0.0:{port}");
    println!("Workshop API listening on http://0.0.0.0:{port}");

    let shutdown_signal = async {
        tokio::signal::ctrl_c().await.ok();
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .expect("Workshop API server error");
}

/// Foreground entrypoint used by `metalcraft-agent --api`. Equivalent to
/// `serve(config.port, build_router(config.api_key))`.
pub async fn start(config: WorkshopApiConfig) {
    serve(config.port, build_router(config.api_key)).await;
}

// ── Snapshot handler ────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/v1/snapshot",
    tag = "agent",
    responses((status = 200, body = ProjectSnapshot)),
)]
async fn get_snapshot() -> Json<ProjectSnapshot> {
    let personas = list_persona_summaries();
    let skills = list_skill_summaries();
    let flows = metalcraft_flows::list_flows(&paths::flows_dir());
    let sessions = list_diagnostics_sessions();
    let api_tools = list_api_tool_summaries();
    let keys = list_key_summaries();
    let agent_presets =
        crate::agent_preset::AgentPreset::list_summaries(&paths::agent_presets_dir());
    let agent_instances: Vec<_> = crate::agent_instance::list();

    Json(ProjectSnapshot {
        personas,
        skills,
        flows,
        sessions,
        api_tools,
        keys,
        agent_presets,
        agent_instances,
        default_agent_preset: crate::agent_preset::DEFAULT_PRESET.to_string(),
        layout: ProjectLayout {
            data_dir: paths::data_dir().display().to_string(),
            personas_dir: paths::personas_dir().display().to_string(),
            skills_dir: paths::skills_dir().display().to_string(),
            flows_dir: paths::flows_dir().display().to_string(),
            sessions_dir: paths::sessions_dir().display().to_string(),
            api_tools_dir: paths::api_tools_dir().display().to_string(),
            agent_presets_dir: paths::agent_presets_dir().display().to_string(),
            agent_instances_dir: paths::agent_instances_dir().display().to_string(),
        },
    })
}

// ── Persona handlers ────────────────────────────────────────────────────

/// User-local personas plus enabled-pack personas, with locals shadowing
/// packs on slug collision.
fn list_persona_summaries() -> Vec<PersonaSummary> {
    let layered =
        crate::integrations::list_files_layered(&paths::personas_dir(), "personas", "json");
    let mut out = Vec::with_capacity(layered.len());
    for (path, origin) in layered {
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(persona) = serde_json::from_str::<Persona>(&content) else {
            continue;
        };
        out.push(PersonaSummary {
            slug,
            name: persona.name,
            description: persona.description,
            pack_id: origin.pack_id().map(String::from),
            read_only: origin.is_read_only(),
        });
    }
    out
}

// ── Agent packs ──────────────────────────────────────────────────────────────
// The unit of installation. An agent pack provides one agent preset plus every
// persona, skill and integration it needs.

#[utoipa::path(
    get,
    path = "/api/v1/agent-packs",
    tag = "agent-packs",
    responses((status = 200, description = "Installed agent packs")),
)]
async fn list_agent_packs() -> Response {
    Json(serde_json::json!({ "agent_packs": crate::agent_packs::list() })).into_response()
}

#[utoipa::path(
    get,
    path = "/api/v1/agent-packs/{id}",
    tag = "agent-packs",
    params(("id" = String, Path, description = "Agent pack id")),
    responses((status = 200, description = "The pack's manifest"), (status = 404, description = "Not installed")),
)]
async fn get_agent_pack(Path(id): Path<String>) -> Response {
    match crate::agent_packs::find(&id) {
        Some(p) => Json(p).into_response(),
        None => err_json(
            StatusCode::NOT_FOUND,
            format!("agent pack '{id}' is not installed"),
        ),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct InstallAgentPackQuery {
    /// Install from a `.agentpack` already on the pod's disk. Omit to upload the
    /// archive as the request body instead.
    #[serde(default)]
    path: Option<String>,
    /// Download from a registry. The origin must be configured — see
    /// [`crate::agent_registry`] and `GET /api/v1/agent-packs/registries`.
    #[serde(default)]
    url: Option<String>,
    /// A registry reference: `@amy_kitchen`, or `axoniac:@amy_kitchen` to say which
    /// host. A bare reference published on two configured registries is an **error**
    /// rather than a first match — see `specs/AGENT_PACK_FORMAT.md` §11.3.
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    /// Install from a `verified-only` host even though the pack is not verified.
    /// The operator is the one being asked, so they are allowed to say yes.
    #[serde(default)]
    allow_unverified: Option<bool>,
}

/// Where the archive for this request comes from.
///
/// The three sources are the same for `/inspect` and `/install`, deliberately: a
/// dialog inspects a thing and then installs *that same thing*, and any difference
/// between the two paths would be a difference between what was approved and what
/// was done.
/// An archive to inspect or install, and where it came from.
///
/// `resolved` is present only when a registry answered for it: an upload has no host
/// to attribute the install to, and neither does a path on disk.
struct PackBytes {
    bytes: Vec<u8>,
    /// What the lockfile pins — a URL, a path, or `"upload"`.
    source: String,
    resolved: Option<crate::agent_registry::Resolved>,
}

async fn agent_pack_bytes(
    q: &InstallAgentPackQuery,
    body: &axum::body::Bytes,
) -> Result<PackBytes, Response> {
    if let Some(reference) = q
        .reference
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        let resolved = crate::agent_registry::resolve(reference)
            .await
            .map_err(|e| err_json(StatusCode::BAD_REQUEST, e))?;
        crate::agent_registry::trust_permits(&resolved, q.allow_unverified.unwrap_or(false))
            // 403 rather than 400: the request is well-formed and the pack exists;
            // this pod is declining it on policy.
            .map_err(|e| err_json(StatusCode::FORBIDDEN, e))?;
        return match crate::agent_registry::fetch(&resolved.download_url).await {
            // Record the *resolved* origin, not the reference the user typed, so the
            // lockfile pins where the bytes actually came from.
            Ok(b) => Ok(PackBytes {
                bytes: b,
                source: resolved.download_url.clone(),
                resolved: Some(resolved),
            }),
            Err(e) => Err(err_json(StatusCode::BAD_GATEWAY, e)),
        };
    }
    if let Some(url) = q.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        return match crate::agent_registry::fetch(url).await {
            Ok(b) => Ok(PackBytes {
                bytes: b,
                source: url.to_string(),
                resolved: None,
            }),
            // A refused origin is the caller's mistake (400); a registry that failed
            // to answer is not (502).
            Err(e) if e.contains("will not download") => Err(err_json(StatusCode::BAD_REQUEST, e)),
            Err(e) => Err(err_json(StatusCode::BAD_GATEWAY, e)),
        };
    }
    if let Some(path) = q.path.as_deref() {
        return match std::fs::read(path) {
            Ok(b) => Ok(PackBytes {
                bytes: b,
                source: path.to_string(),
                resolved: None,
            }),
            Err(e) => Err(err_json(
                StatusCode::BAD_REQUEST,
                format!("reading {path}: {e}"),
            )),
        };
    }
    if !body.is_empty() {
        return Ok(PackBytes {
            bytes: body.to_vec(),
            source: "upload".to_string(),
            resolved: None,
        });
    }
    Err(err_json(
        StatusCode::BAD_REQUEST,
        // Name `?ref=` first: it is the parameter almost every caller means, and
        // leaving it out of this message cost a client an afternoon — it sent
        // `?reference=`, got told about three things it did not want, and had no
        // way to see that the name was the problem.
        "provide ?ref= (a registry reference), ?url=, ?path=, or upload the \
         .agentpack as the request body"
            .to_string(),
    ))
}

/// What installing this pack would grant, and what it would change.
///
/// The install dialog's whole reason to exist. Without this a client could only
/// show a permission summary *after* installing, which is not consent — or parse the
/// archive itself, duplicating the validator that has to be authoritative anyway.
///
/// Everything here is derived from the archive's own bytes. The consent summary
/// never comes from what the author wrote about their pack.
#[derive(Serialize, utoipa::ToSchema)]
struct AgentPackPreview {
    manifest: crate::agent_packs::AgentPackManifest,
    consent: crate::agent_packs::ConsentSummary,
    /// The single preset this pack provides.
    #[serde(skip_serializing_if = "Option::is_none")]
    preset: Option<String>,
    /// Content hash of the archive as received, so a UI can show what it is about to
    /// install and compare it against what a registry advertised.
    content_sha256: String,
    /// Where the bytes came from — a URL, a path, or `"upload"`.
    source: String,
    /// The version already installed under this id, if any. Present means this is an
    /// upgrade (or a downgrade), not a first install, and the dialog should say so.
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_version: Option<String>,
    /// Credentials the pod does not have yet. A warning, not a blocker: the pack
    /// installs and its tools fail clearly at call time until `key_set` fixes it.
    missing_env: Vec<String>,
    /// Preset slugs another installed pack already provides.
    preset_collisions: Vec<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/agent-packs/inspect",
    tag = "agent-packs",
    params(
        ("url" = Option<String>, Query, description = "Registry URL to download from"),
        ("path" = Option<String>, Query, description = "Local .agentpack path"),
    ),
    request_body(content = Vec<u8>, description = "The .agentpack archive", content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "What installing this would grant", body = AgentPackPreview),
        (status = 400, description = "Not a valid agent pack", body = ErrorResponse),
        (status = 502, description = "The registry could not be reached", body = ErrorResponse),
    ),
)]
async fn post_inspect_agent_pack(
    Query(q): Query<InstallAgentPackQuery>,
    body: axum::body::Bytes,
) -> Response {
    let PackBytes { bytes, source, .. } = match agent_pack_bytes(&q, &body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // The same read the installer performs — traversal guard, size cap, hash check,
    // containment. An archive that fails here is one that would fail to install, and
    // saying so now is the point.
    let bundle = match crate::agent_packs::Bundle::read(&bytes) {
        Ok(b) => b,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
    };

    let installed = crate::agent_packs::find(&bundle.manifest.id);
    let preset = bundle.preset_slug().map(str::to_string);
    // A slug that already resolves on this pod — from a seeded preset, a local one, or
    // another pack. Installing over it does not merely shadow the existing agent: the
    // loader treats ambiguity as an error rather than picking one, so both become
    // unusable. That is worth saying before, not after.
    let existing = crate::agent_preset::AgentPreset::list_summaries(&paths::agent_presets_dir());
    let preset_collisions: Vec<String> = preset
        .iter()
        .filter(|slug| {
            existing
                .iter()
                .any(|p| &&p.slug == slug && p.pack_id.as_deref() != Some(&bundle.manifest.id))
        })
        .cloned()
        .collect();
    let missing_env: Vec<String> = bundle
        .consent
        .requires_env
        .iter()
        .filter(|e| e.required && crate::key_store::lookup(&e.name).is_none())
        .map(|e| e.name.clone())
        .collect();

    Json(AgentPackPreview {
        content_sha256: crate::agent_packs::bundle::content_hash(&bundle.files),
        installed_version: installed.map(|p| p.manifest.version),
        preset,
        consent: bundle.consent,
        manifest: bundle.manifest,
        source,
        missing_env,
        preset_collisions,
    })
    .into_response()
}

/// `POST /api/v1/agent-packs/{id}/update`
///
/// Separate from install because the *consequences* are different: install adds an
/// agent, update changes agents that already exist and that somebody may be talking
/// to right now. The report says what followed — and, crucially, names any agent
/// whose persona or preset the new version withdrew, so the two silent failure modes
/// become two lines in a dialog.
#[utoipa::path(
    post,
    path = "/api/v1/agent-packs/{id}/update",
    tag = "agent-packs",
    params(("id" = String, Path, description = "The installed agent pack to update")),
    request_body(content = Vec<u8>, description = "The newer .agentpack, or use ?url=/?ref=/?path="),
    responses(
        (status = 200, description = "Update report", body = crate::agent_packs::UpdateReport),
        (status = 400, description = "Not installed, older, or invalid"),
    ),
)]
async fn post_update_agent_pack(
    Path(id): Path<String>,
    Query(q): Query<InstallAgentPackQuery>,
    body: axum::body::Bytes,
) -> Response {
    let PackBytes { bytes, source, .. } = match agent_pack_bytes(&q, &body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Check the archive is the pack the caller named *before* updating, not after:
    // a mismatched id must not be discovered once the new version is already on
    // disk. `Bundle::read` is pure, so this costs a parse and nothing else.
    match crate::agent_packs::Bundle::read(&bytes) {
        Ok(b) if b.manifest.id != id => {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!("that archive is agent pack '{}', not '{id}'", b.manifest.id),
            );
        }
        Ok(_) => {}
        Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
    }

    match crate::agent_packs::update(&bytes, &source) {
        Ok(report) => Json(report).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/agent-packs/registries",
    tag = "agent-packs",
    responses((status = 200, description = "Origins this pod will download an agent pack from", body = Registries)),
)]
async fn get_agent_pack_registries() -> Response {
    let cfg = crate::agent_registry::load();
    Json(Registries {
        origins: cfg.origins(),
        default: cfg.default.clone(),
        registries: cfg
            .registries
            .iter()
            .map(|(name, r)| RegistryView {
                name: name.clone(),
                url: r.url.clone(),
                trust: r.trust,
                is_default: *name == cfg.default,
            })
            .collect(),
    })
    .into_response()
}

/// Where this pod is willing to fetch an agent pack from.
///
/// Returned rather than only enforced so a UI can say what it accepts *before* the
/// user pastes a link and gets refused — and so a picker can offer the configured
/// hosts by name rather than making somebody remember an origin.
#[derive(Serialize, utoipa::ToSchema)]
struct Registries {
    /// Bare origins, kept for clients written against the earlier shape.
    origins: Vec<String>,
    default: String,
    registries: Vec<RegistryView>,
}

#[derive(Serialize, utoipa::ToSchema)]
struct RegistryView {
    name: String,
    url: String,
    trust: crate::agent_registry::Trust,
    is_default: bool,
}

// ── Registry connection ──────────────────────────────────────────────────────
//
// Browsing and installing a *public* pack needs no credential, so none of this is on
// the critical path for "install the agent I just found". What it buys is the rest:
// private packs, and a host that can say which account this pod belongs to. The
// credential is the one the pod already holds — connecting points a registry at it.

/// One place where a registry failure becomes a status code.
///
/// The distinctions are the ones a UI acts on differently: a typo in a name, a
/// malformed reference, a host that does not have the pack, a host that does not offer
/// this part of the protocol at all, and a host having a bad day. Reading them off a
/// typed error rather than off message text means rewording an error cannot silently
/// change what the API returns.
fn registry_error(e: crate::agent_registry::RegistryError) -> Response {
    use crate::agent_registry::RegistryError as E;
    let code = match &e {
        E::Unknown(_) | E::NotFound(_) => StatusCode::NOT_FOUND,
        E::BadReference(_) => StatusCode::BAD_REQUEST,
        E::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
        // Not a failure: the operator configured this pod by environment variable and
        // this is us declining to write a file that would never be read.
        E::Locked(_) => StatusCode::CONFLICT,
        E::Host(_) => StatusCode::BAD_GATEWAY,
    };
    err_json(code, e.to_string())
}

/// What this pod is to Octaweave, and whether the tools are installed.
///
/// A read, not a write: there is nothing to configure. The pod presents the Metalcraft
/// token it already holds, and the only human step is linking the two accounts once —
/// which happens on Octaweave's own page, at `link_url`.
#[utoipa::path(
    get,
    path = "/api/v1/services/octaweave",
    tag = "agent-packs",
    responses((status = 200, description = "This pod's standing with Octaweave", body = crate::octaweave::OctaweaveConnection)),
)]
async fn get_octaweave_status() -> Response {
    Json(crate::octaweave::status().await).into_response()
}

#[utoipa::path(
    get,
    path = "/api/v1/agent-packs/registries/{name}/status",
    tag = "agent-packs",
    params(("name" = String, Path, description = "A configured registry name")),
    responses(
        (status = 200, description = "What this pod is to that registry", body = crate::agent_registry::Connection),
        (status = 404, description = "No such registry is configured", body = ErrorResponse),
    ),
)]
async fn get_registry_status(Path(name): Path<String>) -> Response {
    match crate::agent_registry::status(&name).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct ConnectRegistryQuery {
    /// Which key-store entry holds the bearer to send. Defaults to this pod's
    /// Metalcraft ID token — the point of the whole exercise is that no new credential
    /// is created, so the default is the one that already exists.
    #[serde(default)]
    token_key: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/agent-packs/registries/{name}/connect",
    tag = "agent-packs",
    params(
        ("name" = String, Path, description = "A configured registry name"),
        ("token_key" = Option<String>, Query, description = "Key-store entry to draw the bearer from; defaults to METALCRAFT_TOKEN"),
    ),
    responses(
        (status = 200, description = "Where the connection got to", body = crate::agent_registry::Connection),
        (status = 404, description = "No such registry is configured", body = ErrorResponse),
        (status = 409, description = "Registries come from AGENT_PACK_REGISTRIES and cannot be edited", body = ErrorResponse),
    ),
)]
async fn post_registry_connect(
    Path(name): Path<String>,
    Query(q): Query<ConnectRegistryQuery>,
) -> Response {
    let key = q
        .token_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .unwrap_or(crate::agent_registry::POD_TOKEN_KEY);
    match crate::agent_registry::connect(&name, key).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => registry_error(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/agent-packs/registries/{name}/disconnect",
    tag = "agent-packs",
    params(("name" = String, Path, description = "A configured registry name")),
    responses(
        (status = 200, description = "The registry, now anonymous", body = crate::agent_registry::Connection),
        (status = 404, description = "No such registry is configured", body = ErrorResponse),
        (status = 409, description = "Registries come from AGENT_PACK_REGISTRIES and cannot be edited", body = ErrorResponse),
    ),
)]
async fn post_registry_disconnect(Path(name): Path<String>) -> Response {
    match crate::agent_registry::disconnect(&name).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct RegistrySearchQuery {
    /// Free text. Omitted asks the host for whatever it puts forward, which is a
    /// browse list rather than an empty search.
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/v1/agent-packs/registries/{name}/search",
    tag = "agent-packs",
    params(
        ("name" = String, Path, description = "A configured registry name"),
        ("q" = Option<String>, Query, description = "Search text; omit to browse"),
        ("limit" = Option<u32>, Query, description = "1–100, default 25"),
    ),
    responses(
        (status = 200, description = "What that host publishes"),
        (status = 404, description = "No such registry is configured", body = ErrorResponse),
        (status = 501, description = "That host is fetch-only and does not offer search", body = ErrorResponse),
        (status = 502, description = "The host could not be searched", body = ErrorResponse),
    ),
)]
async fn get_registry_search(
    Path(name): Path<String>,
    Query(q): Query<RegistrySearchQuery>,
) -> Response {
    match crate::agent_registry::search(&name, q.q.as_deref(), q.limit.unwrap_or(25)).await {
        Ok(results) => {
            Json(serde_json::json!({ "registry": name, "results": results })).into_response()
        }
        Err(e) => registry_error(e),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/agent-packs/registries/{name}/packs/{id}/manifest",
    tag = "agent-packs",
    params(
        ("name" = String, Path, description = "A configured registry name"),
        ("id" = String, Path, description = "The pack's handle on that host"),
    ),
    responses(
        (status = 200, description = "The pack's manifest, as the host serves it"),
        (status = 400, description = "That is not a usable pack id", body = ErrorResponse),
        (status = 404, description = "No such registry, or no such pack on it", body = ErrorResponse),
        (status = 502, description = "The host would not serve it", body = ErrorResponse),
    ),
)]
async fn get_registry_manifest(Path((name, id)): Path<(String, String)>) -> Response {
    match crate::agent_registry::manifest(&name, &id).await {
        Ok(m) => Json(m).into_response(),
        Err(e) => registry_error(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/agent-packs/install",
    tag = "agent-packs",
    params(("path" = Option<String>, Query, description = "Local .agentpack path; omit to upload the archive as the body")),
    request_body(content = Vec<u8>, description = "The .agentpack archive", content_type = "application/octet-stream"),
    responses((status = 200, description = "Install report"), (status = 400, description = "Rejected")),
)]
async fn post_install_agent_pack(
    Query(q): Query<InstallAgentPackQuery>,
    body: axum::body::Bytes,
) -> Response {
    let PackBytes {
        bytes,
        source,
        resolved,
    } = match agent_pack_bytes(&q, &body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match crate::agent_packs::install(&bytes, &source) {
        Ok(report) => {
            // Told after the fact, in the background, and only when a registry is the
            // one that answered. The install has already happened locally; a host that
            // is down must not delay the response, and cannot undo it.
            if let Some(resolved) = resolved {
                tokio::spawn(async move { crate::agent_registry::report_install(&resolved).await });
            }
            Json(report).into_response()
        }
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct UninstallQuery {
    #[serde(default)]
    force: Option<bool>,
}

#[utoipa::path(
    delete,
    path = "/api/v1/agent-packs/{id}",
    tag = "agent-packs",
    params(("id" = String, Path, description = "Agent pack id"),
           ("force" = Option<bool>, Query, description = "Orphan any saved agents that use it")),
    responses((status = 200, description = "Uninstall report"), (status = 409, description = "In use")),
)]
async fn delete_agent_pack(Path(id): Path<String>, Query(q): Query<UninstallQuery>) -> Response {
    match crate::agent_packs::uninstall(&id, q.force.unwrap_or(false)) {
        Ok(report) => Json(report).into_response(),
        // "In use" is a conflict, not a 404 — the caller can retry with force.
        Err(e) if e.contains("in use by") => err_json(StatusCode::CONFLICT, e),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct ExportAgentPackRequest {
    preset: String,
    #[serde(default)]
    version: Option<String>,
    /// Write the archive here. Omit to receive the bytes in the response.
    #[serde(default)]
    out: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/agent-packs/export",
    tag = "agent-packs",
    request_body = ExportAgentPackRequest,
    responses((status = 200, description = "The .agentpack bytes, or a report if `out` was given")),
)]
async fn post_export_agent_pack(Json(req): Json<ExportAgentPackRequest>) -> Response {
    let version = req.version.as_deref().unwrap_or("0.1.0");
    let bytes = match crate::agent_packs::export(&req.preset, version) {
        Ok(b) => b,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
    };
    match req.out {
        Some(out) => match std::fs::write(&out, &bytes) {
            Ok(()) => {
                Json(serde_json::json!({ "path": out, "bytes": bytes.len() })).into_response()
            }
            Err(e) => err_json(StatusCode::BAD_REQUEST, format!("writing {out}: {e}")),
        },
        None => (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"agent.agentpack\"",
                ),
            ],
            bytes,
        )
            .into_response(),
    }
}

// ── Agent instances ──────────────────────────────────────────────────────────
// An instance is a live agent: a preset, a name, and (from AP4) its own memory.
// Conversations are the chats that belong to it.

#[derive(Serialize, utoipa::ToSchema)]
struct InstanceDetail {
    #[serde(flatten)]
    instance: crate::agent_instance::AgentInstance,
    conversations: Vec<ChatSummary>,
    /// What this agent is scheduled to do — the schedules armed to it. A pod
    /// could not previously answer that question about a background agent.
    scheduled: Vec<ScheduledFlowRow>,
}

fn scheduled_for(instance_id: &str) -> Vec<ScheduledFlowRow> {
    crate::scheduled_flows::for_instance(instance_id)
        .into_iter()
        .map(scheduled_row)
        .collect()
}

/// `GET /api/v1/agents/instances/{id}/flows` — what this agent is scheduled to do.
///
/// The same list `GET …/instances/{id}` embeds, on its own route. It is worth its own
/// endpoint because it is the question somebody asks right before they decide whether
/// to trust a background agent, and answering it should not mean fetching an agent's
/// whole conversation index.
#[utoipa::path(
    get,
    path = "/api/v1/agents/instances/{id}/flows",
    tag = "agent-instances",
    params(("id" = String, Path, description = "Agent instance id")),
    responses(
        (status = 200, description = "Flow schedules armed to this agent", body = InstanceFlows),
        (status = 404, description = "No such agent"),
    ),
)]
async fn get_agent_instance_flows(Path(id): Path<String>) -> Response {
    if crate::agent_instance::load(&id).is_err() {
        return err_json(StatusCode::NOT_FOUND, format!("no agent '{id}'"));
    }
    Json(InstanceFlows {
        scheduled: scheduled_for(&id),
    })
    .into_response()
}

#[derive(Serialize, utoipa::ToSchema)]
struct InstanceFlows {
    scheduled: Vec<ScheduledFlowRow>,
}

fn conversations_of(instance_id: &str) -> Vec<ChatSummary> {
    let mut out: Vec<ChatSummary> = read_persisted_chats()
        .into_iter()
        .filter(|c| c.instance_id.as_deref() == Some(instance_id))
        .map(ChatSummary::of)
        .collect();
    out.sort_by(|a, b| b.recency().cmp(a.recency()));
    out
}

/// An agent in a list, with the one derived number a list actually needs.
///
/// Typed rather than patched into a `serde_json::Value` so it reaches the spec —
/// `conversation_count` was invisible to both generated clients, which is how a
/// field that exists on the wire can still be unusable.
#[derive(Serialize, utoipa::ToSchema)]
struct InstanceListItem {
    #[serde(flatten)]
    instance: crate::agent_instance::AgentInstance,
    conversation_count: usize,
}

#[derive(Serialize, utoipa::ToSchema)]
struct InstanceList {
    instances: Vec<InstanceListItem>,
}

#[utoipa::path(
    get,
    path = "/api/v1/agents/instances",
    tag = "agent-instances",
    responses((status = 200, description = "Live agents on this pod", body = InstanceList)),
)]
async fn list_agent_instances() -> Response {
    let instances: Vec<InstanceListItem> = crate::agent_instance::list()
        .into_iter()
        .map(|i| InstanceListItem {
            conversation_count: conversations_of(&i.id).len(),
            instance: i,
        })
        .collect();
    Json(InstanceList { instances }).into_response()
}

#[utoipa::path(
    get,
    path = "/api/v1/agents/instances/{id}",
    tag = "agent-instances",
    params(("id" = String, Path, description = "Instance id")),
    responses((status = 200, body = InstanceDetail), (status = 404, description = "Not found")),
)]
async fn get_agent_instance(Path(id): Path<String>) -> Response {
    match crate::agent_instance::load(&id) {
        Ok(instance) => {
            let conversations = conversations_of(&instance.id);
            let scheduled = scheduled_for(&instance.id);
            Json(InstanceDetail {
                instance,
                conversations,
                scheduled,
            })
            .into_response()
        }
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct CreateInstanceRequest {
    #[serde(default)]
    agent_preset: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/agents/instances",
    tag = "agent-instances",
    request_body = CreateInstanceRequest,
    responses((status = 200, description = "Created")),
)]
async fn post_create_agent_instance(Json(req): Json<CreateInstanceRequest>) -> Response {
    use crate::agent_instance::{AgentInstance, InstanceOrigin};
    let slug = req
        .agent_preset
        .as_deref()
        .unwrap_or(crate::agent_preset::DEFAULT_PRESET);
    let preset = match crate::agent_preset::AgentPreset::load(slug, &paths::agent_presets_dir()) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
    };
    if let Err(e) = preset.ensure_spawnable() {
        return err_json(StatusCode::BAD_REQUEST, e);
    }
    let mut instance = AgentInstance::new(&preset, InstanceOrigin::Workshop);
    if let Some(name) = req.name {
        instance.name = name;
    }
    match instance.save() {
        Ok(()) => Json(instance).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/agents/instances/{id}",
    tag = "agent-instances",
    params(("id" = String, Path, description = "Instance id")),
    responses((status = 200, description = "Deleted"), (status = 404, description = "Not found")),
)]
async fn delete_agent_instance(Path(id): Path<String>) -> Response {
    // Deleting an agent a cron still fires into would leave the schedule pointing at
    // nothing — it would run, but memoryless, and nobody would be told why. Refuse
    // and name the flows so the fix is obvious.
    let scheduled = scheduled_for(&id);
    if !scheduled.is_empty() {
        let what: Vec<String> = scheduled
            .iter()
            .map(|row| {
                format!(
                    "{} ({})",
                    row.flow_name
                        .as_deref()
                        .unwrap_or(&row.scheduled.flow_id),
                    row.scheduled.schedule.display_name()
                )
            })
            .collect();
        return err_json(
            StatusCode::CONFLICT,
            format!(
                "agent '{id}' still runs scheduled flows: {}. Disarm those schedules first.",
                what.join("; ")
            ),
        );
    }
    // Conversations survive deliberately: losing an agent should not lose transcripts.
    let orphaned = conversations_of(&id).len();
    match crate::agent_instance::delete(&id) {
        Ok(()) => Json(serde_json::json!({ "deleted": id, "conversations_kept": orphaned }))
            .into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct PatchInstanceRequest {
    #[serde(default)]
    name: Option<String>,
    /// Move within the preset's roster. Rejected if the persona isn't in it.
    #[serde(default)]
    persona: Option<String>,
}

#[utoipa::path(
    patch,
    path = "/api/v1/agents/instances/{id}",
    tag = "agent-instances",
    params(("id" = String, Path, description = "Instance id")),
    request_body = PatchInstanceRequest,
    responses((status = 200, description = "Updated"), (status = 404, description = "Not found")),
)]
async fn patch_agent_instance(
    Path(id): Path<String>,
    Json(req): Json<PatchInstanceRequest>,
) -> Response {
    let mut instance = match crate::agent_instance::load(&id) {
        Ok(i) => i,
        Err(e) => return err_json(StatusCode::NOT_FOUND, e),
    };
    if let Some(name) = req.name {
        // A rename is a rename, and now it could not be anything else: the
        // `persistent` flag it used to set alongside the name — silently changing
        // how long the pod kept the agent, from a text field that says nothing
        // about lifetimes — does not exist. Nothing deletes an agent on a timer.
        instance.name = name;
    }
    if let Some(persona) = req.persona {
        match crate::agent_preset::AgentPreset::load(
            &instance.agent_preset,
            &paths::agent_presets_dir(),
        ) {
            Ok(preset) if !preset.allows_persona(&persona) => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "persona '{persona}' is not in agent '{}' (roster: {})",
                        preset.slug,
                        preset.callable_personas().join(", ")
                    ),
                );
            }
            // A preset that no longer resolves must not lock an agent out of its own
            // persona switch; the orphan case is reported elsewhere.
            _ => {}
        }
        instance.persona = persona;
    }
    instance.touch();
    match instance.save() {
        Ok(()) => Json(instance).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct MemoryViewQuery {
    #[serde(default)]
    limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/api/v1/agents/instances/{id}/memory",
    tag = "agent-instances",
    params(("id" = String, Path, description = "Instance id")),
    responses((status = 200, description = "What this agent knows, shipped vs learned")),
)]
async fn get_agent_instance_memory(
    Path(id): Path<String>,
    Query(q): Query<MemoryViewQuery>,
) -> Response {
    if crate::agent_instance::load(&id).is_err() {
        return err_json(
            StatusCode::NOT_FOUND,
            format!("agent instance '{id}' not found"),
        );
    }
    let view = crate::memory::instance_view(&id, q.limit.unwrap_or(50).clamp(1, 500)).await;
    Json(view).into_response()
}

#[derive(Deserialize, utoipa::ToSchema)]
struct NewConversationRequest {
    #[serde(default)]
    model_name: Option<String>,
    /// Start this conversation as a specific persona from the agent's roster.
    #[serde(default)]
    persona_slug: Option<String>,
}

/// Every conversation this agent has had, newest activity first.
///
/// The same list `GET …/instances/{id}` embeds, on its own route — a session list
/// reloads whenever a conversation changes, and it has no use for the agent's
/// memory or its schedules, which that endpoint also pays to assemble.
#[utoipa::path(
    get,
    path = "/api/v1/agents/instances/{id}/conversations",
    tag = "agent-instances",
    params(("id" = String, Path, description = "Instance id")),
    responses(
        (status = 200, body = Vec<ChatSummary>),
        (status = 404, description = "No such agent"),
    ),
)]
async fn list_instance_conversations(Path(id): Path<String>) -> Response {
    if crate::agent_instance::load(&id).is_err() {
        return err_json(StatusCode::NOT_FOUND, format!("agent '{id}' not found"));
    }
    Json(conversations_of(&id)).into_response()
}

#[utoipa::path(
    post,
    path = "/api/v1/agents/instances/{id}/conversations",
    tag = "agent-instances",
    params(("id" = String, Path, description = "Instance id")),
    request_body = NewConversationRequest,
    responses((status = 200, body = ChatSummary)),
)]
async fn post_instance_conversation(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Json(req): Json<NewConversationRequest>,
) -> Response {
    // Continuing an existing agent is the same operation as starting a chat; this
    // route just makes it discoverable from the instance you are looking at.
    post_create_chat(
        State(state),
        Json(CreateChatRequest {
            persona_slug: req.persona_slug,
            model_name: req.model_name,
            agent_preset: None,
            instance_id: Some(id),
            name: None,
        }),
    )
    .await
}

// ── Agent presets ────────────────────────────────────────────────────────────
// A preset is what a user picks when starting a chat; the persona underneath is an
// implementation detail of it. Pack-provided presets are read-only, exactly like
// pack-provided personas and skills.

#[utoipa::path(
    get,
    path = "/api/v1/agent-presets",
    responses((status = 200, description = "Installed agent presets")),
    tag = "agent-presets"
)]
async fn list_agent_presets() -> Response {
    let summaries = crate::agent_preset::AgentPreset::list_summaries(&paths::agent_presets_dir());
    Json(serde_json::json!({
        "presets": summaries,
        "default": crate::agent_preset::DEFAULT_PRESET,
    }))
    .into_response()
}

/// A preset with its roster resolved against what this pod actually has.
///
/// Typed rather than assembled ad hoc so it reaches `openapi.json`: both Workshop
/// frontends generate their client types from the spec, and a response described only
/// by a prose `description` gives them nothing to generate.
#[derive(Serialize, utoipa::ToSchema)]
struct PresetDetail {
    preset: crate::agent_preset::AgentPreset,
    personas: Vec<RosterPersona>,
}

/// One entry in a preset's roster.
///
/// `installed: false` is the interesting case: the preset names a persona this pod
/// does not have. A UI should render it disabled with its `error` rather than
/// silently omit it — omission looks like the preset is smaller than it is.
#[derive(Serialize, utoipa::ToSchema)]
struct RosterPersona {
    slug: String,
    installed: bool,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    /// Why it could not be resolved. Present only when `installed` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/agent-presets/{slug}",
    params(("slug" = String, Path, description = "Preset slug")),
    responses((status = 200, description = "The preset, with its roster resolved", body = PresetDetail),
              (status = 404, body = ErrorResponse)),
    tag = "agent-presets"
)]
async fn get_agent_preset(Path(slug): Path<String>) -> Response {
    let preset = match crate::agent_preset::AgentPreset::load(&slug, &paths::agent_presets_dir()) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::NOT_FOUND, e),
    };
    // Resolve the roster so the caller can render it without N more round-trips,
    // and so a preset naming a persona that isn't installed is visible as such.
    let personas: Vec<RosterPersona> = preset
        .callable_personas()
        .iter()
        .map(|slug| match Persona::load(slug, &paths::personas_dir()) {
            Ok(p) => RosterPersona {
                slug: slug.clone(),
                installed: true,
                name: p.name.clone(),
                description: p.description.clone(),
                tools: p.resolved_tool_names(),
                skills: p.skills.clone(),
                error: None,
            },
            Err(e) => RosterPersona {
                // Name it after its slug rather than leaving it blank: a disabled row
                // reading "morning-briefer — not installed" says more than an empty one.
                name: slug.clone(),
                slug: slug.clone(),
                installed: false,
                description: String::new(),
                tools: Vec::new(),
                skills: Vec::new(),
                error: Some(e),
            },
        })
        .collect();
    Json(PresetDetail { preset, personas }).into_response()
}

#[utoipa::path(
    put,
    path = "/api/v1/agent-presets/{slug}",
    params(("slug" = String, Path, description = "Preset slug")),
    responses((status = 200, description = "Saved"), (status = 409, description = "Owned by a pack")),
    tag = "agent-presets"
)]
async fn put_agent_preset(
    Path(slug): Path<String>,
    Json(mut preset): Json<crate::agent_preset::AgentPreset>,
) -> Response {
    let dir = paths::agent_presets_dir();
    // A pack-owned slug is read-only here; pick a different one rather than
    // shadowing a read-only entry through the API.
    if !dir.join(format!("{slug}.json")).exists() {
        if let Ok(existing) = crate::agent_preset::AgentPreset::load(&slug, &dir) {
            let _ = existing;
            if let Some(summary) = crate::agent_preset::AgentPreset::list_summaries(&dir)
                .into_iter()
                .find(|s| s.slug == slug)
            {
                if let Some(pack_id) = summary.pack_id {
                    return err_json(
                        StatusCode::CONFLICT,
                        format!(
                            "agent preset '{slug}' is provided by the '{pack_id}' pack and is read-only. Choose a different slug."
                        ),
                    );
                }
            }
        }
    }
    preset.slug = slug.clone();
    match preset.save(&dir) {
        Ok(()) => Json(serde_json::json!({ "saved": slug })).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/agent-presets/{slug}",
    params(("slug" = String, Path, description = "Preset slug")),
    responses((status = 200, description = "Deleted"), (status = 404, description = "Not user-owned")),
    tag = "agent-presets"
)]
async fn delete_agent_preset(Path(slug): Path<String>) -> Response {
    match crate::agent_preset::AgentPreset::delete(&slug, &paths::agent_presets_dir()) {
        Ok(()) => Json(serde_json::json!({ "deleted": slug })).into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/personas",
    tag = "personas",
    summary = "Every persona on this pod: the pod's own, plus those an enabled pack vendors.",
    description = "Locals shadow packs on a slug collision, which is the same precedence the\nrunner resolves with — so what is listed here is what would actually run.\n\nSummaries only. A persona's system prompt is most of its bytes and none of an\nindex's job; `GET /personas/{slug}` is the whole document.",
    responses((status = 200, body = Vec<PersonaSummary>)),
)]
async fn list_personas() -> Json<Vec<PersonaSummary>> {
    Json(list_persona_summaries())
}

#[utoipa::path(
    get,
    path = "/api/v1/skills",
    tag = "skills",
    summary = "Every skill on this pod: the pod's own, plus those an enabled pack vendors.",
    description = "Summaries only — a skill *is* its markdown body, so an index that carried it\nwould be the whole skills directory in one response. `GET /skills/{slug}` has\nthe body.",
    responses((status = 200, body = Vec<SkillSummary>)),
)]
async fn list_skills() -> Json<Vec<SkillSummary>> {
    Json(list_skill_summaries())
}

#[utoipa::path(
    get,
    path = "/api/v1/personas/{slug}",
    tag = "personas",
    params(("slug" = String, Path, description = "Persona slug")),
    responses((status = 200, body = crate::persona::Persona), (status = 404, body = ErrorResponse)),
)]
async fn get_persona(Path(slug): Path<String>) -> Response {
    let filename = format!("{slug}.json");
    let Some((path, _origin)) =
        crate::integrations::resolve_file(&paths::personas_dir(), "personas", &filename)
    else {
        return err_json(StatusCode::NOT_FOUND, format!("persona '{slug}' not found"));
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return err_json(StatusCode::NOT_FOUND, format!("persona '{slug}' not found"));
    };
    match serde_json::from_str::<Persona>(&content) {
        Ok(persona) => Json(persona).into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to parse: {e}"),
        ),
    }
}

#[utoipa::path(
    put,
    path = "/api/v1/personas/{slug}",
    tag = "personas",
    params(("slug" = String, Path, description = "Persona slug")),
    request_body = crate::persona::Persona,
    responses((status = 200, description = "Saved")),
)]
async fn put_persona(Path(slug): Path<String>, Json(persona): Json<Persona>) -> Response {
    // Reject if this slug is currently owned by a pack — the user must pick
    // a different slug instead of trying to shadow a read-only entry through
    // this endpoint. (Genuine local shadows happen when the user creates a
    // local file with the same slug via the filesystem.)
    let filename = format!("{slug}.json");
    let local_exists = paths::personas_dir().join(&filename).exists();
    if !local_exists {
        if let Some((_, origin)) =
            crate::integrations::resolve_file(&paths::personas_dir(), "personas", &filename)
        {
            if let Some(pack_id) = origin.pack_id() {
                return err_json(
                    StatusCode::CONFLICT,
                    format!(
                        "persona '{slug}' is provided by the '{pack_id}' integration and is read-only. Choose a different slug."
                    ),
                );
            }
        }
    }
    match persona.save(&slug, &paths::personas_dir()) {
        Ok(()) => Json(persona).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/personas/{slug}",
    tag = "personas",
    params(("slug" = String, Path, description = "Persona slug")),
    responses((status = 200, description = "Deleted")),
)]
async fn delete_persona(Path(slug): Path<String>) -> Response {
    let local = paths::personas_dir().join(format!("{slug}.json"));
    if !local.exists() {
        // Either the slug doesn't exist at all, or it's pack-owned. Either
        // way the user can't delete it through this endpoint.
        return err_json(
            StatusCode::NOT_FOUND,
            format!("persona '{slug}' is not a user-local persona"),
        );
    }
    match Persona::delete(&slug, &paths::personas_dir()) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => err_json(StatusCode::NOT_FOUND, format!("persona '{slug}' not found")),
    }
}

// ── Skill handlers ──────────────────────────────────────────────────────
// Skill types and CRUD live in `crate::skill` so the meta tools share them.

#[utoipa::path(
    get,
    path = "/api/v1/skills/{slug}",
    tag = "skills",
    params(("slug" = String, Path, description = "Skill slug")),
    responses((status = 200, body = crate::skill::Skill), (status = 404, body = ErrorResponse)),
)]
async fn get_skill(Path(slug): Path<String>) -> Response {
    match load_skill(&slug) {
        Some(skill) => Json(skill).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("skill '{slug}' not found")),
    }
}

#[utoipa::path(
    put,
    path = "/api/v1/skills/{slug}",
    tag = "skills",
    params(("slug" = String, Path, description = "Skill slug")),
    request_body = crate::skill::Skill,
    responses((status = 200, description = "Saved")),
)]
async fn put_skill(Path(slug): Path<String>, Json(skill): Json<Skill>) -> Response {
    // Block writing to a slug that's currently provided by a pack (the user
    // would otherwise be shadowing read-only content silently).
    let filename = format!("{slug}.md");
    let local_exists = paths::skills_dir().join(&filename).exists();
    if !local_exists {
        if let Some((_, origin)) =
            crate::integrations::resolve_file(&paths::skills_dir(), "skills", &filename)
        {
            if let Some(pack_id) = origin.pack_id() {
                return err_json(
                    StatusCode::CONFLICT,
                    format!(
                        "skill '{slug}' is provided by the '{pack_id}' integration and is read-only. Choose a different slug."
                    ),
                );
            }
        }
    }
    match save_skill(&slug, &skill) {
        Ok(()) => Json(Skill {
            slug,
            description: skill.description,
            body: skill.body,
            pack_id: None,
            read_only: false,
        })
        .into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/skills/{slug}",
    tag = "skills",
    params(("slug" = String, Path, description = "Skill slug")),
    responses((status = 200, description = "Deleted")),
)]
async fn delete_skill(Path(slug): Path<String>) -> Response {
    let path = paths::skills_dir().join(format!("{slug}.md"));
    if !path.exists() {
        return err_json(
            StatusCode::NOT_FOUND,
            format!("skill '{slug}' is not a user-local skill"),
        );
    }
    match std::fs::remove_file(&path) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete: {e}"),
        ),
    }
}

// ── Flow handlers ───────────────────────────────────────────────────────

// ── Flow listing ────────────────────────────────────────────────────────

/// One flow, resolved for display.
///
/// A flow is *what work is this*: the graph, the agent preset it runs as, and how
/// many schedules point at it. **When** it runs is not here — that is
/// `GET /api/v1/scheduled-flows`, one call for the whole pod, which a client joins
/// against this listing by `flow_id`.
#[derive(Serialize, utoipa::ToSchema)]
struct FlowListItem {
    id: String,
    name: String,
    node_count: usize,
    created_at: String,
    updated_at: String,
    /// v2 flows run on the state-machine executor and `POST /run` answers with a
    /// `FlowRunSummary`; v1 flows answer with the legacy per-prompt response. Worth
    /// knowing before offering a button that has to render one of them.
    v2: bool,
    /// The preset this flow runs as. Always populated — an unbound flow resolves to
    /// the default agent, which is what it effectively already was.
    preset: String,
    /// How many schedules point at this flow, of which how many are enabled.
    ///
    /// Enough for a listing to say "runs twice a day" or "never runs" without
    /// fetching every schedule; a client that wants the triggers themselves reads
    /// `/scheduled-flows`.
    scheduled_count: usize,
    /// Of `scheduled_count`, how many are enabled. Zero means nothing fires.
    enabled_count: usize,
}

#[derive(Serialize, utoipa::ToSchema)]
struct FlowList {
    flows: Vec<FlowListItem>,
}

fn flow_list_item(flow: &metalcraft_flows::SavedFlow) -> FlowListItem {
    let scheduled = crate::scheduled_flows::for_flow(&flow.id);
    FlowListItem {
        id: flow.id.clone(),
        name: flow.name.clone(),
        node_count: flow.flow.nodes.len(),
        created_at: flow.created_at.clone(),
        updated_at: flow.updated_at.clone(),
        v2: crate::flow_exec::is_v2_flow(flow),
        preset: crate::flow_bindings::preset_for(&flow.id),
        enabled_count: scheduled.iter().filter(|sf| sf.enabled).count(),
        scheduled_count: scheduled.len(),
    }
}

/// `GET /api/v1/flows` — every flow on this pod.
///
/// The listing the API never had: until now a client had to already know a flow's
/// id to see anything at all, which made "show me what this pod is set up to do"
/// unanswerable. See `docs/FLOWS_AS_AGENTS_PLAN.md` §3.
#[utoipa::path(
    get,
    path = "/api/v1/flows",
    tag = "flows",
    responses((status = 200, description = "Every flow installed on this pod", body = FlowList)),
)]
async fn list_flows() -> Response {
    let dir = paths::flows_dir();
    // `metalcraft_flows::list_flows` sorts newest-edited first; reload each flow to
    // resolve its preset and schedule counts, keeping that order.
    let flows: Vec<FlowListItem> = metalcraft_flows::list_flows(&dir)
        .into_iter()
        .filter_map(|summary| metalcraft_flows::load_flow(&dir, &summary.id))
        .map(|flow| flow_list_item(&flow))
        .collect();
    Json(FlowList { flows }).into_response()
}

#[utoipa::path(
    get,
    path = "/api/v1/flows/{id}",
    tag = "flows",
    params(("id" = String, Path, description = "Flow id")),
    responses(
        (status = 200, description = "The saved flow, graph included", body = metalcraft_flows::SavedFlow),
        (status = 404, body = ErrorResponse),
    ),
)]
async fn get_flow(Path(id): Path<String>) -> Response {
    match metalcraft_flows::load_flow(&paths::flows_dir(), &id) {
        Some(flow) => Json(flow).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found")),
    }
}

/// What is wrong with a flow graph, without saving it.
#[derive(Serialize, utoipa::ToSchema)]
struct FlowValidation {
    /// True when the graph would save. An editor enables its save button on this
    /// rather than on an empty `errors`, so a future non-fatal warning does not
    /// silently start blocking saves.
    valid: bool,
    /// One sentence per problem, in the validator's own words.
    errors: Vec<String>,
}

/// Check a flow graph without persisting it.
///
/// `PUT /flows/{id}` already validates before saving and is still the authority —
/// this exists so an editor can say what is wrong *while someone is typing*,
/// which is the only time the answer can still change what they do. Same
/// `metalcraft_flows::validate` behind both, so the two can never disagree.
///
/// Not a 400: an invalid graph is the expected answer to this question, not a bad
/// request. The status says the check ran; the body says what it found.
#[utoipa::path(
    post,
    path = "/api/v1/flows/validate",
    tag = "flows",
    request_body = metalcraft_flows::SavedFlow,
    responses((status = 200, body = FlowValidation)),
)]
async fn post_validate_flow(Json(flow): Json<metalcraft_flows::SavedFlow>) -> Response {
    let errors: Vec<String> = metalcraft_flows::validate(&flow)
        .into_iter()
        .map(|e| e.to_string())
        .collect();
    Json(FlowValidation {
        valid: errors.is_empty(),
        errors,
    })
    .into_response()
}

#[utoipa::path(
    put,
    path = "/api/v1/flows/{id}",
    tag = "flows",
    params(("id" = String, Path, description = "Flow id")),
    request_body = metalcraft_flows::SavedFlow,
    responses(
        (status = 200, description = "Saved, with the server-stamped `updated_at`", body = metalcraft_flows::SavedFlow),
        (status = 400, description = "The graph is invalid; the body lists why", body = ErrorResponse),
        (status = 409, description = "The flow changed since it was loaded — `updated_at` does not match", body = ErrorResponse),
    ),
)]
async fn put_flow(
    Path(id): Path<String>,
    Json(mut flow): Json<metalcraft_flows::SavedFlow>,
) -> Response {
    flow.id = id;

    // Refuse a save built on a version of this flow that is no longer the
    // current one.
    //
    // `updated_at` is the precondition: a client sends back the document it
    // loaded, so a mismatch means somebody else saved in between and this save
    // would erase their work without either person seeing anything. Two people —
    // or one person on a phone and a desktop — editing the same automation is
    // the ordinary case now that both clients can edit, and last-writer-wins is
    // only invisible until it costs somebody an afternoon.
    //
    // Absent for a flow that does not exist yet, because there is nothing to
    // conflict with. Every *other* writer on this pod (pack install, the agent's
    // own `flow_*` tools, schedule migration) calls `save_flow` directly and is
    // deliberately not subject to this: they are not editing somebody's draft,
    // they are installing or migrating, and failing those on a timestamp would
    // break an install for a reason nobody could act on.
    if let Some(current) = metalcraft_flows::load_flow(&paths::flows_dir(), &flow.id)
        && current.updated_at != flow.updated_at
    {
        return err_json(
            StatusCode::CONFLICT,
            format!(
                "This flow changed since you opened it (it was last saved at {}, \
                 and you started from {}). Reload it and make the change again — \
                 saving now would erase whatever the other edit did.",
                current.updated_at, flow.updated_at
            ),
        );
    }

    // Saving a graph can no longer break a timer: the schedules pointing at this
    // flow are separate documents and are not touched here. What a bad save can
    // still do is make the flow unrunnable, which the daemon reports when the
    // schedule next comes due.
    let errors = metalcraft_flows::validate(&flow);
    if !errors.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            errors
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        );
    }

    // Stamped here, not by the client. The precondition above is only meaningful
    // if this value is the pod's to set — a client that could choose its own
    // could hand back the one it read and defeat the check without meaning to.
    // The saved document comes back in the response, so the caller has the new
    // value to base its next save on.
    flow.updated_at = chrono::Utc::now().to_rfc3339();

    match metalcraft_flows::save_flow(&paths::flows_dir(), &flow) {
        Ok(()) => Json(flow).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/flows/{id}",
    tag = "flows",
    params(("id" = String, Path, description = "Flow id")),
    responses((status = 200, description = "Deleted")),
)]
async fn delete_flow(Path(id): Path<String>) -> Response {
    if metalcraft_flows::delete_flow(&paths::flows_dir(), &id) {
        let _ = crate::lockfile::remove_flow(&id);
        // Both outlive the flow file otherwise: a later flow reusing the id would
        // silently inherit somebody else's agent, and an orphaned schedule would
        // make the daemon log a missing-flow warning on every poll forever.
        let _ = crate::flow_bindings::forget(&id);
        let dropped = crate::scheduled_flows::forget_flow(&id);
        if dropped > 0 {
            log::info!("Deleted flow '{id}' and its {dropped} schedule(s)");
        }
        StatusCode::NO_CONTENT.into_response()
    } else {
        err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found"))
    }
}

// ---- Scheduled flows -------------------------------------------------------
//
// *When* a flow runs, as its own resource. One document per schedule, so creating
// one is arming and deleting one is disarming — there is no separate "is it on"
// flag on the flow that could disagree with what is here.

/// What a [`metalcraft_flows::ScheduledFlow`] looks like on the wire, for the
/// OpenAPI document only.
///
/// **Nothing constructs one.** It exists because `ScheduledFlow` lives in the
/// `metalcraft-flows` crate and does not derive `ToSchema`, so the only way to
/// describe it here was `value_type = Object` — and an `Object` is not a gap in
/// the generated client, it is a hole that swallows its neighbours. utoipa emits
/// `Record<string, never>` for it, and TypeScript intersects that index signature
/// with the fields beside it: `id: string` becomes `string & never`. Every client
/// that generates from this document lost `id`, `flow_id`, `enabled` and
/// `schedule` — the whole stored half of every scheduled-flow response, including
/// the id needed to address one.
///
/// A mirror rather than a `ToSchema` derive on the real type because the real type
/// is in a published crate, and describing our own API should not require cutting
/// a release of somebody else's. The cost is that a mirror can drift from what it
/// mirrors, so it does not get to: `scheduled_flow_schema_matches_the_artifact`
/// serializes a real `ScheduledFlow` and fails if the two disagree about field
/// names. When `metalcraft-flows` next derives `ToSchema`, delete this and point
/// `value_type` at the real thing.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
struct ScheduledFlowDoc {
    /// Stable identifier, opaque by convention (`sf_<random>`).
    id: String,
    /// The flow this runs, by its `id`. May dangle: the flow can be deleted out
    /// from under it, which is why `flow_name` is optional on the row.
    flow_id: String,
    /// Whether it fires. The only switch there is — the flow no longer knows it is
    /// scheduled, so there is no flow-level master switch to disagree with.
    enabled: bool,
    /// The trigger and its per-schedule overrides.
    schedule: ScheduleSpecDoc,
    /// The agent it runs as, so successive firings accumulate memory instead of
    /// waking up amnesiac.
    #[schema(nullable)]
    instance_id: Option<String>,
    /// The author suggestion this was created from. Provenance, not identity.
    #[schema(nullable)]
    from_suggestion: Option<String>,
    /// RFC-3339 creation timestamp.
    created_at: String,
    /// RFC-3339 last-modified timestamp.
    updated_at: String,
}

/// Wire shape of [`metalcraft_flows::ScheduleSpec`], for the document only. See
/// [`ScheduledFlowDoc`] — same reason, same deal with drift.
///
/// The trigger is flattened onto the object and tagged by `type`, so a cron
/// schedule is `{ "type": "cron", "cron": "0 8 * * *" }` rather than carrying a
/// nested trigger object.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
struct ScheduleSpecDoc {
    /// `manual` | `minutes` | `hours` | `cron`.
    #[serde(rename = "type")]
    r#type: String,
    /// Present for `minutes` and `hours`. Must be positive.
    #[schema(nullable)]
    interval: Option<u32>,
    /// Present for `cron`. A standard cron expression; the pod parses it, the
    /// spec crate does not.
    #[schema(nullable)]
    cron: Option<String>,
    /// Human-readable label ("Morning brief") — what a UI shows, and what the pod
    /// names a minted agent after.
    #[schema(nullable)]
    name: Option<String>,
    /// IANA timezone the `cron` trigger is evaluated in. Ignored by other
    /// triggers, and `null` means the pod's own time.
    #[schema(nullable)]
    timezone: Option<String>,
    /// Inputs handed to the flow when this fires, so one flow can run with
    /// different parameters on different schedules.
    #[schema(nullable)]
    inputs: Option<serde_json::Value>,
    /// Persona override for runs this schedule starts.
    #[schema(nullable)]
    persona: Option<String>,
}

/// One scheduled flow, resolved for display: the stored document plus the three
/// things a client would otherwise have to compute or fetch.
#[derive(Serialize, utoipa::ToSchema)]
struct ScheduledFlowRow {
    /// The artifact exactly as stored, so an editor can round-trip it without a
    /// second representation of the same thing.
    #[serde(flatten)]
    #[schema(value_type = ScheduledFlowDoc)]
    scheduled: metalcraft_flows::ScheduledFlow,
    /// Name of the flow it runs. **Absent when that flow no longer exists** — a
    /// schedule that can never fire, which is worth showing as broken rather than
    /// quietly listing as fine.
    #[serde(skip_serializing_if = "Option::is_none")]
    flow_name: Option<String>,
    /// Name of the agent it runs as. Absent if the instance was deleted out from
    /// under it.
    #[serde(skip_serializing_if = "Option::is_none")]
    instance_name: Option<String>,
    /// Human-readable trigger: ``"Cron `0 0 8 * * *` (America/Detroit)"``.
    description: String,
    /// Next projected fire time, or absent for a manual trigger — and for a cron
    /// this pod cannot parse, which is how a schedule that will never fire looks.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_fire_at: Option<String>,
}

fn scheduled_row(sf: metalcraft_flows::ScheduledFlow) -> ScheduledFlowRow {
    let preview = crate::scheduled_flows::preview(&sf.schedule);
    ScheduledFlowRow {
        flow_name: metalcraft_flows::load_flow(&paths::flows_dir(), &sf.flow_id).map(|f| f.name),
        instance_name: sf
            .instance_id
            .as_deref()
            .and_then(|id| crate::agent_instance::load(id).ok())
            .map(|i| i.name),
        description: preview.description,
        // `preview` projects three; a row wants the next one.
        next_fire_at: preview.next_runs.into_iter().next(),
        scheduled: sf,
    }
}

#[derive(Serialize, utoipa::ToSchema)]
struct ScheduledFlowList {
    scheduled: Vec<ScheduledFlowRow>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
struct ScheduledFlowQuery {
    /// Only schedules of this flow.
    #[serde(default)]
    flow_id: Option<String>,
    /// Only schedules armed to this agent.
    #[serde(default)]
    instance_id: Option<String>,
}

/// `GET /api/v1/scheduled-flows` — everything this pod will do on its own.
///
/// The complete answer: nothing else fires a flow on a timer. An empty list means
/// this pod does nothing unless somebody asks it to.
#[utoipa::path(
    get,
    path = "/api/v1/scheduled-flows",
    tag = "flows",
    params(ScheduledFlowQuery),
    responses((status = 200, description = "Scheduled flows on this pod", body = ScheduledFlowList)),
)]
async fn list_scheduled_flows(Query(q): Query<ScheduledFlowQuery>) -> Response {
    let scheduled: Vec<ScheduledFlowRow> = crate::scheduled_flows::list()
        .into_iter()
        .filter(|sf| q.flow_id.as_deref().is_none_or(|f| sf.flow_id == f))
        .filter(|sf| {
            q.instance_id
                .as_deref()
                .is_none_or(|i| sf.instance_id.as_deref() == Some(i))
        })
        .map(scheduled_row)
        .collect();
    Json(ScheduledFlowList { scheduled }).into_response()
}

#[utoipa::path(
    get,
    path = "/api/v1/scheduled-flows/{id}",
    tag = "flows",
    params(("id" = String, Path, description = "Scheduled flow id")),
    responses((status = 200, body = ScheduledFlowRow), (status = 404, body = ErrorResponse)),
)]
async fn get_scheduled_flow(Path(id): Path<String>) -> Response {
    match crate::scheduled_flows::get(&id) {
        Some(sf) => Json(scheduled_row(sf)).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("no scheduled flow '{id}'")),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct CreateScheduledFlowRequest {
    /// The flow to run.
    flow_id: String,
    /// When to run it: `{ "type": "cron", "cron": "…", "timezone": "…" }`.
    #[schema(value_type = ScheduleSpecDoc)]
    schedule: metalcraft_flows::ScheduleSpec,
    /// Start firing immediately. Defaults to `true` — creating a schedule is the
    /// act of asking for it; a client staging one passes `false`.
    #[serde(default = "default_true")]
    enabled: bool,
    /// Attach to an existing agent instead of minting one — e.g. run the briefer as
    /// the same agent you chat with.
    #[serde(default)]
    instance_id: Option<String>,
    /// The author's suggestion key, when the person accepted a suggested schedule
    /// from a pack or the registry.
    #[serde(default)]
    from_suggestion: Option<String>,
    /// A hand-chosen id instead of a generated one. Rejected on collision: a create
    /// must never overwrite an existing schedule.
    #[serde(default)]
    id: Option<String>,
}

fn default_true() -> bool {
    true
}

/// `POST /api/v1/scheduled-flows` — arm a flow.
///
/// This is the consent point: it creates the schedule **and** the agent that will
/// run it, since "run this while nobody is watching" is one decision rather than
/// two. Schedules of one flow share an agent unless `instance_id` says
/// otherwise, so the evening run remembers the morning one.
#[utoipa::path(
    post,
    path = "/api/v1/scheduled-flows",
    tag = "flows",
    request_body = CreateScheduledFlowRequest,
    responses(
        (status = 201, description = "Armed", body = ScheduledFlowRow),
        (status = 400, description = "Bad trigger, or a persona outside the flow's roster", body = ErrorResponse),
        (status = 404, description = "No such flow", body = ErrorResponse),
    ),
)]
async fn post_scheduled_flow(Json(req): Json<CreateScheduledFlowRequest>) -> Response {
    let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), &req.flow_id) else {
        return err_json(
            StatusCode::NOT_FOUND,
            format!("flow '{}' not found", req.flow_id),
        );
    };
    match crate::scheduled_flows::arm(crate::scheduled_flows::NewSchedule {
        flow: &flow,
        schedule: req.schedule,
        enabled: req.enabled,
        instance: req.instance_id.as_deref(),
        from_suggestion: req.from_suggestion,
        id: req.id,
    }) {
        Ok(sf) => (StatusCode::CREATED, Json(scheduled_row(sf))).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct UpdateScheduledFlowRequest {
    /// Replace the trigger and its overrides.
    #[serde(default)]
    #[schema(value_type = Option<ScheduleSpecDoc>)]
    schedule: Option<metalcraft_flows::ScheduleSpec>,
    /// Pause (`false`) or resume (`true`) without deleting. The agent and its
    /// memory are untouched either way.
    #[serde(default)]
    enabled: Option<bool>,
    /// Move this schedule to a different agent.
    #[serde(default)]
    instance_id: Option<String>,
}

#[utoipa::path(
    put,
    path = "/api/v1/scheduled-flows/{id}",
    tag = "flows",
    params(("id" = String, Path, description = "Scheduled flow id")),
    request_body = UpdateScheduledFlowRequest,
    responses(
        (status = 200, body = ScheduledFlowRow),
        (status = 400, body = ErrorResponse),
        (status = 404, body = ErrorResponse),
    ),
)]
async fn put_scheduled_flow(
    Path(id): Path<String>,
    Json(req): Json<UpdateScheduledFlowRequest>,
) -> Response {
    let Some(mut sf) = crate::scheduled_flows::get(&id) else {
        return err_json(StatusCode::NOT_FOUND, format!("no scheduled flow '{id}'"));
    };
    if let Some(schedule) = req.schedule {
        // A schedule may override the persona, and the containment rule has to hold
        // for an edit exactly as it does for a create — otherwise arming safely and
        // then editing into a persona outside the roster is a way around it.
        if let Some(persona) = schedule.persona.as_deref() {
            let preset_slug = crate::flow_bindings::preset_for(&sf.flow_id);
            match crate::agent_preset::AgentPreset::load(&preset_slug, &paths::agent_presets_dir())
            {
                Ok(preset) if !preset.allows_persona(persona) => {
                    return err_json(
                        StatusCode::BAD_REQUEST,
                        format!(
                            "schedule names persona '{persona}', which is not in agent '{}'",
                            preset.slug
                        ),
                    );
                }
                _ => {}
            }
        }
        sf.schedule = schedule;
    }
    if let Some(enabled) = req.enabled {
        sf.enabled = enabled;
    }
    if let Some(instance_id) = req.instance_id {
        // Checked, not created: moving a schedule onto an agent that does not
        // exist would leave it firing into nothing every morning, silently.
        if crate::agent_instance::load(&instance_id).is_err() {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!("no agent '{instance_id}'"),
            );
        }
        sf.instance_id = Some(instance_id);
    }
    sf.updated_at = chrono::Utc::now().to_rfc3339();
    match crate::scheduled_flows::save(&sf) {
        Ok(()) => Json(scheduled_row(sf)).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

/// `DELETE /api/v1/scheduled-flows/{id}` — disarm.
///
/// **The agent and everything it remembers are kept.** Disarming is "stop running
/// this on a timer", not "destroy the thing that was running it", and a client
/// should not imply otherwise.
#[utoipa::path(
    delete,
    path = "/api/v1/scheduled-flows/{id}",
    tag = "flows",
    params(("id" = String, Path, description = "Scheduled flow id")),
    responses(
        (status = 204, description = "Disarmed; the agent and its memory are kept"),
        (status = 404, body = ErrorResponse),
    ),
)]
async fn delete_scheduled_flow(Path(id): Path<String>) -> Response {
    match crate::scheduled_flows::disarm(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct PreviewScheduleRequest {
    /// The trigger to project. Need not correspond to anything saved.
    #[schema(value_type = ScheduleSpecDoc)]
    schedule: metalcraft_flows::ScheduleSpec,
}

/// `POST /api/v1/scheduled-flows/preview` — when *would* this fire?
///
/// Takes an unsaved spec so an editor can answer the question before committing to
/// it. An empty `next_runs` on a cron trigger means this pod cannot parse it — the
/// thing a person most needs to know before saving.
#[utoipa::path(
    post,
    path = "/api/v1/scheduled-flows/preview",
    tag = "flows",
    request_body = PreviewScheduleRequest,
    responses((status = 200, body = crate::scheduled_flows::SchedulePreview)),
)]
async fn post_schedule_preview(Json(req): Json<PreviewScheduleRequest>) -> Response {
    Json(crate::scheduled_flows::preview(&req.schedule)).into_response()
}

// ---- Flow ↔ preset binding -------------------------------------------------
//
// Which preset a flow runs as — the roster its personas must come from. Which
// *instance* each schedule fires into lives on the schedule itself.
// See `docs/FLOWS_AND_AGENT_PRESETS_PLAN.md`.

/// The binding for one flow, resolved for display.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct FlowBindingView {
    flow_id: String,
    /// The preset the flow runs as. Always populated — an unbound flow resolves to
    /// the default agent, which is what it effectively already was.
    preset: String,
    /// True when the preset was chosen deliberately rather than defaulted.
    bound: bool,
    /// Personas the flow names, and whether the preset can reach each one.
    personas: Vec<FlowPersonaCheck>,
    /// The schedules of this flow that are armed to an agent.
    armed: Vec<ArmedSchedule>,
    /// Everything the arm dialog needs to state what arming this actually permits.
    ///
    /// Arming is the second consent moment, and the sharper one: a scheduled flow
    /// acts **while nobody is watching**, so a mutating tool inside one is a bigger
    /// commitment than the same tool in a chat where an approval prompt exists.
    consent: ArmConsent,
}

/// The resolved content of the arm dialog.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ArmConsent {
    /// Display name of the preset this flow runs as.
    preset_name: String,
    /// Origins its tools can reach.
    domains: Vec<String>,
    /// Credentials it will use.
    requires_env: Vec<String>,
    /// Credentials the pod does not have — those tools will fail at 3am rather than
    /// at a moment anyone is looking.
    missing_env: Vec<String>,
    /// Tools that can change something on the other end.
    mutating_tools: Vec<String>,
    tool_count: usize,
    /// Seed memories the agent starts from. It accumulates more on every run.
    base_memories: usize,
}

fn arm_consent(preset: Option<&crate::agent_preset::AgentPreset>) -> ArmConsent {
    let Some(preset) = preset else {
        return ArmConsent {
            preset_name: String::new(),
            domains: Vec::new(),
            requires_env: Vec::new(),
            missing_env: Vec::new(),
            mutating_tools: Vec::new(),
            tool_count: 0,
            base_memories: 0,
        };
    };
    let consent = crate::agent_packs::consent_for_preset(preset);
    let requires_env: Vec<String> = consent
        .requires_env
        .iter()
        .map(|e| e.name.clone())
        .collect();
    // The pack-derived summary covers what *integrations* vend. An agent's
    // personas also carry built-in tools — `bash`, `edit_file`, `web_fetch` — and
    // those are the ones you would most want named before agreeing to let it run
    // unwatched. Without this, a seeded preset (which has no integrations at all)
    // reported "0 tools" for an agent that can execute shell commands: a consent
    // summary that is not merely thin but wrong.
    let mut tools: std::collections::BTreeSet<String> = consent.tools.iter().cloned().collect();
    // The delegation roster, not just the declared one: a preset that delegates to
    // any installed persona can reach their tools too, and a consent summary that
    // omitted them would understate exactly the thing it exists to disclose.
    for slug in preset.delegation_roster(&paths::personas_dir()) {
        if let Ok(persona) = Persona::load(&slug, &paths::personas_dir()) {
            tools.extend(persona.resolved_tool_names());
        }
    }
    let mut mutating: Vec<String> = consent.mutating_tools.clone();
    for name in &tools {
        if changes_something(name) && !mutating.contains(name) {
            mutating.push(name.clone());
        }
    }
    mutating.sort();

    ArmConsent {
        preset_name: preset.name.clone(),
        missing_env: requires_env
            .iter()
            .filter(|n| crate::key_store::lookup(n).is_none())
            .cloned()
            .collect(),
        requires_env,
        domains: consent.domains,
        mutating_tools: mutating,
        tool_count: tools.len(),
        base_memories: crate::memory::instance::current_base_version(&preset.slug)
            .and_then(|v| crate::memory::instance::load_base(&preset.slug, &v).ok())
            .and_then(|b| b.try_read().map(|b| b.len()).ok())
            .unwrap_or(0),
    }
}

/// Whether a tool can change something, for the purpose of *describing* an agent
/// before it runs.
///
/// Deliberately not `OperationKind::default_permission()`, which answers a
/// different question — "would this prompt?" — and auto-approves `write_file`
/// when the path does not exist yet. Creating a file is still an effect, and a
/// summary of what an unwatched agent may do should say so. So: anything that is
/// not a read.
fn changes_something(tool_name: &str) -> bool {
    use crate::approval::OperationKind as K;
    !matches!(
        K::classify(tool_name, &serde_json::Value::Null),
        K::ReadFile | K::ListFiles | K::Search | K::LoadSkill | K::MetaRead
    )
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct FlowPersonaCheck {
    slug: String,
    allowed: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ArmedSchedule {
    /// The scheduled flow's id.
    schedule_id: String,
    /// Its label, so the arm dialog can say *which* schedule without showing an
    /// opaque id.
    name: String,
    instance_id: String,
    /// Absent if the instance was deleted out from under it.
    #[serde(skip_serializing_if = "Option::is_none")]
    instance_name: Option<String>,
}

fn binding_view(flow: &metalcraft_flows::SavedFlow) -> FlowBindingView {
    let raw = crate::flow_bindings::get(&flow.id);
    let preset_slug = crate::flow_bindings::preset_for(&flow.id);
    let preset =
        crate::agent_preset::AgentPreset::load(&preset_slug, &paths::agent_presets_dir()).ok();
    FlowBindingView {
        flow_id: flow.id.clone(),
        preset: preset_slug,
        bound: raw.preset.is_some(),
        personas: crate::flow_bindings::personas_named(flow)
            .into_iter()
            .map(|slug| FlowPersonaCheck {
                allowed: preset.as_ref().is_none_or(|p| p.allows_persona(&slug)),
                slug,
            })
            .collect(),
        armed: crate::scheduled_flows::for_flow(&flow.id)
            .into_iter()
            .filter_map(|sf| {
                let instance_id = sf.instance_id?;
                Some(ArmedSchedule {
                    instance_name: crate::agent_instance::load(&instance_id)
                        .ok()
                        .map(|i| i.name),
                    name: sf.schedule.display_name(),
                    schedule_id: sf.id,
                    instance_id,
                })
            })
            .collect(),
        consent: arm_consent(preset.as_ref()),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/flows/{id}/binding",
    tag = "flows",
    params(("id" = String, Path, description = "Flow id")),
    responses((status = 200, body = FlowBindingView), (status = 404, body = ErrorResponse)),
)]
async fn get_flow_binding(Path(id): Path<String>) -> Response {
    match metalcraft_flows::load_flow(&paths::flows_dir(), &id) {
        Some(flow) => Json(binding_view(&flow)).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found")),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct BindFlowRequest {
    /// The preset slug. `null` clears the binding back to the default agent.
    #[serde(default)]
    preset: Option<String>,
}

#[utoipa::path(
    put,
    path = "/api/v1/flows/{id}/binding",
    tag = "flows",
    params(("id" = String, Path, description = "Flow id")),
    request_body = BindFlowRequest,
    responses(
        (status = 200, body = FlowBindingView),
        (status = 400, description = "Flow names personas outside the roster", body = ErrorResponse),
        (status = 404, body = ErrorResponse),
    ),
)]
async fn put_flow_binding(Path(id): Path<String>, Json(req): Json<BindFlowRequest>) -> Response {
    let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), &id) else {
        return err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found"));
    };
    let result = match req.preset.as_deref() {
        Some(slug) => crate::flow_bindings::bind_preset(&flow, slug),
        None => crate::flow_bindings::unbind(&id),
    };
    match result {
        Ok(()) => Json(binding_view(&flow)).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct InstallFlowRequest {
    /// Registry slug of the flow to install (equals the flow id).
    slug: String,
}

/// Install a registry flow onto this agent: download its `SavedFlow` JSON from
/// flows.metalcraftai.com, validate it, and save it into the flows dir (installed
/// disabled). Returns `{ flow, dependencies }` — the dependency report lists any
/// packs/personas the flow needs that aren't installed yet.
#[utoipa::path(
    post,
    path = "/api/v1/flows/install",
    tag = "flows",
    request_body = InstallFlowRequest,
    responses(
        (status = 200, description = "Installed flow + dependency report", body = crate::flow_install::InstallResult),
        (status = 400, body = ErrorResponse),
        (status = 502, body = ErrorResponse),
    ),
)]
async fn post_install_flow(Json(req): Json<InstallFlowRequest>) -> Response {
    match crate::flow_install::install_flow_from_registry(&req.slug).await {
        Ok(result) => {
            // Pin the installed flow in the lockfile (version + content hash from the
            // registry) so a rebuilt/cloned pod reinstalls the same document. Best-effort.
            if let Ok((version, Some(hash))) = crate::registry::flow_version(&req.slug).await {
                let _ = crate::lockfile::record_flow(
                    &req.slug,
                    &version,
                    &hash,
                    &crate::registry::flows_base_url(),
                );
            }
            Json(result).into_response()
        }
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

/// Report whether this pod satisfies the integrations an already-installed flow
/// declares in its `requires` block. One outcome per pack.
///
/// It reports rather than installs. Fetching a pack from here was a second install
/// path into a second layout; an integration reaches a pod inside an agent pack the
/// operator installs, through `POST /api/v1/agent-packs/install` and nowhere else.
#[utoipa::path(
    post,
    path = "/api/v1/flows/{id}/check-dependencies",
    tag = "flows",
    params(("id" = String, Path, description = "Flow id")),
    responses(
        (status = 200, description = "Per-pack outcomes", body = InstallDependenciesResponse),
        (status = 404, body = ErrorResponse),
    ),
)]
async fn post_check_flow_dependencies(Path(id): Path<String>) -> Response {
    let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), &id) else {
        return err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found"));
    };
    let outcomes = crate::flow_install::check_flow_dependencies(&flow);
    Json(InstallDependenciesResponse {
        flow: id,
        packs: outcomes,
    })
    .into_response()
}

/// Response for `POST /flows/{id}/check-dependencies`.
#[derive(Serialize, utoipa::ToSchema)]
struct InstallDependenciesResponse {
    /// The flow whose dependencies were checked.
    flow: String,
    /// One outcome per required pack.
    packs: Vec<crate::flow_install::PackInstallOutcome>,
}

// ── Diagnostics handlers ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/v1/diagnostics",
    tag = "diagnostics",
    responses((status = 200, body = Vec<crate::diagnostics_browse::DiagnosticsSessionSummary>)),
)]
async fn list_diagnostics() -> Json<Vec<DiagnosticsSessionSummary>> {
    Json(list_diagnostics_sessions())
}

#[utoipa::path(
    get,
    path = "/api/v1/diagnostics/{id}",
    tag = "diagnostics",
    params(("id" = String, Path, description = "Session id")),
    responses((status = 200, body = crate::diagnostics_browse::DiagnosticsSession), (status = 404, body = ErrorResponse)),
)]
async fn get_diagnostics_session(Path(id): Path<String>) -> Response {
    match read_diagnostics_session(&id) {
        Some(session) => Json(session).into_response(),
        None => err_json(
            StatusCode::NOT_FOUND,
            format!("diagnostics session '{id}' not found"),
        ),
    }
}

/// The OTLP trace for one session — where a turn's time actually went.
///
/// Deliberately separate from `GET /diagnostics/{id}`: that response already
/// carries every message of every turn, and a trace is read for timings, often
/// while the session's transcript is already on screen. Bundling them would make
/// the common read pay for the large one.
///
/// `404` covers both "no such session" and "that session has no trace" — a run
/// from before tracing existed reads the same way to a client, which has nothing
/// to show either way.
#[utoipa::path(
    get,
    path = "/api/v1/diagnostics/{id}/trace",
    tag = "diagnostics",
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "OTLP/JSON trace document (OpenTelemetry GenAI conventions)"),
        (status = 404, body = ErrorResponse),
    ),
)]
async fn get_diagnostics_trace(Path(id): Path<String>) -> Response {
    match crate::diagnostics_browse::read_diagnostics_trace(&id) {
        Some(trace) => Json(trace).into_response(),
        None => err_json(
            StatusCode::NOT_FOUND,
            format!("no trace for diagnostics session '{id}'"),
        ),
    }
}

// ── API Tool handlers ───────────────────────────────────────────────────

fn list_api_tool_summaries() -> Vec<ApiToolSummary> {
    let layered =
        crate::integrations::list_files_layered(&paths::api_tools_dir(), "api_tools", "json");
    let mut summaries: Vec<ApiToolSummary> = layered
        .into_iter()
        .filter_map(|(path, origin)| {
            let content = std::fs::read_to_string(&path).ok()?;
            let config: HttpApiToolConfig = serde_json::from_str(&content).ok()?;
            Some(ApiToolSummary {
                name: config.name,
                description: config.description,
                pack_id: origin.pack_id().map(String::from),
                read_only: origin.is_read_only(),
            })
        })
        .collect();
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    summaries
}

#[utoipa::path(
    get,
    path = "/api/v1/api-tools",
    tag = "api-tools",
    responses((status = 200, body = Vec<ApiToolSummary>)),
)]
async fn list_api_tools() -> Json<Vec<ApiToolSummary>> {
    Json(list_api_tool_summaries())
}

#[utoipa::path(
    get,
    path = "/api/v1/api-tools/{name}",
    tag = "api-tools",
    params(("name" = String, Path, description = "Tool name")),
    responses((status = 200, body = crate::tools::http_api::HttpApiToolConfig), (status = 404, body = ErrorResponse)),
)]
async fn get_api_tool(Path(name): Path<String>) -> Response {
    let filename = format!("{name}.json");
    let Some((path, _)) =
        crate::integrations::resolve_file(&paths::api_tools_dir(), "api_tools", &filename)
    else {
        return err_json(
            StatusCode::NOT_FOUND,
            format!("api-tool '{name}' not found"),
        );
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<HttpApiToolConfig>(&content) {
            Ok(config) => Json(config).into_response(),
            Err(e) => err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to parse: {e}"),
            ),
        },
        Err(_) => err_json(
            StatusCode::NOT_FOUND,
            format!("api-tool '{name}' not found"),
        ),
    }
}

#[utoipa::path(
    put,
    path = "/api/v1/api-tools/{name}",
    tag = "api-tools",
    params(("name" = String, Path, description = "Tool name")),
    request_body = crate::tools::http_api::HttpApiToolConfig,
    responses((status = 200, description = "Saved")),
)]
async fn put_api_tool(
    Path(name): Path<String>,
    Json(mut config): Json<HttpApiToolConfig>,
) -> Response {
    config.name = name.clone();
    let filename = format!("{name}.json");
    let local_exists = paths::api_tools_dir().join(&filename).exists();
    if !local_exists {
        if let Some((_, origin)) =
            crate::integrations::resolve_file(&paths::api_tools_dir(), "api_tools", &filename)
        {
            if let Some(pack_id) = origin.pack_id() {
                return err_json(
                    StatusCode::CONFLICT,
                    format!(
                        "api-tool '{name}' is provided by the '{pack_id}' integration and is read-only. Choose a different name."
                    ),
                );
            }
        }
    }
    let path = paths::api_tools_dir().join(&filename);
    match serde_json::to_string_pretty(&config) {
        Ok(content) => match std::fs::write(&path, content) {
            Ok(()) => Json(config).into_response(),
            Err(e) => err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write: {e}"),
            ),
        },
        Err(e) => err_json(StatusCode::BAD_REQUEST, format!("Failed to serialize: {e}")),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/api-tools/{name}",
    tag = "api-tools",
    params(("name" = String, Path, description = "Tool name")),
    responses((status = 200, description = "Deleted")),
)]
async fn delete_api_tool(Path(name): Path<String>) -> Response {
    let path = paths::api_tools_dir().join(format!("{name}.json"));
    if !path.exists() {
        return err_json(
            StatusCode::NOT_FOUND,
            format!("api-tool '{name}' is not a user-local api-tool"),
        );
    }
    match std::fs::remove_file(&path) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete: {e}"),
        ),
    }
}

// ── API key store handlers ──────────────────────────────────────────────
//
// The key store (`<data>/keys.json`) holds the secrets that HTTP-API tools
// reference via `$NAME`. The workshop manages them here; raw values only ever
// flow inward (on PUT) — list/get responses are always masked.

#[derive(Deserialize, utoipa::ToSchema)]
struct KeyValueBody {
    value: String,
    /// When set, the key is written to this channel's secret scope instead of
    /// the global namespace.
    #[serde(default)]
    channel_id: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
struct KeyScopeQuery {
    /// When set, target this channel's secret scope instead of global.
    #[serde(default)]
    channel_id: Option<String>,
}

/// Global-scope masked keys — for the load-time snapshot and the sidebar.
fn list_key_summaries() -> Vec<KeySummary> {
    crate::key_store::KeyStore::load(&paths::keys_file())
        .list_masked()
        .into_iter()
        .map(|(name, masked)| KeySummary { name, masked })
        .collect()
}

/// Whether a channel's secrets are managed by its connection (the built-in
/// `metalcraft` channel) — such secrets are read-only in the UI.
fn is_channel_managed(channel_slug: &str) -> bool {
    crate::channels::get_channel(channel_slug)
        .map(|c| c.managed)
        .unwrap_or(false)
}

/// All stored keys with scope + managed flags, for the scope-aware Keys page.
fn list_key_entries() -> Vec<KeyEntry> {
    let store = crate::key_store::KeyStore::load(&paths::keys_file());
    store
        .list_scoped()
        .into_iter()
        .map(|(scope, name, masked)| match scope {
            crate::key_store::KeyScope::Global => KeyEntry {
                managed: crate::key_store::is_env_authoritative(&name),
                name,
                masked,
                scope: "global".into(),
                channel_id: None,
                channel_name: None,
            },
            crate::key_store::KeyScope::Channel(id) => {
                let ch = crate::channels::get_channel(&id);
                KeyEntry {
                    name,
                    masked,
                    scope: "channel".into(),
                    channel_name: ch.as_ref().map(|c| c.name.clone()),
                    managed: ch.map(|c| c.managed).unwrap_or(false),
                    channel_id: Some(id),
                }
            }
        })
        .collect()
}

#[utoipa::path(
    get,
    path = "/api/v1/keys",
    tag = "keys",
    responses((status = 200, body = Vec<KeyEntry>)),
)]
async fn list_keys() -> Json<Vec<KeyEntry>> {
    Json(list_key_entries())
}

/// Whether this pod can run a turn, and on whose credential.
///
/// The one question a client cannot answer from `GET /api/v1/keys`: that endpoint
/// lists `keys.json`, and a provisioned pod's credential is injected as container
/// env, so a healthy pod reads as an empty store. Clients that inferred "no key,
/// cannot think" from it told people their working pod was dead. The pod is the
/// only honest source, so it answers here — through the same function a turn uses.
#[derive(Serialize, utoipa::ToSchema)]
struct InferenceStatus {
    /// A credential resolves, so a turn has something to authenticate with. This
    /// is not a promise the turn *succeeds*: the gateway still meters credits and
    /// requires the account's premium, which the pod cannot see.
    ready: bool,
    /// Which credential answered — `"stored"` (bound through this API),
    /// `"environment"` (injected by provisioning, or a `.env`), `"pod_token"`
    /// (this pod's own identity, offered only at the gateway), or `"none"`.
    credential: String,
    /// Where inference is routed, userinfo and query stripped. Absent means the
    /// default, OpenAI proper.
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Routed at the Metalcraft gateway — so turns bill the account's credits.
    gateway: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/inference",
    tag = "keys",
    responses((status = 200, body = InferenceStatus)),
)]
async fn get_inference_status() -> Json<InferenceStatus> {
    let credential = crate::runtime::inference_credential();
    Json(InferenceStatus {
        ready: credential.is_some(),
        credential: credential
            .map(|(_, c)| c.as_str().to_string())
            .unwrap_or_else(|| "none".into()),
        base_url: crate::runtime::inference_base_url(),
        gateway: crate::runtime::inference_at_gateway(),
    })
}

/// Keys recommended by enabled packs, each flagged configured/missing. Lets the
/// key store UI surface "these enabled packs need these keys" without the user
/// having to read each pack's manifest.
#[utoipa::path(
    get,
    path = "/api/v1/keys/recommended",
    tag = "keys",
    responses((status = 200, body = Vec<RecommendedKey>)),
)]
async fn list_recommended_keys() -> Json<Vec<RecommendedKey>> {
    // Recommendations from enabled integrations. The `packs` field carries
    // the source label (pack id) so the key-store UI can show "who wants this".
    let mut merged: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (name, sources) in crate::integrations::recommended_env() {
        merged.entry(name).or_default().extend(sources);
    }
    let out = merged
        .into_iter()
        .map(|(name, packs)| RecommendedKey {
            configured: crate::key_store::lookup(&name).is_some(),
            managed: crate::key_store::is_env_authoritative(&name),
            name,
            packs,
        })
        .collect();
    Json(out)
}

/// Upsert a key. The name comes from the path; the raw value (and an optional
/// target `channel_id`) from the body. Managed keys — a platform-injected global
/// (env-authoritative) or a provisioner-backed channel's secrets — are read-only
/// and rejected here.
#[utoipa::path(
    put,
    path = "/api/v1/keys/{name}",
    tag = "keys",
    params(("name" = String, Path, description = "Key name")),
    request_body = KeyValueBody,
    responses((status = 200, description = "Saved")),
)]
async fn put_key(Path(name): Path<String>, Json(body): Json<KeyValueBody>) -> Response {
    if name.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "key name must not be empty");
    }
    if body.value.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "key value must not be empty");
    }
    let path = paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    let (scope, channel_id, channel_name) = match body.channel_id.as_deref() {
        Some(cid) => {
            if is_channel_managed(cid) {
                return err_json(
                    StatusCode::FORBIDDEN,
                    "this channel's secrets are managed by its connection and can't be edited here",
                );
            }
            store.upsert_channel(cid, &name, &body.value);
            let cname = crate::channels::get_channel(cid).map(|c| c.name);
            ("channel".to_string(), Some(cid.to_string()), cname)
        }
        None => {
            if crate::key_store::is_env_authoritative(&name) {
                return err_json(
                    StatusCode::FORBIDDEN,
                    "this key is provided by the platform and can't be edited",
                );
            }
            store.upsert(&name, &body.value);
            ("global".to_string(), None, None)
        }
    };
    match store.save(&path) {
        Ok(()) => Json(KeyEntry {
            masked: crate::key_store::mask(&body.value),
            name,
            scope,
            channel_id,
            channel_name,
            managed: false,
        })
        .into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write: {e}"),
        ),
    }
}

/// Reveal a stored key's **raw** value. An optional `?channel_id=` targets a
/// channel's secret scope; omitted means global. This is the one endpoint that
/// returns an unmasked value — an explicit, user-initiated action in the Keys UI.
/// Only returns values physically present in the store (a derived/env-only value
/// like a provisioner's `API_KEY` is not stored, so there's nothing to reveal).
#[utoipa::path(
    get,
    path = "/api/v1/keys/{name}/reveal",
    tag = "keys",
    params(("name" = String, Path, description = "Key name"), ("channel_id" = Option<String>, Query, description = "Channel scope for a channel secret")),
    responses((status = 200, body = KeyRevealResponse), (status = 404, body = ErrorResponse)),
)]
async fn reveal_key(Path(name): Path<String>, Query(q): Query<KeyScopeQuery>) -> Response {
    let store = crate::key_store::KeyStore::load(&paths::keys_file());
    let value = match q.channel_id.as_deref() {
        Some(cid) => store.get_channel(cid, &name).map(str::to_string),
        None => store.get(&name).map(str::to_string),
    };
    match value {
        Some(value) => Json(KeyRevealResponse { value }).into_response(),
        None => err_json(
            StatusCode::NOT_FOUND,
            format!("key '{name}' has no stored value to reveal"),
        ),
    }
}

/// Delete a key. An optional `?channel_id=` targets a channel's secret scope;
/// omitted means the global scope. Managed keys are read-only and rejected.
#[utoipa::path(
    delete,
    path = "/api/v1/keys/{name}",
    tag = "keys",
    params(("name" = String, Path, description = "Key name"), ("channel_id" = Option<String>, Query, description = "Channel scope for a channel secret")),
    responses((status = 200, description = "Deleted")),
)]
async fn delete_key(Path(name): Path<String>, Query(q): Query<KeyScopeQuery>) -> Response {
    let path = paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    let removed = match q.channel_id.as_deref() {
        Some(cid) => {
            if is_channel_managed(cid) {
                return err_json(
                    StatusCode::FORBIDDEN,
                    "this channel's secrets are managed by its connection and can't be deleted here",
                );
            }
            store.delete_channel_key(cid, &name)
        }
        None => {
            if crate::key_store::is_env_authoritative(&name) {
                return err_json(
                    StatusCode::FORBIDDEN,
                    "this key is provided by the platform and can't be deleted",
                );
            }
            store.delete(&name)
        }
    };
    if !removed {
        return err_json(StatusCode::NOT_FOUND, format!("key '{name}' not found"));
    }
    match store.save(&path) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write: {e}"),
        ),
    }
}

// ── Flow template handlers ──────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
struct FlowTemplateSummary {
    slug: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
struct FlowTemplate {
    slug: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
    /// The raw template flow JSON, ready to be cloned and edited as a new flow.
    flow: serde_json::Value,
}

fn list_flow_template_summaries() -> Vec<FlowTemplateSummary> {
    let layered = crate::integrations::list_files_layered(
        &paths::flow_templates_dir(),
        "flow_templates",
        "json",
    );
    let mut summaries: Vec<FlowTemplateSummary> = layered
        .into_iter()
        .filter_map(|(path, origin)| {
            let slug = path.file_stem()?.to_str()?.to_string();
            let content = std::fs::read_to_string(&path).ok()?;
            let value: serde_json::Value = serde_json::from_str(&content).ok()?;
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&slug)
                .to_string();
            Some(FlowTemplateSummary {
                slug,
                name,
                pack_id: origin.pack_id().map(String::from),
            })
        })
        .collect();
    summaries.sort_by(|a, b| a.slug.cmp(&b.slug));
    summaries
}

#[utoipa::path(
    get,
    path = "/api/v1/flow-templates",
    tag = "flows",
    responses((status = 200, body = Vec<FlowTemplateSummary>)),
)]
async fn list_flow_templates() -> Json<Vec<FlowTemplateSummary>> {
    Json(list_flow_template_summaries())
}

#[utoipa::path(
    get,
    path = "/api/v1/flow-templates/{slug}",
    tag = "flows",
    params(("slug" = String, Path, description = "Template slug")),
    responses((status = 200, body = FlowTemplate), (status = 404, body = ErrorResponse)),
)]
async fn get_flow_template(Path(slug): Path<String>) -> Response {
    let filename = format!("{slug}.json");
    let Some((path, origin)) = crate::integrations::resolve_file(
        &paths::flow_templates_dir(),
        "flow_templates",
        &filename,
    ) else {
        return err_json(
            StatusCode::NOT_FOUND,
            format!("template '{slug}' not found"),
        );
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return err_json(
                StatusCode::NOT_FOUND,
                format!("template '{slug}' not found"),
            );
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("parse error: {e}"),
            );
        }
    };
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(&slug)
        .to_string();
    Json(FlowTemplate {
        slug,
        name,
        pack_id: origin.pack_id().map(String::from),
        flow: value,
    })
    .into_response()
}

// ── Run flow handler ────────────────────────────────────────────────────

#[derive(Deserialize, Default, utoipa::ToSchema)]
struct RunFlowRequest {
    /// Optional persona override. A v2 flow owns its persona via the entry node,
    /// so this is normally omitted and left to the flow. It is only a fallback for
    /// a flow that declares none (and for legacy v1 flows). Prompt nodes that
    /// declare their own persona override it regardless.
    #[serde(default)]
    persona_slug: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
    /// Values for the flow's declared entry `inputs`. Object of `{ name: value }`;
    /// missing inputs fall back to their declared defaults. Ignored by v1 flows.
    #[serde(default)]
    inputs: Option<serde_json::Value>,
    /// Run as this agent, so the run recalls from and writes to its memory and
    /// leaves a conversation behind. Omitted resolves from the flow's armed
    /// schedules — see [`instance_for_manual_run`].
    #[serde(default)]
    instance_id: Option<String>,
}

/// Which agent a hand-triggered run should be.
///
/// Explicit wins. Otherwise the flow's armed agent, when there is exactly one:
/// pressing "run now" on an automation that fires every morning should be the
/// same act as the morning firing, not a stranger doing the same work with no
/// memory of it.
///
/// Several *different* agents (possible only when someone deliberately attached
/// schedules to separate ones) resolves to none, plus a warning naming them.
/// Picking one would silently write to a memory nobody chose; refusing the run
/// outright would break every caller that ran the flow before this existed. A
/// run that worked, said what it could not decide, and named the field that
/// decides it is the honest middle.
///
/// Returns `(instance, warning)`.
fn instance_for_manual_run(
    flow_id: &str,
    explicit: Option<&str>,
) -> (Option<String>, Option<String>) {
    if let Some(id) = explicit {
        return (Some(id.to_string()), None);
    }
    let mut armed: Vec<String> = crate::scheduled_flows::for_flow(flow_id)
        .into_iter()
        .filter_map(|sf| sf.instance_id)
        .collect();
    armed.sort();
    armed.dedup();
    match armed.len() {
        0 => (None, None),
        1 => (armed.into_iter().next(), None),
        _ => (
            None,
            Some(format!(
                "flow '{flow_id}' is armed to {} different agents ({}); ran without one.                  Pass `instance_id` to run as a specific agent.",
                armed.len(),
                armed.join(", ")
            )),
        ),
    }
}

#[derive(Serialize, utoipa::ToSchema)]
struct RunFlowResponse {
    flow_id: String,
    prompts: Vec<flows::FlowPromptResult>,
}

/// The run endpoint returns one of two shapes depending on the flow's spec
/// version: v2 (state-machine) flows return a [`FlowRunSummary`]; v1 (linear)
/// flows return a [`RunFlowResponse`]. Documented as a `oneOf` so generated
/// clients see a proper union rather than an opaque object. Never constructed —
/// it exists only to describe the response schema.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum RunFlowOutput {
    V2(crate::flow_exec::FlowRunSummary),
    V1(RunFlowResponse),
}

#[utoipa::path(
    post,
    path = "/api/v1/flows/{id}/run",
    tag = "flows",
    params(("id" = String, Path, description = "Flow id")),
    request_body = RunFlowRequest,
    responses((status = 200, description = "v2 flows return a FlowRunSummary; v1 flows return a RunFlowResponse", body = RunFlowOutput)),
)]
// A v2 run resolves an agent (§5 of `docs/FLOWS_AS_AGENTS_PLAN.md`) so a manual
// run of an armed automation is the same act as its scheduled firing: same
// memory, and a conversation you can read afterwards. A flow nobody armed still
// runs memoryless and leaves nothing behind — testing an unbound flow stays a
// test. v1 flows are unchanged; the legacy path has no instance to thread.
async fn post_run_flow(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    body: Option<Json<RunFlowRequest>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();

    let context = match AgentRuntimeContext::from_environment().map_err(|e| e.to_string()) {
        Ok(c) => c,
        Err(msg) => {
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("runtime not available: {msg}"),
            );
        }
    };
    // A v2 flow owns its persona (the entry node declares it); the request slug
    // is only an optional override, so don't manufacture a default here — that
    // would mislabel the run's session. v1 flows still fall back below.
    let persona_override = req.persona_slug.clone().filter(|s| !s.trim().is_empty());
    let model_name = req
        .model_name
        .unwrap_or_else(crate::runtime::configured_default_model);

    let Some(flow) = metalcraft_flows::load_flow(&crate::paths::flows_dir(), &id) else {
        return err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found"));
    };

    // v2 flows run on the stateful executor and return a run summary; v1 flows
    // keep the legacy per-prompt response.
    if crate::flow_exec::is_v2_flow(&flow) {
        let inputs = req.inputs.clone().unwrap_or_else(|| serde_json::json!({}));
        let (instance_id, ambiguous) =
            instance_for_manual_run(&flow.id, req.instance_id.as_deref());
        return match crate::flow_exec::run_flow_v2_as(
            &context,
            flow,
            &state.cwd,
            persona_override.as_deref(),
            &model_name,
            &inputs,
            instance_id,
        )
        .await
        {
            Ok(mut summary) => {
                summary.warnings.extend(ambiguous);
                Json(summary).into_response()
            }
            Err(e) => err_json(StatusCode::BAD_REQUEST, e),
        };
    }

    let persona_slug = persona_override.unwrap_or_else(crate::runtime::configured_default_persona);
    match flows::run_flow(&context, &id, &state.cwd, &persona_slug, &model_name).await {
        Ok(results) => Json(RunFlowResponse {
            flow_id: id,
            prompts: results,
        })
        .into_response(),
        // A missing flow yields 404; an unrunnable graph yields 400.
        Err(e) if e.contains("not found") => err_json(StatusCode::NOT_FOUND, e),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

// ── Flow-run (v2 pause/resume) handlers ─────────────────────────────────

/// List persisted flow runs, optionally filtered by `?flow_id=`.
#[utoipa::path(
    get,
    path = "/api/v1/flow-runs",
    tag = "flows",
    responses((status = 200, description = "Flow run summaries", body = Vec<crate::flow_runs::FlowRun>)),
)]
async fn list_flow_runs(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let mut runs = crate::flow_runs::list_runs(&paths::runs_dir());
    if let Some(f) = q.get("flow_id") {
        runs.retain(|r| &r.flow_id == f);
    }
    runs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(runs).into_response()
}

/// Get one flow run by id.
#[utoipa::path(
    get,
    path = "/api/v1/flow-runs/{run_id}",
    tag = "flows",
    params(("run_id" = String, Path, description = "Flow run id")),
    responses((status = 200, description = "Flow run detail", body = crate::flow_runs::FlowRun), (status = 404, body = ErrorResponse)),
)]
async fn get_flow_run(Path(run_id): Path<String>) -> Response {
    match crate::flow_runs::load_run(&paths::runs_dir(), &run_id) {
        Some(run) => Json(run).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("run '{run_id}' not found")),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct ResumeFlowRunRequest {
    /// Handle to take (an approval decision, or `"after"` for a wait).
    handle: String,
    /// Optional value to set as the resumed node's `_last` input.
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// Resume a paused flow run.
#[utoipa::path(
    post,
    path = "/api/v1/flow-runs/{run_id}/resume",
    tag = "flows",
    params(("run_id" = String, Path, description = "Flow run id")),
    request_body = ResumeFlowRunRequest,
    responses((status = 200, description = "Resumed run", body = crate::flow_exec::FlowRunSummary)),
)]
async fn post_resume_flow_run(
    Path(run_id): Path<String>,
    Json(req): Json<ResumeFlowRunRequest>,
) -> Response {
    let context = match AgentRuntimeContext::from_environment().map_err(|e| e.to_string()) {
        Ok(c) => c,
        Err(msg) => {
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("runtime not available: {msg}"),
            );
        }
    };
    match crate::flow_exec::resume_flow(&context, &run_id, &req.handle, req.data).await {
        Ok(summary) => Json(summary).into_response(),
        Err(e) if e.contains("not found") => err_json(StatusCode::NOT_FOUND, e),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

// ── Chat handlers ───────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
struct ChatSummary {
    id: String,
    /// The agent this conversation belongs to. `None` only on records written before
    /// instances existed, until the startup backfill reaches them.
    ///
    /// Exposed because a client cannot group conversations by agent without it —
    /// which is the whole shape of the chat list once agents exist, and the summary
    /// was the one place it was missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    persona_slug: String,
    model_name: String,
    created_at: String,
    /// How many times the user spoke. The unit a person counts a conversation in
    /// — not messages, which counts the agent's tool chatter, and not the
    /// transcript length, which now also counts reset dividers.
    turn_count: usize,
    /// Last activity, from the chat file's mtime — [`persist_chat`] rewrites it
    /// after every turn, so it is the clock the pod already trusts for staleness
    /// (see [`gateway_session_is_stale`]). `None` when the file cannot be read.
    ///
    /// A list sorted by `created_at` puts a conversation someone has been in all
    /// day below one they opened this morning and abandoned, which is the wrong
    /// way round for the question a session list answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    /// The start of the *last* thing said, trimmed to one line — what makes a
    /// row identifiable as *this* conversation rather than a timestamp beside an
    /// id, and what tells you where it got to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
}

/// How many times the user spoke in a transcript.
fn turns_in(messages: &[ChatMessageWire]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, ChatMessageWire::User { .. }))
        .count()
}

/// The start of the last thing said in a conversation, as a one-line label.
///
/// The *last* rather than the first: a row is read to answer "where did this
/// get to", and an opening line stops describing a conversation the moment it
/// moves on — every long-running thread ends up labelled by a question answered
/// hours ago, and the gateway and flow conversations that all open with the same
/// boilerplate become indistinguishable from each other.
///
/// Either speaker counts, because either can be the last word — a row that only
/// followed the user would show nothing until they replied, and the thing worth
/// seeing after an agent works alone for a while is what it came back with.
/// Tool calls, reasoning and reset dividers are skipped: they are machinery, not
/// something said.
fn preview_of(messages: &[ChatMessageWire]) -> Option<String> {
    messages.iter().rev().find_map(|m| match m {
        ChatMessageWire::User { content } | ChatMessageWire::Assistant { content } => {
            one_line(content)
        }
        _ => None,
    })
}

/// Collapse whitespace and cut to a row's worth of characters, or `None` when
/// there is nothing left to show.
fn one_line(text: &str) -> Option<String> {
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    const MAX: usize = 80;
    if line.chars().count() <= MAX {
        return Some(line);
    }
    Some(line.chars().take(MAX).collect::<String>() + "…")
}

/// When a chat was last written, RFC-3339.
fn chat_updated_at(id: &str) -> Option<String> {
    let modified = std::fs::metadata(chat_file_path(id)).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339())
}

impl ChatSummary {
    /// The one place a stored chat becomes a list row, so every endpoint that
    /// lists conversations describes them identically. They did not: one counted
    /// user turns and the other counted raw messages, so the same conversation
    /// showed two different sizes depending on which screen you opened.
    fn of(pc: PersistedChat) -> Self {
        Self {
            updated_at: chat_updated_at(&pc.id),
            preview: preview_of(&pc.messages),
            turn_count: turns_in(&pc.messages),
            id: pc.id,
            instance_id: pc.instance_id,
            persona_slug: pc.persona_slug,
            model_name: pc.model_name,
            created_at: pc.created_at,
        }
    }

    /// Newest activity first, falling back to creation for a chat with no file.
    fn recency(&self) -> &str {
        self.updated_at.as_deref().unwrap_or(&self.created_at)
    }
}

#[derive(Serialize, utoipa::ToSchema)]
struct ChatDetail {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    persona_slug: String,
    model_name: String,
    created_at: String,
    messages: Vec<ChatMessageWire>,
    /// The plan of the turn running right now, empty when none is. Lets a client
    /// that arrives mid-turn render the plan it missed the frames for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    plan: Vec<crate::turn_plan::PlanStep>,
}

/// Wire form for `metalcraft::AgentMessage` — the in-memory enum isn't
/// `Serialize`, so we convert before responding. Also used as the on-disk
/// format for persisted chats, so it derives `Deserialize` too.
#[derive(Serialize, Deserialize, Clone, utoipa::ToSchema)]
#[serde(tag = "role", rename_all = "snake_case")]
enum ChatMessageWire {
    User {
        content: String,
    },
    Assistant {
        content: String,
    },
    /// A reasoning item preserved so it can be replayed with its tool call on a
    /// later turn (Responses API requirement for reasoning models). Persisted so
    /// the pairing survives a reload — dropping it would re-trigger the "function
    /// call without required reasoning item" 400 on resume.
    Reasoning {
        id: String,
        encrypted: String,
    },
    ToolCall {
        id: String,
        #[serde(default)]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        id: String,
        #[serde(default)]
        call_id: Option<String>,
        name: String,
        result: String,
    },
    /// A conversation boundary: everything above it is history the model no
    /// longer sees.
    ///
    /// The only wire message with no `AgentMessage` counterpart, and that is the
    /// whole point of it. A reset ends a *context*, not a session — the transcript
    /// keeps everything and the context restarts here.
    ///
    /// Distinct from the other way a context gets cleared, which is a *new
    /// session* under the same agent (what a gateway conversation does after a
    /// quiet gap). That one leaves no divider because there is nothing to divide:
    /// it is a different conversation. This is for a clean slate inside the
    /// conversation you are already in. `reason` is short free text shown on the
    /// divider, so a transcript can answer "why does my history stop here".
    Reset {
        at: String,
        reason: String,
    },
}


impl ChatMessageWire {
    /// The model-facing form of this message, or `None` when it has none.
    ///
    /// Replaces a `From` impl on purpose: with [`ChatMessageWire::Reset`] in the
    /// enum the conversion stopped being total, and an infallible-looking `into()`
    /// would have had to invent a message for a divider.
    fn into_agent_message(self) -> Option<AgentMessage> {
        Some(match self {
            Self::User { content } => AgentMessage::User(content),
            Self::Assistant { content } => AgentMessage::Assistant(content),
            Self::Reasoning { id, encrypted } => AgentMessage::Reasoning { id, encrypted },
            Self::ToolCall {
                id,
                call_id,
                name,
                args,
            } => AgentMessage::ToolCall {
                id,
                call_id,
                name,
                args,
            },
            Self::ToolResult {
                id,
                call_id,
                name,
                result,
            } => AgentMessage::ToolResult {
                id,
                call_id,
                name,
                result,
            },
            Self::Reset { .. } => return None,
        })
    }
}

impl From<&AgentMessage> for ChatMessageWire {
    fn from(m: &AgentMessage) -> Self {
        match m {
            AgentMessage::User(s) => Self::User { content: s.clone() },
            AgentMessage::Assistant(s) => Self::Assistant { content: s.clone() },
            AgentMessage::Reasoning { id, encrypted } => Self::Reasoning {
                id: id.clone(),
                encrypted: encrypted.clone(),
            },
            AgentMessage::ToolCall {
                id,
                call_id,
                name,
                args,
            } => Self::ToolCall {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                args: args.clone(),
            },
            AgentMessage::ToolResult {
                id,
                call_id,
                name,
                result,
            } => Self::ToolResult {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                result: result.clone(),
            },
        }
    }
}

/// The whole conversation: the segments that have been closed off by a reset,
/// then whatever the live context holds now.
///
/// There is deliberately **no index** relating the two halves. Compaction
/// rewrites `state.messages` wholesale from inside a running turn
/// (`runtime::…::compact_if_needed`), so any cursor into it is stale the moment
/// a long conversation crosses the threshold — and the messages a stale cursor
/// drops are the newest ones. Concatenation cannot go wrong that way.
fn transcript_of(s: &ChatSession) -> Vec<ChatMessageWire> {
    let mut out = s.archived.clone();
    out.extend(live_messages(s));
    out
}

/// The live context's messages in wire form.
fn live_messages(s: &ChatSession) -> impl Iterator<Item = ChatMessageWire> + '_ {
    s.state
        .iter()
        .flat_map(|st| st.messages.iter().map(ChatMessageWire::from))
}

/// End this conversation's context without ending the conversation.
///
/// The messages the agent is about to stop seeing move into `archived` — where
/// nothing will rewrite them — and a divider goes in behind them. The model
/// starts the next turn from nothing.
fn mark_reset(s: &mut ChatSession, reason: &str) -> ChatMessageWire {
    let closing: Vec<ChatMessageWire> = live_messages(s).collect();
    s.archived.extend(closing);
    let mark = ChatMessageWire::Reset {
        at: chrono::Utc::now().to_rfc3339(),
        reason: reason.to_string(),
    };
    s.archived.push(mark.clone());
    s.state = None;
    mark
}

/// Rebuild a conversation's context from its transcript: the messages after the
/// last reset, with the dividers themselves dropped.
fn context_from_transcript(transcript: &[ChatMessageWire]) -> Option<AgentState> {
    let start = transcript
        .iter()
        .rposition(|m| matches!(m, ChatMessageWire::Reset { .. }))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut messages = transcript[start..]
        .iter()
        .cloned()
        .filter_map(ChatMessageWire::into_agent_message);
    let first = messages.next()?;
    // `AgentState::new` insists on an opening user message and keeps the rest of
    // its fields private, so seed with one and correct it when the first message
    // is something else (a transcript can start mid-turn after a reload).
    let mut st = match first {
        AgentMessage::User(content) => AgentState::new(content),
        other => {
            let mut s = AgentState::new("");
            s.messages.clear();
            s.messages.push(other);
            s
        }
    };
    st.messages.extend(messages);
    st.is_done = true; // turns are complete when persisted
    Some(st)
}

/// On-disk shape for a persisted chat. Mirrors [`ChatSession`] but flattens
/// the optional `AgentState` to a plain message vec so the file is human-
/// readable and tolerant of metalcraft API changes.
#[derive(Serialize, Deserialize)]
struct PersistedChat {
    id: String,
    /// Set from v0.30. Absent on legacy chats until `backfill_from_chats` runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    persona_slug: String,
    model_name: String,
    cwd: String,
    created_at: String,
    /// Session I/O type. Defaults to `Workshop` so chats written before this
    /// field existed still load.
    #[serde(default)]
    preset: SessionPreset,
    #[serde(default)]
    messages: Vec<ChatMessageWire>,
}

fn chat_file_path(id: &str) -> std::path::PathBuf {
    paths::chats_dir().join(format!("{id}.json"))
}

/// Snapshot the session and write it to disk. Holds the mutex briefly to
/// collect data, drops it before the (synchronous) write.
///
/// The file holds the whole conversation — archived segments and live context
/// both — so what is on disk is what a client renders, and the split is an
/// in-memory detail of which half a model is still being shown.
async fn persist_chat(session: &Arc<Mutex<ChatSession>>) {
    let snapshot = {
        let s = session.lock().await;
        PersistedChat {
            id: s.id.clone(),
            instance_id: s.instance_id.clone(),
            persona_slug: s.persona_slug.clone(),
            model_name: s.model_name.clone(),
            cwd: s.cwd.clone(),
            created_at: s.created_at.clone(),
            preset: s.preset.clone(),
            messages: transcript_of(&s),
        }
    };
    let path = chat_file_path(&snapshot.id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&snapshot) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log::warn!("failed to persist chat {}: {e}", snapshot.id);
            }
        }
        Err(e) => log::warn!("failed to serialize chat {}: {e}", snapshot.id),
    }
    touch_instance(snapshot.instance_id.as_deref());
}

/// Mark an agent as having just done something.
///
/// Called from [`persist_chat`] because that is the one point every kind of turn
/// already passes through — a workshop turn, a gateway message, a flow firing.
/// It used to be called only when an agent was patched or a conversation was
/// created, so an agent answering WhatsApp all day looked untouched since the day
/// it was minted, and both clients filed it under "history" after three days of
/// exactly the traffic it exists to handle.
fn touch_instance(instance_id: Option<&str>) {
    let Some(id) = instance_id else { return };
    let Ok(mut instance) = crate::agent_instance::load(id) else {
        // A conversation can outlive its agent (deleting one keeps its
        // transcripts on purpose). Not worth a warning on every turn.
        return;
    };
    instance.touch();
    if let Err(e) = instance.save() {
        // Never fails a turn: a stale `last_active_at` sorts a list wrong, which
        // is not worth losing the conversation that was just persisted.
        log::debug!("could not touch agent {id}: {e}");
    }
}

fn remove_chat_file(id: &str) {
    let path = chat_file_path(id);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("failed to delete chat file {}: {e}", path.display());
        }
    }
}

/// Load all chats from `<data>/chats/` into the in-memory store. Called once
/// at startup. Any chat whose file is malformed is logged and skipped.
fn load_persisted_chats() -> HashMap<String, Arc<Mutex<ChatSession>>> {
    let dir = paths::chats_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return HashMap::new(),
    };
    let mut out = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("failed to read chat file {}: {e}", path.display());
                continue;
            }
        };
        let pc: PersistedChat = match serde_json::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("failed to parse chat file {}: {e}", path.display());
                continue;
            }
        };
        // A reload resumes the *context*, not the whole file: everything before
        // the last divider is history this agent has already stopped seeing, and
        // replaying it here would undo every reset on the next restart.
        // Split the file back into the two halves: everything up to and including
        // the last divider is closed, the rest is the context to resume. Replaying
        // the whole file as context would quietly undo every reset on restart.
        let split = pc
            .messages
            .iter()
            .rposition(|m| matches!(m, ChatMessageWire::Reset { .. }))
            .map(|i| i + 1)
            .unwrap_or(0);
        let state = context_from_transcript(&pc.messages);
        let archived = pc.messages[..split].to_vec();
        let session = ChatSession {
            id: pc.id.clone(),
            instance_id: pc.instance_id.clone(),
            persona_slug: pc.persona_slug,
            model_name: pc.model_name,
            cwd: pc.cwd,
            preset: pc.preset,
            state,
            archived,
            created_at: pc.created_at,
            diagnostics: None,
            trace: None, // recreated lazily on the first turn, like diagnostics
            busy: false, // anything that was busy at shutdown couldn't have
            // finished cleanly; reset so the user can retry.
            interrupt: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        plan: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        out.insert(pc.id.clone(), Arc::new(Mutex::new(session)));
    }
    out
}

#[derive(Deserialize, utoipa::ToSchema)]
struct CreateChatRequest {
    /// Optional now: with an `agent_preset`, the persona comes from the preset's
    /// default. Kept for callers that still pick a persona directly.
    #[serde(default)]
    persona_slug: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
    /// The agent to start this conversation with. Defaults to `general-agent`.
    #[serde(default)]
    agent_preset: Option<String>,
    /// Continue an existing agent instead of minting a new one — how a named agent
    /// accumulates conversations.
    #[serde(default)]
    instance_id: Option<String>,
    /// Name the agent. A label only — nothing about an agent's lifetime follows
    /// from it, and nothing deletes an agent on a timer.
    #[serde(default)]
    name: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/chats",
    tag = "chats",
    responses((status = 200, body = Vec<ChatSummary>)),
)]
async fn list_chats(State(_state): State<Arc<ApiState>>) -> Response {
    // Read the chat list straight from `<data>/chats/*.json` rather than the
    // in-memory store. The two are kept in sync (every create/turn/delete
    // persists), but reading disk means the list is correct across restarts
    // and reflects any out-of-band edits — the in-memory map is only the
    // authority for *live* per-turn state, not the catalog.
    let mut out: Vec<ChatSummary> = read_persisted_chats()
        .into_iter()
        .map(ChatSummary::of)
        .collect();
    out.sort_by(|a, b| b.recency().cmp(a.recency()));
    Json(out).into_response()
}

/// Read and parse every `<data>/chats/*.json` into [`PersistedChat`]s.
/// Malformed files are logged and skipped. Shared by the list endpoint and
/// startup load.
/// How long a session survives with nothing said in it.
///
/// Sessions are what age out, not agents. A transcript nobody has opened in a
/// month is history; the agent that wrote it is a relationship, and everything it
/// learned lives in its memory rather than in the transcript — so dropping the
/// one costs a reading of what happened, and dropping the other would cost
/// everything it knows.
///
/// Thirty days is chosen to be longer than any plausible "I'll come back to
/// this" and short enough that a pod answering texts all year does not accrue an
/// unbounded directory that [`read_persisted_chats`] walks on every listing.
pub const SESSION_TTL_DAYS: i64 = 30;

/// What one sweep did. For the log and the tests; nothing branches on it.
#[derive(Debug, Default, Clone)]
pub struct ChatReapReport {
    pub reaped: Vec<String>,
    /// Ids that could not be removed, with why. Reported rather than fatal — one
    /// stuck file must not stop the sweep.
    pub failed: Vec<(String, String)>,
}

/// Delete sessions with no activity in [`SESSION_TTL_DAYS`], file and all.
///
/// One thing is spared: a session a **paused flow run** will resume into. An
/// approval waiting three weeks for a person is exactly the case this protects,
/// and resuming into a deleted transcript would hand the agent a decision it has
/// no context for.
///
/// Deliberately *not* spared: anything merely present in the in-memory store.
/// That map is hydrated from disk at startup, so it holds every chat this pod has
/// ever written — "in the map" means loaded, not open, and treating it as a guard
/// would spare everything and sweep nothing.
///
/// The map entry goes with the file, so the sweep frees the memory too rather
/// than leaving a session nobody can reach still resident.
///
/// **The agent is never touched.** Reaping every session an agent has does not
/// reap the agent: it goes on knowing what it learned, with nothing left to read.
pub async fn reap_stale_chats() -> ChatReapReport {
    let mut report = ChatReapReport::default();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(SESSION_TTL_DAYS);

    // A run that has not finished may still resume into its conversation.
    let awaited: Vec<String> = crate::flow_runs::list_runs(&paths::runs_dir())
        .into_iter()
        .filter(|r| r.status == "paused")
        .filter_map(|r| r.chat_id)
        .collect();

    let stale: Vec<String> = read_persisted_chats()
        .into_iter()
        .map(|c| c.id)
        .filter(|id| !awaited.contains(id))
        .filter(|id| {
            // An unparseable (or missing) timestamp is treated as recent:
            // refusing to guess is better than deleting somebody's transcript
            // because its clock field was odd.
            chat_updated_at(id)
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc) < cutoff)
                .unwrap_or(false)
        })
        .collect();
    if stale.is_empty() {
        return report;
    }

    let store = chat_store();
    let mut chats = store.lock().await;
    for id in stale {
        match std::fs::remove_file(chat_file_path(&id)) {
            Ok(()) => {
                chats.remove(&id);
                crate::memory::capture::record_session_end(&id);
                report.reaped.push(id);
            }
            Err(e) => report.failed.push((id, e.to_string())),
        }
    }
    report
}

fn read_persisted_chats() -> Vec<PersistedChat> {
    let dir = paths::chats_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path).map(|c| serde_json::from_str::<PersistedChat>(&c)) {
            Ok(Ok(pc)) => out.push(pc),
            Ok(Err(e)) => log::warn!("failed to parse chat file {}: {e}", path.display()),
            Err(e) => log::warn!("failed to read chat file {}: {e}", path.display()),
        }
    }
    out
}

#[utoipa::path(
    post,
    path = "/api/v1/chats",
    tag = "chats",
    request_body = CreateChatRequest,
    responses((status = 200, body = ChatSummary)),
)]
async fn post_create_chat(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<CreateChatRequest>,
) -> Response {
    use crate::agent_instance::{AgentInstance, InstanceOrigin};
    use crate::agent_preset::{AgentPreset, DEFAULT_PRESET};

    // Resolve the agent first: either continue an existing instance, or mint one from
    // a preset. The persona follows from that unless the caller named one explicitly.
    let mut instance = match &req.instance_id {
        Some(existing) => match crate::agent_instance::load(existing) {
            Ok(i) => i,
            Err(e) => return err_json(StatusCode::NOT_FOUND, e),
        },
        None => {
            let slug = req.agent_preset.as_deref().unwrap_or(DEFAULT_PRESET);
            match AgentPreset::load(slug, &paths::agent_presets_dir()) {
                Ok(preset) => match preset.ensure_spawnable() {
                    Ok(()) => AgentInstance::new(&preset, InstanceOrigin::Workshop),
                    Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
                },
                Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
            }
        }
    };

    // An explicit persona wins *for this conversation only*, and must be one this agent
    // can actually be — the same containment `sub_agent` enforces, applied at the front
    // door. It is not a persona move: this used to write the override back to the
    // instance, so starting one chat as a named persona silently repointed that agent
    // for every conversation after it. Moving an agent's persona is what
    // `PATCH /api/v1/agents/{id}` is for, and it is a thing someone asks for on purpose.
    let persona_slug = match &req.persona_slug {
        Some(p) => {
            if let Ok(preset) =
                AgentPreset::load(&instance.agent_preset, &paths::agent_presets_dir())
            {
                if !preset.allows_persona(p) {
                    return err_json(
                        StatusCode::BAD_REQUEST,
                        format!(
                            "persona '{p}' is not in agent '{}' (roster: {})",
                            preset.slug,
                            preset.callable_personas().join(", ")
                        ),
                    );
                }
            }
            p.clone()
        }
        None => instance.persona.clone(),
    };

    // Validate persona exists before creating a chat — fail fast instead of
    // surfacing the error mid-stream.
    if Persona::load(&persona_slug, &paths::personas_dir()).is_err() {
        return err_json(
            StatusCode::BAD_REQUEST,
            format!("persona '{persona_slug}' not found"),
        );
    }

    // A name for the agent this chat runs as — and only a name. This may be an agent
    // that already existed (`instance_id`), so promoting it here would change the
    // lifetime of something the caller only meant to label. It is protected anyway
    // while this conversation exists: the reaper keeps any agent a transcript points at.
    if let Some(name) = &req.name {
        instance.name = name.clone();
    }
    instance.touch();
    if let Err(e) = instance.save() {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let model_name = req
        .model_name
        .unwrap_or_else(crate::runtime::configured_default_model);
    let diagnostics = DiagnosticsLogger::new().ok().map(Arc::new);
    // The OTLP trace shares the diagnostics session-dir name so traces/<id> and
    // sessions/<id> line up. Best-effort, like diagnostics: never block a chat.
    let trace = trace_for(diagnostics.as_deref(), &model_name);
    let session = ChatSession {
        id: id.clone(),
        instance_id: Some(instance.id.clone()),
        persona_slug,
        model_name: model_name.clone(),
        cwd: state.cwd.clone(),
        preset: SessionPreset::Workshop,
        state: None,
        archived: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        diagnostics,
        trace,
        busy: false,
        interrupt: Arc::new(AtomicBool::new(false)),
        pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        plan: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let session_arc = Arc::new(Mutex::new(session));
    {
        let s = session_arc.lock().await;
        if let Some(logger) = &s.diagnostics {
            if let Ok(persona) = Persona::load(&s.persona_slug, &paths::personas_dir()) {
                let system_prompt = persona.build_system_prompt(&paths::skills_dir(), &s.cwd);
                logger.log_session_info(SessionInfo {
                    persona_name: &persona.name,
                    persona_slug: &s.persona_slug,
                    model_name: &s.model_name,
                    cwd: &s.cwd,
                    system_prompt: &system_prompt,
                    tools: &persona.resolved_tool_names(),
                    skills: &persona.skills,
                    auto_approve: true,
                    flow_id: None,
                    instance_id: s.instance_id.as_deref(),
                });
            }
        }
    }
    state
        .chats
        .lock()
        .await
        .insert(id.clone(), session_arc.clone());
    persist_chat(&session_arc).await;
    let s = session_arc.lock().await;
    Json(ChatSummary {
        id: s.id.clone(),
        instance_id: s.instance_id.clone(),
        persona_slug: s.persona_slug.clone(),
        model_name: s.model_name.clone(),
        created_at: s.created_at.clone(),
        turn_count: 0,
        // Just written by `persist_chat` above, so this is "now" — and a brand
        // new conversation with no `updated_at` would sort to the bottom of the
        // list it was just created at the top of.
        updated_at: chat_updated_at(&s.id),
        preview: None,
    })
    .into_response()
}

#[utoipa::path(
    get,
    path = "/api/v1/chats/{id}",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    responses((status = 200, body = ChatDetail), (status = 404, body = ErrorResponse)),
)]
async fn get_chat(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let chats = state.chats.lock().await;
    let Some(session) = chats.get(&id).cloned() else {
        return err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    drop(chats);
    let s = session.lock().await;
    // The transcript, not the context: a client renders the conversation, and a
    // reset must not look like the history was deleted.
    let messages = transcript_of(&s);
    Json(ChatDetail {
        id: s.id.clone(),
        instance_id: s.instance_id.clone(),
        persona_slug: s.persona_slug.clone(),
        model_name: s.model_name.clone(),
        created_at: s.created_at.clone(),
        messages,
        plan: s.plan.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    })
    .into_response()
}

#[utoipa::path(
    delete,
    path = "/api/v1/chats/{id}",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    responses((status = 200, description = "Deleted")),
)]
async fn delete_chat(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let mut chats = state.chats.lock().await;
    if chats.remove(&id).is_some() {
        drop(chats);
        remove_chat_file(&id);
        crate::memory::capture::record_session_end(&id);
        StatusCode::NO_CONTENT.into_response()
    } else {
        err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"))
    }
}

// ── Chat context: what a slash command acts on ──────────────────────────
//
// Everything a long conversation needs done *to* it rather than *said* to it.
// These exist because a client had no way to ask: typing `/compact` into a chat
// sent the literal text to the model, which spent a turn interpreting it as
// prose. Compaction was reachable only by drifting past 60% of the window.

/// What a chat's context currently costs.
#[derive(Serialize, utoipa::ToSchema)]
struct ChatContext {
    /// Rough estimate (~4 chars per token), the same one compaction decides on.
    /// Not the provider's count — good enough to answer "how full is this?".
    estimated_tokens: usize,
    message_count: usize,
    /// The window compaction sizes against.
    context_window: usize,
    /// Automatic compaction fires above this. A client can render the headroom.
    compact_threshold_tokens: usize,
    /// Whether the next turn would compact on its own.
    would_compact: bool,
}

/// The result of a forced compaction.
#[derive(Serialize, utoipa::ToSchema)]
struct ChatCompacted {
    /// False when there was nothing old enough to summarize — not an error, and
    /// the honest answer for a short conversation.
    compacted: bool,
    tokens_before: usize,
    tokens_after: usize,
    messages_before: usize,
    messages_after: usize,
    /// The summary that replaced the old half, when one was produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

fn chat_context_of(state: Option<&AgentState>) -> ChatContext {
    let config = crate::context::CompactionConfig::default();
    let estimated_tokens = state.map(crate::context::estimate_tokens).unwrap_or(0);
    let threshold = (config.context_window as f64 * config.compact_threshold) as usize;
    ChatContext {
        estimated_tokens,
        message_count: state.map(|s| s.messages.len()).unwrap_or(0),
        context_window: config.context_window,
        compact_threshold_tokens: threshold,
        would_compact: estimated_tokens >= threshold,
    }
}

/// What this chat's context costs right now — the read behind `/tokens`, and the
/// number a client needs to show headroom before someone hits the wall.
#[utoipa::path(
    get,
    path = "/api/v1/chats/{id}/context",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    responses((status = 200, body = ChatContext)),
)]
async fn get_chat_context(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let chats = state.chats.lock().await;
    let Some(session) = chats.get(&id).cloned() else {
        return err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    drop(chats);
    let s = session.lock().await;
    Json(chat_context_of(s.state.as_ref())).into_response()
}

/// Compact this chat's context now — `/compact`.
///
/// Deliberately does everything an automatic compaction does, including handing
/// the summary to memory: it is the most concentrated account of the conversation
/// that will ever exist and the LLM call is already paid for, so a forced
/// compaction that dropped it would quietly be worth less than one that happened
/// by itself.
///
/// Refuses mid-turn. Compaction rewrites the message list the running turn is
/// reading, and "your context changed under you" is not a failure mode worth
/// having.
#[utoipa::path(
    post,
    path = "/api/v1/chats/{id}/compact",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    responses(
        (status = 200, body = ChatCompacted),
        (status = 409, description = "The chat is mid-turn"),
    ),
)]
async fn post_chat_compact(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let chats = state.chats.lock().await;
    let Some(session) = chats.get(&id).cloned() else {
        return err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    drop(chats);

    // Claim the session the same way a turn does, so the two can never interleave.
    let (mut agent_state, model_name, persona_slug, instance_id) = {
        let mut s = session.lock().await;
        if s.busy {
            return err_json(StatusCode::CONFLICT, "chat is already mid-turn");
        }
        let Some(agent_state) = s.state.clone() else {
            // Nothing said yet: report the no-op rather than inventing a summary.
            return Json(ChatCompacted {
                compacted: false,
                tokens_before: 0,
                tokens_after: 0,
                messages_before: 0,
                messages_after: 0,
                summary: None,
            })
            .into_response();
        };
        s.busy = true;
        (
            agent_state,
            s.model_name.clone(),
            s.persona_slug.clone(),
            s.instance_id.clone(),
        )
    };

    /// Release `busy` on every exit path — an early return that left it set would
    /// wedge the chat for the rest of the process.
    async fn release(session: &Arc<Mutex<ChatSession>>) {
        session.lock().await.busy = false;
    }

    // Built before any await: `from_environment`'s error is a non-Send boxed
    // error and must not be held across a yield point.
    let context_result = AgentRuntimeContext::from_environment().map_err(|e| e.to_string());
    let context = match context_result {
        Ok(c) => c,
        Err(msg) => {
            release(&session).await;
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("runtime not available: {msg}"),
            );
        }
    };
    let client_result =
        crate::runtime::build_openai_client(&context.api_key).map_err(|e| e.to_string());
    let client = match client_result {
        Ok(c) => c,
        Err(msg) => {
            release(&session).await;
            return err_json(StatusCode::SERVICE_UNAVAILABLE, msg);
        }
    };

    // The trait that puts `completion_model` on a rig client; scoped here so the
    // import cannot collide with the turn path's own model construction.
    use rig::client::CompletionClient as _;

    let tokens_before = crate::context::estimate_tokens(&agent_state);
    let messages_before = agent_state.messages.len();
    let outcome = crate::context::compact_now(
        &mut agent_state,
        &client.completion_model(&model_name),
        &crate::context::CompactionConfig::default(),
    )
    .await;

    let summary = match outcome {
        Ok(summary) => summary,
        Err(msg) => {
            release(&session).await;
            return err_json(StatusCode::BAD_GATEWAY, format!("could not compact: {msg}"));
        }
    };

    let tokens_after = crate::context::estimate_tokens(&agent_state);
    let messages_after = agent_state.messages.len();
    {
        let mut s = session.lock().await;
        // Only on success: a failed summary must not truncate anyone's history.
        if summary.is_some() {
            s.state = Some(agent_state);
        }
        s.busy = false;
    }
    if let Some(summary) = &summary {
        crate::memory::capture::record_compaction(
            &crate::memory::capture::CaptureContext {
                chat_id: Some(id.clone()),
                persona: Some(persona_slug),
                instance_id,
            },
            summary,
        );
        persist_chat(&session).await;
    }

    Json(ChatCompacted {
        compacted: summary.is_some(),
        tokens_before,
        tokens_after,
        messages_before,
        messages_after,
        summary,
    })
    .into_response()
}

/// End this conversation's context without ending the conversation — `/reset`.
///
/// The agent starts its next turn from nothing, and the transcript gains a
/// divider where that happened. Nothing is deleted: this used to be `/clear`,
/// which dropped the history outright because the transcript and the context were
/// the same list and there was nowhere else for it to live. `DELETE /chats/{id}`
/// is still how a conversation actually goes away.
///
/// Refuses mid-turn, like compaction: pulling the context out from under a
/// running turn is not a failure mode worth having.
#[utoipa::path(
    post,
    path = "/api/v1/chats/{id}/reset",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    responses(
        (status = 200, body = ChatContext),
        (status = 409, description = "The chat is mid-turn"),
    ),
)]
async fn post_chat_reset(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let chats = state.chats.lock().await;
    let Some(session) = chats.get(&id).cloned() else {
        return err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    drop(chats);
    {
        let mut s = session.lock().await;
        if s.busy {
            return err_json(StatusCode::CONFLICT, "chat is already mid-turn");
        }
        reset_context(&mut s, "reset").await;
    }
    persist_chat(&session).await;
    Json(chat_context_of(None)).into_response()
}

/// Reset a conversation's context and tell anyone watching.
///
/// The one path every reset goes through — the explicit endpoint, the gateway's
/// idle cutoff, a flow's pre-run wipe — so the divider is written and broadcast
/// exactly once regardless of who asked.
async fn reset_context(s: &mut ChatSession, reason: &str) {
    let mark = mark_reset(s, reason);
    if let ChatMessageWire::Reset { at, reason } = mark {
        // A send error only means nobody is watching; the mark is in the
        // transcript either way, and the next load will render it.
        let _ = chat_event_sender(&s.id)
            .await
            .send(ChatEvent::Reset { at, reason });
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct ChatTurnRequest {
    message: String,
}

/// SSE event wire format. One JSON object per event. The `kind` field
/// discriminates; payloads vary by kind. Events form a lifecycle:
///   `turn_started` → `phase`* → (`llm_started` → `llm_completed`
///                   → `tool_started`* → `tool_completed`*)+
///                   → `done`
/// (`tool_started` and `tool_completed` can repeat per LLM step; `phase` frames
/// cover the pre-model work that would otherwise be silent.)
#[derive(Serialize, Clone, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChatEvent {
    /// Marks the start of a turn — emitted once at the top of `post_chat_turn`
    /// so the workshop can open a new group in the transcript.
    TurnStarted {
        turn_index: usize,
        user_message: String,
        /// Diagnostics session directory name for this chat, so the workshop can
        /// deep-link a turn (or a turn error) to its session logs. `None` for a
        /// chat with no active logger — e.g. one reloaded after an agent restart.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// LLM call has started. Paired with a later `LlmCompleted`.
    LlmStarted,
    /// LLM call finished. `messages` is any new assistant message(s); tool
    /// calls produced by this LLM step are emitted as separate `ToolStarted`
    /// events immediately after.
    LlmCompleted {
        messages: Vec<ChatMessageWire>,
        duration_ms: u64,
    },
    /// A tool the LLM requested is about to run.
    ToolStarted {
        tool_call_id: String,
        name: String,
        args: serde_json::Value,
    },
    /// The tool finished. `result` is the `ToolResult` message that was
    /// appended to state.
    ToolCompleted {
        tool_call_id: String,
        name: String,
        duration_ms: u64,
        result: ChatMessageWire,
    },
    /// The agent's user-facing message, produced by a `say_to_user` or
    /// `ask_user` tool call. In tool-only mode this — not free-text
    /// `LlmCompleted` content — is the assistant's message; the workshop renders
    /// it as the reply bubble. The underlying tool start/finish events are
    /// suppressed so the reply isn't also shown as a raw tool card.
    ///
    /// `awaiting_reply` distinguishes the two ways a turn ends: an answer closes
    /// the exchange, a question leaves it open and the client should invite a
    /// response (and may render `options` as tappable choices). Both are omitted
    /// when unset, so a client written before `ask_user` existed still sees a
    /// perfectly ordinary reply — which is the correct degradation, since the
    /// question is in the text either way.
    Reply {
        content: String,
        #[serde(default, skip_serializing_if = "core::ops::Not::not")]
        awaiting_reply: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
    },
    /// What the turn is doing *before* the model is called — work that emits no
    /// other frame and that a client would otherwise render as an
    /// undifferentiated wait.
    ///
    /// Compaction is a whole extra summarization LLM call and recall is an
    /// embeddings call; both run ahead of the executor, so nothing between
    /// `turn_started` and the first `llm_started` used to say a word. `phase` is
    /// an open string (`crate::runtime::phase`) precisely so a client older than
    /// a phase can drop the frame instead of breaking on it.
    Phase { phase: String },
    /// A message was accepted while a turn was already running, and is waiting
    /// its turn rather than starting one.
    ///
    /// Emitted on the chat's live bus rather than on the sending request, which
    /// has already been answered with 202 — the point of queueing is that the
    /// sender does not hold a connection open waiting. `position` is 1 for the
    /// next message to run.
    Queued { message: String, position: usize },
    /// This turn's plan, as `update_plan` wrote it. Sent on every change,
    /// including the empty list a new turn starts with, so a client renders the
    /// plan as it stands rather than accumulating every version of it.
    Plan { steps: Vec<crate::turn_plan::PlanStep> },
    /// A queued message joined the turn that was already running, rather than
    /// waiting for it to end. The client should stop showing it as pending.
    Injected { message: String },
    /// Terminal event. `status` is "completed" | "interrupted" | "failed".
    Done {
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A classified turn failure, safe to show the user. Emitted instead of a
    /// bare `Done{status:"failed"}` so frontends can render a friendly message
    /// (and branch on `code`) rather than the raw provider-error chain. `code`
    /// is the machine-readable identifier (see `runtime::ErrorCode`); `message`
    /// is the user-facing text; `retryable` hints whether "try again" is useful.
    /// A `done` still follows so lifecycle handling is unchanged for old clients.
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
    /// The conversation's context ended — see [`ChatMessageWire::Reset`].
    ///
    /// Not part of a turn's lifecycle: it arrives on its own, usually with nobody
    /// having typed anything. Without it a client watching a flow's agent would
    /// see the 3am firing start from nothing and have no way to say why, since
    /// the divider it should draw is only in the transcript it loaded hours ago.
    Reset { at: String, reason: String },
}

/// The answer to a turn request that arrived mid-turn: taken, not started.
#[derive(Serialize, utoipa::ToSchema)]
struct ChatQueued {
    /// Always true. Present so a client can branch on the body rather than on
    /// the status code, which is easy to lose through a wrapper.
    queued: bool,
    /// Place in line, 1 for the next message to run.
    position: usize,
}

#[derive(Serialize, utoipa::ToSchema)]
struct ChatInterrupt {
    /// True when a turn was running and has been asked to stop. False when the
    /// chat was already idle — the turn finished on its own between the press
    /// and this request, which is not a failure and must not be shown as one.
    stopping: bool,
}

/// Continue the run, unless someone has pressed stop.
///
/// Called at both of the step guard's exits rather than at its entry, so the
/// frames for the step that just ran reach the transcript before the turn ends.
/// The stop therefore lands at the next step boundary — a tool call in flight
/// finishes, an LLM call in flight is still paid for — which is the strongest
/// promise a step guard can keep.
fn continue_unless_stopped(interrupt: &AtomicBool) -> GuardAction {
    if interrupt.load(Ordering::Relaxed) {
        GuardAction::Stop("Stopped by the user.".into())
    } else {
        GuardAction::Continue
    }
}

/// Wrap a guard so the stop button reaches a turn that built its own.
///
/// The workshop chat's guard checks the flag itself (it is built inline, per
/// turn, around this chat's event sender). The headless paths — a scheduled
/// follow-up, an inbound gateway message — share the general agent guard, and
/// they run against the *same chat*, which a client can be watching with the
/// same stop button. `/interrupt` answers `stopping: true` for any of them
/// because the chat is busy; this is what makes that answer true.
fn stoppable(guard: StepGuard<AgentState>, interrupt: Arc<AtomicBool>) -> StepGuard<AgentState> {
    Arc::new(move |state: &AgentState, ev| {
        // The inner guard runs first and unconditionally: it is what writes the
        // turn's diagnostics, and a stopped turn should be as legible afterwards
        // as any other.
        let action = guard(state, ev);
        match continue_unless_stopped(&interrupt) {
            GuardAction::Stop(reason) => GuardAction::Stop(reason),
            GuardAction::Continue => action,
        }
    })
}

/// Ask a running turn to stop — the client's stop button.
///
/// Returns as soon as the request is recorded, not when the turn ends: the
/// executor notices at its next step boundary and finishes the turn itself,
/// emitting `done{status:"interrupted"}` on the chat's event stream. That frame,
/// not this response, is what says the agent has actually stopped.
///
/// Interrupting is resumable by construction — the guard stops the executor
/// between steps, so the partial state is written back like any other turn and
/// the next message continues from it. Nothing is discarded.
#[utoipa::path(
    post,
    path = "/api/v1/chats/{id}/interrupt",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    responses(
        (status = 200, body = ChatInterrupt),
        (status = 404, description = "No such chat"),
    ),
)]
async fn post_chat_interrupt(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Response {
    let chats = state.chats.lock().await;
    let Some(session) = chats.get(&id).cloned() else {
        return err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    drop(chats);
    let s = session.lock().await;
    // An idle chat is not an error: pressing stop as the last frame arrives is
    // an ordinary race, and the honest answer is "there was nothing to stop".
    let stopping = s.busy;
    if stopping {
        s.interrupt.store(true, Ordering::Relaxed);
    }
    Json(ChatInterrupt { stopping }).into_response()
}

/// Run one turn against the chat session. Streams new messages as Server-Sent
/// Events as the agent steps; closes the connection when the executor returns.
#[axum::debug_handler]
#[utoipa::path(
    post,
    path = "/api/v1/chats/{id}/turn",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    request_body = ChatTurnRequest,
    responses(
        (status = 200, description = "SSE stream (text/event-stream) of ChatEvent frames", body = ChatEvent, content_type = "text/event-stream"),
        (status = 202, description = "A turn was already running: the message is queued and will run next. Its frames arrive on `GET /chats/{id}/events`, not on this response.", body = ChatQueued),
        (status = 404, description = "No such chat"),
    ),
)]
async fn post_chat_turn(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Json(req): Json<ChatTurnRequest>,
) -> axum::response::Response {
    let chats = state.chats.lock().await;
    let Some(session) = chats.get(&id).cloned() else {
        return err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    drop(chats);

    // Lock the session up-front: stamp it busy, snapshot what we need to run
    // the executor, and seed/continue the AgentState. If anything fails we
    // release `busy` before returning.
    let (
        persona_slug,
        model_name,
        cwd,
        agent_state,
        turn_index,
        diagnostics,
        trace,
        interrupt,
        pending_for_mailbox,
        plan_for_sink,
    ) = {
        let mut s = session.lock().await;
        if s.busy {
            // Queue rather than refuse. A person who thinks of something while
            // the agent is eight tool calls deep should not have to hold the
            // thought until it finishes — and 409 made the client lock its
            // composer, which is what made that necessary.
            let position = queue_message(&mut lock_pending(&s.pending), req.message.clone());
            drop(s);
            let _ = chat_event_sender(&id).await.send(ChatEvent::Queued {
                message: req.message.clone(),
                position,
            });
            return (
                StatusCode::ACCEPTED,
                Json(ChatQueued {
                    queued: true,
                    position,
                }),
            )
                .into_response();
        }
        s.busy = true;
        // Start clean: a stop pressed while nothing was running must not stop
        // this turn before it has taken a step.
        s.interrupt.store(false, Ordering::Relaxed);
        let prior_turns = s
            .state
            .as_ref()
            .map(|st| {
                st.messages
                    .iter()
                    .filter(|m| matches!(m, AgentMessage::User(_)))
                    .count()
            })
            .unwrap_or(0);
        let next_state = match s.state.take() {
            Some(prev) => prev.continue_with(req.message.clone()),
            None => AgentState::new(req.message.clone()),
        };

        // A chat reloaded after an agent restart comes back from disk with no
        // diagnostics logger — `PersistedChat` doesn't round-trip it. Recreate
        // one on the first turn so observability resumes; it opens a fresh
        // session dir for this process's run of the chat. Best-effort, exactly
        // like `create_chat`: a logger failure must never block the turn.
        if s.diagnostics.is_none() {
            if let Some(logger) = DiagnosticsLogger::new().ok().map(Arc::new) {
                if let Ok(persona) = Persona::load(&s.persona_slug, &paths::personas_dir()) {
                    let system_prompt = persona.build_system_prompt(&paths::skills_dir(), &s.cwd);
                    logger.log_session_info(SessionInfo {
                        persona_name: &persona.name,
                        persona_slug: &s.persona_slug,
                        model_name: &s.model_name,
                        cwd: &s.cwd,
                        system_prompt: &system_prompt,
                        tools: &persona.tools,
                        skills: &persona.skills,
                        auto_approve: true,
                        flow_id: None,
                        instance_id: s.instance_id.as_deref(),
                    });
                }
                s.diagnostics = Some(logger);
            }
        }
        // Mirror the trace logger: a reloaded chat has neither logger; recreate
        // both so the OTLP trace resumes on the same session id as diagnostics.
        if s.trace.is_none() {
            s.trace = trace_for(s.diagnostics.as_deref(), &s.model_name);
        }

        (
            s.persona_slug.clone(),
            s.model_name.clone(),
            s.cwd.clone(),
            next_state,
            prior_turns, // new turn's index = prior count (0-based)
            s.diagnostics.clone(),
            s.trace.clone(),
            s.interrupt.clone(),
            s.pending.clone(),
            s.plan.clone(),
        )
    };

    // Build the error message string before any await so the non-Send
    // `Box<dyn Error>` from from_environment doesn't get held across a yield.
    let context_result = AgentRuntimeContext::from_environment().map_err(|e| e.to_string());
    let context = match context_result {
        Ok(c) => c,
        Err(msg) => {
            session.lock().await.busy = false;
            return err_json(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("runtime not available: {msg}"),
            );
        }
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
    let session_for_task = session.clone();

    // The diagnostics session directory name doubles as the session id the
    // workshop uses to open this chat's logs (see `get_diagnostics_session`).
    let session_id = diagnostics.as_ref().and_then(|d| {
        d.session_dir()
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    });

    // TurnStarted goes out immediately so the workshop can open a fresh
    // group in the transcript before any agent activity.
    let _ = tx
        .send(ChatEvent::TurnStarted {
            turn_index: turn_index,
            user_message: req.message.clone(),
            session_id,
        })
        .await;

    // Open the OTLP turn span for this user message before any agent activity.
    if let Some(t) = &trace {
        t.start_turn(&req.message);
    }

    tokio::spawn(async move {
        // Per-turn timing state, shared between the LlmCallHook (start) and
        // the step_guard (finish).
        let llm_started_at: Arc<std::sync::Mutex<Option<std::time::Instant>>> =
            Arc::new(std::sync::Mutex::new(None));
        let tools_in_flight: Arc<std::sync::Mutex<HashMap<String, std::time::Instant>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));

        // LlmCallHook fires before each LLM call. Open the stopwatch and
        // notify the client.
        let llm_call_hook: metalcraft::LlmCallHook = {
            let tx = tx.clone();
            let started = llm_started_at.clone();
            let diagnostics = diagnostics.clone();
            let trace = trace.clone();
            Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
                if let Some(logger) = &diagnostics {
                    logger.log_llm_request(snapshot);
                }
                if let Some(t) = &trace {
                    t.on_llm_start();
                }
                *started.lock().unwrap() = Some(std::time::Instant::now());
                let _ = tx.try_send(ChatEvent::LlmStarted);
            })
        };

        // LlmResponseHook fires after each `.send()` returns, while the LLM span
        // is still open (the step_guard hasn't run yet) — stamp token usage onto
        // it. This is the only signal the pre-call hook above can't provide.
        let llm_response_hook: metalcraft::LlmResponseHook = {
            let trace = trace.clone();
            Arc::new(move |snapshot: &metalcraft::LlmResponseSnapshot| {
                if let Some(t) = &trace {
                    let u = &snapshot.usage;
                    t.on_llm_usage(
                        u.input_tokens,
                        u.output_tokens,
                        u.total_tokens,
                        u.cached_input_tokens,
                        u.reasoning_tokens,
                    );
                }
            })
        };

        // step_guard fires after every node step. Diff against `seen` to
        // pick up newly-appended messages and translate them into the
        // appropriate fine-grained events.
        let seen_up_to = Arc::new(std::sync::Mutex::new(agent_state.messages.len()));
        let step_guard: StepGuard<AgentState> = {
            let tx = tx.clone();
            let seen = seen_up_to.clone();
            let llm_started_at = llm_started_at.clone();
            let tools_in_flight = tools_in_flight.clone();
            let diagnostics = diagnostics.clone();
            let trace = trace.clone();
            let interrupt = interrupt.clone();
            Arc::new(move |state: &AgentState, _ev| {
                if let Some(logger) = &diagnostics {
                    logger.log_turn(state);
                }
                let mut guard = seen.lock().unwrap();
                if *guard >= state.messages.len() {
                    return continue_unless_stopped(&interrupt);
                }
                let new = &state.messages[*guard..];
                *guard = state.messages.len();

                // Bucket the new messages so we can emit:
                //   LlmCompleted (with assistant text + duration) FIRST,
                //   then ToolStarted per ToolCall,
                //   then ToolCompleted per ToolResult.
                let mut assistant_msgs: Vec<ChatMessageWire> = Vec::new();
                // Raw assistant text for the OTLP LLM-span response event.
                let mut assistant_text = String::new();
                let mut new_tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();
                // (id, name, wire, raw_result) — raw_result feeds the trace span.
                let mut new_tool_results: Vec<(String, String, ChatMessageWire, String)> =
                    Vec::new();

                for m in new {
                    match m {
                        AgentMessage::Assistant(t) => {
                            if !assistant_text.is_empty() {
                                assistant_text.push('\n');
                            }
                            assistant_text.push_str(t);
                            assistant_msgs.push(ChatMessageWire::from(m));
                        }
                        AgentMessage::ToolCall { id, name, args, .. } => {
                            new_tool_calls.push((id.clone(), name.clone(), args.clone()));
                        }
                        AgentMessage::ToolResult {
                            id, name, result, ..
                        } => {
                            new_tool_results.push((
                                id.clone(),
                                name.clone(),
                                ChatMessageWire::from(m),
                                result.clone(),
                            ));
                        }
                        AgentMessage::User(_) => {
                            // User messages mid-turn shouldn't happen, but
                            // include them in the assistant batch so they
                            // aren't silently dropped.
                            assistant_msgs.push(ChatMessageWire::from(m));
                        }
                        AgentMessage::Reasoning { .. } => {
                            // Internal replay artifact — persisted via the full
                            // AgentState, but not surfaced as a UI chat event.
                        }
                    }
                }

                // If this step had any LLM-produced messages, close the
                // LLM stopwatch. (Pure tool steps leave the stopwatch alone.)
                if !assistant_msgs.is_empty() || !new_tool_calls.is_empty() {
                    let duration_ms = llm_started_at
                        .lock()
                        .unwrap()
                        .take()
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    if let Some(t) = &trace {
                        t.on_llm_complete(if assistant_text.is_empty() {
                            None
                        } else {
                            Some(&assistant_text)
                        });
                    }
                    let _ = tx.try_send(ChatEvent::LlmCompleted {
                        messages: assistant_msgs,
                        duration_ms,
                    });
                    for (tool_call_id, name, args) in new_tool_calls {
                        tools_in_flight
                            .lock()
                            .unwrap()
                            .insert(tool_call_id.clone(), std::time::Instant::now());
                        if let Some(t) = &trace {
                            t.on_tool_start(&tool_call_id, &name, &args);
                        }
                        // The reply tools are surfaced as a `Reply` (emitted by
                        // the reply sink during execution), not as a tool card.
                        if !is_reply_tool(&name) {
                            let _ = tx.try_send(ChatEvent::ToolStarted {
                                tool_call_id,
                                name,
                                args,
                            });
                        }
                    }
                }

                for (tool_call_id, name, result, raw_result) in new_tool_results {
                    let duration_ms = tools_in_flight
                        .lock()
                        .unwrap()
                        .remove(&tool_call_id)
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    if let Some(t) = &trace {
                        t.on_tool_complete(&tool_call_id, &raw_result);
                    }
                    // See the matching suppression for `ToolStarted`. A reply
                    // tool that was *refused* (the plan gate turning down an
                    // early `say_to_user`) is suppressed too: nothing was
                    // delivered, the model is about to carry on working, and a
                    // red tool card for a rail doing its job reads as a failure.
                    if !is_reply_tool(&name) {
                        let _ = tx.try_send(ChatEvent::ToolCompleted {
                            tool_call_id,
                            name,
                            duration_ms,
                            result,
                        });
                    }
                }

                continue_unless_stopped(&interrupt)
            })
        };

        // Keep the pre-turn state so a hard failure can be rolled back. The
        // session's state was `take()`n before this task started, so without
        // this restore a failed turn would silently wipe the entire chat
        // history (the next turn would start from scratch).
        let state_before_turn = agent_state.clone();

        // Tool-only output: the agent replies by calling `say_to_user`, which
        // routes the text here as a `Reply` event on the SSE stream. The tool
        // is also terminal, so the turn ends once the reply is sent.
        let reply_sink: crate::tools::ReplySink = {
            let tx = tx.clone();
            Arc::new(move |reply: crate::tools::ReplyEnvelope| {
                let tx = tx.clone();
                Box::pin(async move {
                    tx.send(ChatEvent::Reply {
                        content: reply.text,
                        awaiting_reply: reply.awaiting_reply,
                        options: reply.options,
                    })
                    .await
                    .map_err(|e| e.to_string())
                })
            })
        };

        // What the turn is doing before the model is reached. `try_send`, and a
        // full channel just drops the note: a progress frame must never apply
        // backpressure to the turn it is describing.
        let phase_sink: crate::runtime::PhaseSink = {
            let tx = tx.clone();
            Arc::new(move |phase: &str| {
                let _ = tx.try_send(ChatEvent::Phase {
                    phase: phase.to_string(),
                });
            })
        };

        let outcome = run_chat_turn(
            &context,
            &persona_slug,
            &cwd,
            &model_name,
            Some(&id),
            agent_state,
            step_guard,
            Some(llm_call_hook),
            Some(llm_response_hook),
            RuntimeOptions {
                reply_sink: Some(reply_sink),
                tool_choice: metalcraft::ToolChoice::Required,
                terminal_tools: vec!["say_to_user".to_string(), "ask_user".to_string()],
                // A follow-up armed during this turn is delivered back to this
                // chat when it fires.
                session_binding: Some(crate::scheduled_tasks::IoBinding::WorkshopChat {
                    chat_id: id.clone(),
                }),
                reschedule_depth: 0,
                prompt_extras: crate::persona::PromptExtras::load().await,
                preset_personas: None,
                instance_id: None,
                // Delegation runs a whole agent inside one tool call, where the
                // step guard above cannot reach. `sub_agent` reads this flag so
                // stop lands there too.
                interrupt: Some(interrupt.clone()),
                plan_sink: Some(plan_sink(plan_for_sink.clone(), {
                    let tx = tx.clone();
                    Arc::new(move |ev| {
                        let _ = tx.try_send(ev);
                    })
                })),
            },
            Some(phase_sink),
            // What the person types while this turn runs, delivered at the
            // next safe boundary instead of waiting for the turn to end.
            chat_mailbox(pending_for_mailbox.clone(), {
                let tx = tx.clone();
                Arc::new(move |ev| {
                    let _ = tx.try_send(ev);
                })
            })
            .into(),
        )
        .await;

        // Write the final state back to the session (so the next turn can
        // continue from it) and emit the terminal event. Every arm leaves a
        // state behind — even a failure rolls back to the pre-turn state — so
        // we always persist afterward.
        {
            let mut s = session_for_task.lock().await;
            s.busy = false;
            match outcome {
                Ok(RunOutcome::Completed(state)) => {
                    s.state = Some(state);
                    if let Some(t) = &trace {
                        t.end_turn(true);
                    }
                    let _ = tx
                        .send(ChatEvent::Done {
                            status: "completed".into(),
                            reason: None,
                        })
                        .await;
                }
                Ok(RunOutcome::Interrupted { state, reason, .. }) => {
                    s.state = Some(state);
                    if let Some(t) = &trace {
                        t.end_turn(true);
                    }
                    let _ = tx
                        .send(ChatEvent::Done {
                            status: "interrupted".into(),
                            reason: Some(reason),
                        })
                        .await;
                }
                Ok(RunOutcome::Failed { state, node, error }) => {
                    // metalcraft >=0.6.0 hands back the partial state on a node
                    // failure. Keep it (rather than rolling back) so the failed
                    // turn's completed assistant/tool steps survive in the chat
                    // transcript, not just in the diagnostics turn files.
                    let reason = format!("{node}: {error}");
                    if let Some(logger) = &diagnostics {
                        logger.log_error(&reason);
                    }
                    if let Some(t) = &trace {
                        t.on_error(&reason);
                        t.end_turn(false);
                    }
                    s.state = Some(state);
                    // Classified, user-safe error frame (new clients render this);
                    // the raw `reason` still rides in the trailing `done` for old
                    // clients and diagnostics deep-linking.
                    let ce = crate::runtime::classify_turn_error(&error);
                    let _ = tx
                        .send(ChatEvent::Error {
                            code: ce.code.as_str().into(),
                            message: ce.user_message,
                            retryable: ce.retryable,
                        })
                        .await;
                    let _ = tx
                        .send(ChatEvent::Done {
                            status: "failed".into(),
                            reason: Some(reason),
                        })
                        .await;
                }
                Err(e) => {
                    // A framework-level error (step-limit, checkpoint) with no
                    // recoverable state. Walk the source chain so the reason
                    // carries the real cause, roll back to the pre-turn state so
                    // the chat keeps its history, and record the failure on disk.
                    let reason = error_chain(e.as_ref());
                    if let Some(logger) = &diagnostics {
                        logger.log_error(&reason);
                    }
                    if let Some(t) = &trace {
                        t.on_error(&reason);
                        t.end_turn(false);
                    }
                    s.state = Some(state_before_turn);
                    let ce = crate::runtime::classify_turn_error(&reason);
                    let _ = tx
                        .send(ChatEvent::Error {
                            code: ce.code.as_str().into(),
                            message: ce.user_message,
                            retryable: ce.retryable,
                        })
                        .await;
                    let _ = tx
                        .send(ChatEvent::Done {
                            status: "failed".into(),
                            reason: Some(reason),
                        })
                        .await;
                }
            }
        }
        persist_chat(&session_for_task).await;

        // Anything the person sent while this turn ran now gets its turn. Runs
        // inside the same spawned task rather than a new one, so a queue that
        // refills mid-drain stays one conversation running in order.
        drain_queued_turns(&context, &id).await;
    });

    let stream = ReceiverStream::new(rx).map(|ev| -> Result<Event, Infallible> {
        Ok(Event::default().json_data(&ev).unwrap_or_else(|_| {
            Event::default()
                .data("{\"kind\":\"done\",\"status\":\"failed\",\"reason\":\"serialize\"}")
        }))
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

/// Live subscription to a chat's *agent-initiated* turns (scheduled follow-ups).
/// The workshop opens this when a chat is on screen so a follow-up that fires
/// while the user is idle streams in without a page refresh. Normal
/// user-initiated turns still come back on their own `POST .../turn` response.
#[utoipa::path(
    get,
    path = "/api/v1/chats/{id}/events",
    tag = "chats",
    params(("id" = String, Path, description = "Chat id")),
    responses((status = 200, description = "SSE stream of agent-initiated ChatEvent frames", body = ChatEvent, content_type = "text/event-stream")),
)]
async fn get_chat_events(State(_state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let sender = chat_event_sender(&id).await;
    let rx = sender.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|res| async move {
        match res {
            Ok(ev) => Some(Ok::<Event, Infallible>(
                Event::default().json_data(&ev).unwrap_or_else(|_| {
                    Event::default()
                        .data("{\"kind\":\"done\",\"status\":\"failed\",\"reason\":\"serialize\"}")
                }),
            )),
            // A lagged subscriber just skips missed events rather than erroring.
            Err(_) => None,
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

/// List scheduled follow-ups (pending + recently completed), newest first.
#[utoipa::path(
    get,
    path = "/api/v1/scheduled-tasks",
    tag = "scheduled-tasks",
    responses((status = 200, description = "Scheduled follow-ups", body = Object)),
)]
async fn list_scheduled_tasks(State(_state): State<Arc<ApiState>>) -> Response {
    Json(crate::scheduled_tasks::list()).into_response()
}

/// Cancel a pending scheduled follow-up.
#[utoipa::path(
    delete,
    path = "/api/v1/scheduled-tasks/{id}",
    tag = "scheduled-tasks",
    params(("id" = String, Path, description = "Task id")),
    responses((status = 200, description = "Deleted")),
)]
async fn delete_scheduled_task(
    State(_state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Response {
    match crate::scheduled_tasks::cancel(&id) {
        Ok(true) => (
            StatusCode::OK,
            Json(serde_json::json!({ "cancelled": true })),
        )
            .into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "no pending follow-up with that id"),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Run whatever queued up while a turn was in flight, one message at a time,
/// until the queue is empty.
///
/// Called after a turn releases the session, so it claims `busy` the ordinary
/// way and cannot run beside a user turn. Frames go to the chat's live bus
/// rather than to an HTTP response: the request that queued the message was
/// answered with 202 long ago, so the bus (`GET /chats/{id}/events`) is the only
/// place a client can still be listening.
///
/// Loops rather than recursing, because a queue that refills while it is being
/// drained is the normal case — someone typing three things in a row — and the
/// alternative is one task per message, each waiting on the last.
/// Record the plan on the session and announce it, in that order.
///
/// Both halves matter and they are not the same audience: the frame reaches
/// whoever is watching *now*, and the recorded copy is what a client arriving
/// late is handed by `GET /chats/{id}`. A sink that only sent the frame would
/// leave a reconnecting client with no plan until the next `update_plan` call,
/// which on a long step is minutes.
fn plan_sink(
    held: Arc<std::sync::Mutex<Vec<crate::turn_plan::PlanStep>>>,
    announce: Arc<dyn Fn(ChatEvent) + Send + Sync>,
) -> crate::turn_plan::PlanSink {
    Arc::new(move |steps: &[crate::turn_plan::PlanStep]| {
        *held.lock().unwrap_or_else(|e| e.into_inner()) = steps.to_vec();
        announce(ChatEvent::Plan { steps: steps.to_vec() });
    })
}

/// The mailbox for a chat turn: hand the run whatever the person typed while it
/// was going, at a boundary where doing so is safe.
///
/// **`event.next == "agent"` is the whole rule.** The ReAct graph alternates
/// `agent → tools → agent`, and a message appended anywhere else lands between a
/// tool call and its result — the orphaned-call history the Responses API
/// rejects with an opaque 400. Waiting for the agent node means every tool
/// result from the batch has already been appended, so the history is coherent.
///
/// Messages that arrive too late to catch a boundary stay in the queue and are
/// picked up by [`drain_queued_turns`] once the turn ends, so nothing is lost by
/// being strict here.
fn chat_mailbox(
    pending: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    announce: Arc<dyn Fn(ChatEvent) + Send + Sync>,
) -> metalcraft::Mailbox<AgentState> {
    Arc::new(move |_state: &AgentState, event: &metalcraft::StepEvent| {
        if event.next != "agent" {
            return Vec::new();
        }
        let taken: Vec<String> = { lock_pending(&pending).drain(..).collect() };
        taken
            .into_iter()
            .map(|message| {
                announce(ChatEvent::Injected {
                    message: message.clone(),
                });
                metalcraft::AgentUpdate::UserMessage(message)
            })
            .collect()
    })
}

/// Take the sync queue lock, recovering from a poisoned mutex.
///
/// A panic elsewhere must not strand a chat unable to accept messages; the worst
/// case of reading a poisoned queue is a message ordering nobody notices.
fn lock_pending(
    pending: &std::sync::Mutex<std::collections::VecDeque<String>>,
) -> std::sync::MutexGuard<'_, std::collections::VecDeque<String>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take the next queued message and claim the turn, or say why not.
///
/// Both halves have to happen under one lock or two drains racing each other
/// both pop, and the same conversation runs twice from two different states.
/// Split out from the lock so the rule is testable without an ambient chat: the
/// case that matters is the one that only shows up under contention.
fn claim_next_queued(
    busy: &mut bool,
    pending: &mut std::collections::VecDeque<String>,
) -> Option<String> {
    if *busy {
        return None;
    }
    let message = pending.pop_front()?;
    *busy = true;
    Some(message)
}

/// Put a message in line and report where it landed. 1 is next to run.
fn queue_message(pending: &mut std::collections::VecDeque<String>, message: String) -> usize {
    pending.push_back(message);
    pending.len()
}

pub async fn drain_queued_turns(context: &AgentRuntimeContext, chat_id: &str) {
    loop {
        let store = chat_store();
        let session = { store.lock().await.get(chat_id).cloned() };
        let Some(session) = session else { return };

        // Claim the turn and take the next message under one lock, so two
        // drains racing cannot both pop.
        let (
            message,
            persona_slug,
            model_name,
            cwd,
            agent_state,
            diagnostics,
            interrupt,
            // The same queue, kept past the lock: a message typed during *this*
            // drained turn should join it, not wait for yet another one.
            pending_for_mailbox,
            plan_for_sink,
        ) = {
            let mut s = session.lock().await;
            // Busy means a user turn started first; it drains the rest when it
            // finishes, so this one has nothing left to do. Empty means done.
            // The queue handle is cloned out first: `pending` is an Arc, so this
            // is a pointer copy, and it frees the borrow of `s` that taking the
            // sync lock inline would otherwise hold across `&mut s.busy`.
            let pending = s.pending.clone();
            let Some(message) = claim_next_queued(&mut s.busy, &mut lock_pending(&pending)) else {
                return;
            };

            s.interrupt.store(false, Ordering::Relaxed);
            let next_state = match s.state.take() {
                Some(prev) => prev.continue_with(message.clone()),
                None => AgentState::new(message.clone()),
            };
            (
                message,
                s.persona_slug.clone(),
                s.model_name.clone(),
                s.cwd.clone(),
                next_state,
                s.diagnostics.clone(),
                s.interrupt.clone(),
                pending,
                s.plan.clone(),
            )
        };
        let sender = chat_event_sender(chat_id).await;
        let reply_sink: crate::tools::ReplySink = {
            let sender = sender.clone();
            Arc::new(move |reply: crate::tools::ReplyEnvelope| {
                let sender = sender.clone();
                Box::pin(async move {
                    let _ = sender.send(ChatEvent::Reply {
                        content: reply.text,
                        awaiting_reply: reply.awaiting_reply,
                        options: reply.options,
                    });
                    Ok(())
                })
            })
        };
        let step_guard = stoppable(
            crate::guard::build_agent_guard(crate::guard::GuardConfig::default(), diagnostics.clone()),
            interrupt.clone(),
        );
        let llm_call_hook: Option<metalcraft::LlmCallHook> = diagnostics.as_ref().map(|d| {
            let logger = d.clone();
            Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
                logger.log_llm_request(snapshot);
            }) as metalcraft::LlmCallHook
        });

        let _ = sender.send(ChatEvent::TurnStarted {
            turn_index: 0,
            user_message: message.clone(),
            session_id: None,
        });

        let outcome = run_chat_turn(
            context,
            &persona_slug,
            &cwd,
            &model_name,
            Some(chat_id),
            agent_state,
            step_guard,
            llm_call_hook,
            None,
            RuntimeOptions {
                reply_sink: Some(reply_sink),
                tool_choice: metalcraft::ToolChoice::Required,
                terminal_tools: vec!["say_to_user".to_string(), "ask_user".to_string()],
                session_binding: Some(crate::scheduled_tasks::IoBinding::WorkshopChat {
                    chat_id: chat_id.to_string(),
                }),
                reschedule_depth: 0,
                prompt_extras: crate::persona::PromptExtras::load().await,
                preset_personas: None,
                instance_id: None,
                interrupt: Some(interrupt.clone()),
                plan_sink: Some(plan_sink(plan_for_sink.clone(), {
                    let sender = sender.clone();
                    Arc::new(move |ev| {
                        let _ = sender.send(ev);
                    })
                })),
            },
            Some({
                let sender = sender.clone();
                Arc::new(move |phase: &str| {
                    let _ = sender.send(ChatEvent::Phase {
                        phase: phase.to_string(),
                    });
                })
            }),
        chat_mailbox(pending_for_mailbox.clone(), {
            let sender = sender.clone();
            Arc::new(move |ev| {
                let _ = sender.send(ev);
            })
        })
        .into(),
        )
        .await;

        {
            let mut s = session.lock().await;
            if let Ok(RunOutcome::Completed(state)) = outcome {
                s.state = Some(state);
            }
            s.busy = false;
        }
        persist_chat(&session).await;
        let _ = sender.send(ChatEvent::Done {
            status: "completed".into(),
            reason: None,
        });
    }
}

/// Outcome of trying to deliver a fired follow-up into its chat.
pub enum FollowupDelivery {
    /// The follow-up ran and its reply was delivered/persisted.
    Delivered,
    /// The chat was mid-turn — the caller should requeue and retry shortly.
    ChatBusy,
    /// No such chat (deleted, or never existed in this process).
    ChatMissing,
}

/// Run a fired follow-up as a real turn on its Workshop chat, then persist it
/// and publish the reply on the chat's live event bus. Reuses the same
/// `run_chat_turn` machinery as a user turn, so the follow-up continues the
/// conversation (same persona + accumulated state) and its `say_to_user` reply
/// is recorded in the chat like any other assistant message — visible on reopen
/// and streamed live to any open subscriber.
pub async fn deliver_followup_to_chat(
    context: &AgentRuntimeContext,
    chat_id: &str,
    task: &str,
) -> FollowupDelivery {
    let store = chat_store();
    let session = { store.lock().await.get(chat_id).cloned() };
    let Some(session) = session else {
        return FollowupDelivery::ChatMissing;
    };

    // Snapshot what we need and stamp the session busy, refusing to run
    // concurrently with a user turn.
    let (persona_slug, model_name, cwd, agent_state, diagnostics, interrupt, plan_for_sink) = {
        let mut s = session.lock().await;
        if s.busy {
            return FollowupDelivery::ChatBusy;
        }
        s.busy = true;
        // Start clean, exactly as a user turn does: a stop pressed on the last
        // turn must not kill this one before it takes a step.
        s.interrupt.store(false, Ordering::Relaxed);
        let next_state = match s.state.take() {
            Some(prev) => prev.continue_with(task.to_string()),
            None => AgentState::new(task.to_string()),
        };
        (
            s.persona_slug.clone(),
            s.model_name.clone(),
            s.cwd.clone(),
            next_state,
            s.diagnostics.clone(),
            s.interrupt.clone(),
            s.plan.clone(),
        )
    };

    // Reply sink: publish the agent's say_to_user text to the chat's live bus.
    let sender = chat_event_sender(chat_id).await;
    let reply_sink: crate::tools::ReplySink = {
        let sender = sender.clone();
        Arc::new(move |reply: crate::tools::ReplyEnvelope| {
            let sender = sender.clone();
            Box::pin(async move {
                // A send error just means no live subscriber; the reply is still
                // persisted below, so that's not a failure.
                let _ = sender.send(ChatEvent::Reply {
                    content: reply.text,
                    awaiting_reply: reply.awaiting_reply,
                    options: reply.options,
                });
                Ok(())
            })
        })
    };

    let step_guard = stoppable(
        crate::guard::build_agent_guard(crate::guard::GuardConfig::default(), diagnostics.clone()),
        interrupt.clone(),
    );
    let llm_call_hook: Option<metalcraft::LlmCallHook> = diagnostics.as_ref().map(|d| {
        let logger = d.clone();
        Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
            logger.log_llm_request(snapshot);
        }) as metalcraft::LlmCallHook
    });

    let _ = sender.send(ChatEvent::TurnStarted {
        turn_index: 0,
        user_message: format!("⏰ scheduled follow-up: {task}"),
        session_id: None,
    });

    let outcome = run_chat_turn(
        context,
        &persona_slug,
        &cwd,
        &model_name,
        Some(chat_id),
        agent_state,
        step_guard,
        llm_call_hook,
        None,
        RuntimeOptions {
            reply_sink: Some(reply_sink),
            tool_choice: metalcraft::ToolChoice::Required,
            terminal_tools: vec!["say_to_user".to_string(), "ask_user".to_string()],
            session_binding: Some(crate::scheduled_tasks::IoBinding::WorkshopChat {
                chat_id: chat_id.to_string(),
            }),
            // A follow-up may schedule one more; the tool caps the chain depth.
            reschedule_depth: 0,
            prompt_extras: crate::persona::PromptExtras::load().await,
            preset_personas: None,
            instance_id: None,
            interrupt: Some(interrupt.clone()),
            plan_sink: Some(plan_sink(plan_for_sink.clone(), {
                let sender = sender.clone();
                Arc::new(move |ev| {
                    let _ = sender.send(ev);
                })
            })),
        },
        // A follow-up fires with nobody necessarily watching, which is exactly
        // when a silent four-minute compaction is hardest to explain later.
        Some({
            let sender = sender.clone();
            Arc::new(move |phase: &str| {
                let _ = sender.send(ChatEvent::Phase {
                    phase: phase.to_string(),
                });
            })
        }),
        // A follow-up fires on its own; anything typed while it runs is
        // picked up by the drain afterwards.
        None,
    )
    .await;

    // Write the resulting state back and clear busy, then persist + signal done.
    {
        let mut s = session.lock().await;
        match outcome {
            Ok(RunOutcome::Completed(state)) => s.state = Some(state),
            // On a non-completion, keep whatever partial state came back if any;
            // otherwise the pre-turn state was already taken, so leave None.
            _ => {}
        }
        s.busy = false;
    }
    persist_chat(&session).await;
    let _ = sender.send(ChatEvent::Done {
        status: "completed".into(),
        reason: None,
    });

    // A follow-up firing at 3am is exactly when someone might be typing. Drain
    // here too, or their message waits for the *next* turn from any source.
    drain_queued_turns(context, chat_id).await;

    FollowupDelivery::Delivered
}

#[allow(clippy::too_many_arguments)]
async fn run_chat_turn(
    context: &AgentRuntimeContext,
    persona_slug: &str,
    cwd: &str,
    model_name: &str,
    // Which conversation this turn belongs to, so captured material can be
    // grouped into an episode later. `None` for turns with no chat.
    chat_id: Option<&str>,
    initial_state: AgentState,
    step_guard: StepGuard<AgentState>,
    llm_call_hook: Option<metalcraft::LlmCallHook>,
    llm_response_hook: Option<metalcraft::LlmResponseHook>,
    options: crate::runtime::RuntimeOptions,
    // Where to announce compaction and recall. `None` for a turn nobody is
    // watching live — a gateway turn answers over its adapter, not over frames.
    phase_sink: Option<crate::runtime::PhaseSink>,
    // Messages sent while this turn runs. `None` for a turn nobody is talking
    // to as it happens — a fired follow-up delivers to a queue-drain instead.
    mailbox: Option<metalcraft::Mailbox<AgentState>>,
) -> Result<RunOutcome<AgentState>, Box<dyn std::error::Error + Send + Sync>> {
    use crate::runtime::build_agent_runtime;
    use rig::client::CompletionClient;

    // Which agent is this? Resolved from the conversation rather than plumbed
    // through every caller — the chat record already names its instance, and every
    // turn path funnels through here.
    let instance_id = match (&options.instance_id, chat_id) {
        (Some(id), _) => Some(id.clone()),
        (None, Some(cid)) => {
            let store = chat_store();
            let session = { store.lock().await.get(cid).cloned() };
            match session {
                Some(s) => s.lock().await.instance_id.clone(),
                None => None,
            }
        }
        _ => None,
    };

    let persona = Persona::load(persona_slug, &context.personas_dir)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    let runtime = build_agent_runtime(
        context,
        &persona,
        cwd,
        model_name,
        ApprovalMode::AutoApprove,
        llm_call_hook,
        llm_response_hook,
        options,
        |client, name| client.completion_model(name),
    )
    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;

    // Run the turn through the shared [`TurnRunner`], which compacts the context
    // to fit the window before executing. Workshop and gateway sessions
    // accumulate history indefinitely (`AgentState::continue_with` only appends),
    // so without compaction a long-running chat — e.g. someone who keeps
    // messaging the WhatsApp gateway — would eventually exceed the provider's
    // context limit and every further turn would fail. Both daemon entry points
    // funnel through here; the CLI and one-shot paths share the same primitive.
    // The daemon ignores the "did it compact" flag and relies on the log line.
    let (_compacted, outcome) = crate::runtime::TurnRunner::new(runtime)
        .with_capture_context(chat_id.map(str::to_string), Some(persona_slug.to_string()))
        .with_instance(instance_id)
        .with_phase_sink(phase_sink)
        .with_mailbox(mailbox)
        .run(initial_state, step_guard)
        .await;
    // Box the real error rather than stringifying it, so its `source()` chain
    // survives for `error_chain` to walk when building the failed-turn reason.
    outcome.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
}

/// Render an error with its full `source()` chain (see the same helper in
/// `metalcraft::prebuilt`). Lower-level causes — a reqwest decode error's
/// serde detail, a provider's error payload — live in `source()`, not the
/// top-level `Display`, so `to_string()` alone would discard them.
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let s = cause.to_string();
        if !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        source = cause.source();
    }
    out
}

// `Stream` is brought into scope via `futures_util::stream::StreamExt` for `.map`.
use futures_util::StreamExt;
#[allow(dead_code)]
fn _stream_trait_in_scope<T: Stream<Item = ()>>(_: T) {}

// ── Integration handlers ───────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
struct IntegrationSummary {
    id: String,
    name: String,
    description: String,
    version: String,
    enabled: bool,
    /// Number of personas/skills/api_tools/flow_templates the pack provides.
    personas: usize,
    skills: usize,
    api_tools: usize,
    flow_templates: usize,
    #[serde(default)]
    requires_env: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
struct IntegrationDetail {
    id: String,
    name: String,
    description: String,
    version: String,
    enabled: bool,
    #[serde(default)]
    requires_env: Vec<String>,
    personas: Vec<String>,
    skills: Vec<String>,
    api_tools: Vec<String>,
    flow_templates: Vec<String>,
}

fn count_files(dir: &std::path::Path, ext: &str) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext))
        .count()
}

fn list_file_stems(dir: &std::path::Path, ext: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some(ext) {
                return None;
            }
            Some(p.file_stem()?.to_str()?.to_string())
        })
        .collect();
    out.sort();
    out
}

#[utoipa::path(
    get,
    path = "/api/v1/integrations",
    tag = "integrations",
    responses((status = 200, body = Vec<IntegrationSummary>)),
)]
async fn list_integrations() -> Json<Vec<IntegrationSummary>> {
    let packs = crate::integrations::list_installed();
    let summaries = packs
        .into_iter()
        .map(|p| IntegrationSummary {
            // Installed *is* available; see `integrations::is_enabled`. Kept in the
            // wire shape so existing clients keep parsing it.
            enabled: true,
            personas: count_files(&p.personas_dir(), "json"),
            skills: count_files(&p.skills_dir(), "md"),
            // Declarative HTTP-API tools (api_tools/*.json) plus any native Rust
            // tools the pack contributes (e.g. the s3 pack's S3 tools,
            // which ship no api_tools/ files). See `tools::native_integration_tool_names`.
            api_tools: count_files(&p.api_tools_dir(), "json")
                + crate::tools::native_integration_tool_names(&p.manifest.id).len(),
            flow_templates: count_files(&p.flow_templates_dir(), "json"),
            id: p.manifest.id,
            name: p.manifest.name,
            description: p.manifest.description,
            version: p.manifest.version,
            requires_env: p.manifest.requires_env,
        })
        .collect();
    Json(summaries)
}

#[utoipa::path(
    get,
    path = "/api/v1/integrations/{id}",
    tag = "integrations",
    params(("id" = String, Path, description = "Pack id")),
    responses((status = 200, body = IntegrationDetail), (status = 404, body = ErrorResponse)),
)]
async fn get_integration(Path(id): Path<String>) -> Response {
    let Some(pack) = crate::integrations::list_installed()
        .into_iter()
        .find(|p| p.manifest.id == id)
    else {
        return err_json(StatusCode::NOT_FOUND, format!("pack '{id}' not found"));
    };
    let enabled = crate::integrations::is_enabled(&id);
    // Read file lists before moving the manifest fields out of `pack`.
    let personas = list_file_stems(&pack.personas_dir(), "json");
    let skills = list_file_stems(&pack.skills_dir(), "md");
    // Declarative HTTP-API tools plus any native Rust tools the pack ships (see
    // the summary builder above and `tools::native_integration_tool_names`).
    let mut api_tools = list_file_stems(&pack.api_tools_dir(), "json");
    api_tools.extend(crate::tools::native_integration_tool_names(&id));
    api_tools.sort();
    let flow_templates = list_file_stems(&pack.flow_templates_dir(), "json");
    Json(IntegrationDetail {
        id: pack.manifest.id,
        name: pack.manifest.name,
        description: pack.manifest.description,
        version: pack.manifest.version,
        enabled,
        requires_env: pack.manifest.requires_env,
        personas,
        skills,
        api_tools,
        flow_templates,
    })
    .into_response()
}

/// Kept only so the retired endpoint below still documents the body clients used
/// to send; the value is ignored.
#[derive(Deserialize, utoipa::ToSchema)]
struct SetEnabledRequest {
    #[allow(dead_code)]
    enabled: bool,
}

/// Retired. An integration is no longer independently enabled or disabled —
/// an agent pack is the install unit, and the packs it vendors are simply present
/// (see `docs/AGENT_PACKS_PLAN.md`).
///
/// This answers 410 rather than quietly succeeding: a toggle that returns 204 and
/// changes nothing is worse than one that says it is gone, because the UI would go
/// on showing a state the runtime does not honour.
#[utoipa::path(
    put,
    path = "/api/v1/integrations/{id}/enabled",
    tag = "integrations",
    params(("id" = String, Path, description = "Pack id")),
    request_body = SetEnabledRequest,
    responses((status = 410, description = "Retired — uninstall the agent pack instead", body = ErrorResponse)),
)]
async fn put_integration_enabled(
    Path(id): Path<String>,
    Json(_req): Json<SetEnabledRequest>,
) -> Response {
    err_json(
        StatusCode::GONE,
        format!(
            "integrations are no longer enabled or disabled individually; '{id}' \
             is available because an installed agent pack provides it. Uninstall \
             that agent pack to remove it."
        ),
    )
}

// ── Lockfile (reproducible install manifest) ────────────────────────────

/// The pod's `metalcraft.lock` — every registry pack/flow pinned to an exact
/// version + content hash. Export it to clone a pod's toolset, or feed it to
/// `/lockfile/restore` on a rebuilt pod.
#[utoipa::path(
    get,
    path = "/api/v1/lockfile",
    tag = "lockfile",
    responses((status = 200, description = "The install lockfile", body = crate::lockfile::Lock)),
)]
async fn get_lockfile() -> Response {
    Json(crate::lockfile::load()).into_response()
}

#[derive(Serialize, utoipa::ToSchema)]
struct RestoreOutcome {
    kind: &'static str,
    name: String,
    version: String,
    /// `installed` | `failed`.
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// Wrapper for the lockfile-restore response (`{ outcomes: [...] }`).
#[derive(Serialize, utoipa::ToSchema)]
struct RestoreResult {
    outcomes: Vec<RestoreOutcome>,
}

/// Reinstall everything in the lockfile at its pinned version, verifying each against
/// its locked content hash — the reproducible-rebuild path for a fresh or cloned pod.
/// Returns one outcome per entry; a failure on one entry doesn't stop the rest.
#[utoipa::path(
    post,
    path = "/api/v1/lockfile/restore",
    tag = "lockfile",
    responses((status = 200, description = "Per-entry restore outcomes", body = RestoreResult)),
)]
async fn post_lockfile_restore() -> Response {
    let lock = crate::lockfile::load();
    let mut outcomes: Vec<RestoreOutcome> = Vec::new();
    for e in &lock.packs {
        outcomes.push(restore_pack(e));
    }
    for e in &lock.flows {
        outcomes.push(restore_flow(e).await);
    }
    Json(RestoreResult { outcomes }).into_response()
}

fn restore_pack(e: &crate::lockfile::LockEntry) -> RestoreOutcome {
    let done = |status: &'static str, detail: Option<String>| RestoreOutcome {
        kind: "pack",
        name: e.name.clone(),
        version: e.version.clone(),
        status,
        detail,
    };
    // A pack entry can only be a leftover: nothing has recorded one since the
    // integration registry stopped being an install path, and restoring it would
    // mean fetching a zip and writing it somewhere no resolver reads any more.
    //
    // Reported rather than dropped. Somebody's lockfile still lists these, and a
    // rebuilt pod that silently came up without them would look complete.
    done(
        "skipped",
        Some(format!(
            "'{}' is a legacy integration-registry pack; install an agent pack that vendors \
             it instead",
            e.name
        )),
    )
}

async fn restore_flow(e: &crate::lockfile::LockEntry) -> RestoreOutcome {
    let done = |status: &'static str, detail: Option<String>| RestoreOutcome {
        kind: "flow",
        name: e.name.clone(),
        version: e.version.clone(),
        status,
        detail,
    };
    let bytes = match crate::registry::fetch_flow_bytes(&e.name, Some(&e.version)).await {
        Ok(b) => b,
        Err(err) => return done("failed", Some(err)),
    };
    // Verify integrity against the locked hash before trusting the bytes.
    if crate::lockfile::sha256_hex(&bytes) != e.content_sha256 {
        return done(
            "failed",
            Some("content hash does not match the locked hash".into()),
        );
    }
    let flow: metalcraft_flows::SavedFlow = match serde_json::from_slice(&bytes) {
        Ok(f) => f,
        Err(err) => return done("failed", Some(format!("invalid flow document: {err}"))),
    };
    let errors = metalcraft_flows::validate(&flow);
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return done("failed", Some(format!("flow failed validation: {msg}")));
    }
    match metalcraft_flows::save_flow(&paths::flows_dir(), &flow) {
        Ok(()) => done("installed", None),
        Err(err) => done("failed", Some(err.to_string())),
    }
}

// ── Gateway channel handlers ────────────────────────────────────────────
//
// The gateway link (metalcraft connect/status) lives below; channel CRUD is the
// `/api/v1/channels` endpoints. Inbound messages flow through the unauthenticated
// webhook handlers further below.

/// Recent gateway activity across all channels, including inbound messages that
/// matched no channel (the global Network view). Newest first.
#[utoipa::path(
    get,
    path = "/api/v1/gateway/activity",
    tag = "gateway",
    responses((status = 200, body = Vec<crate::gateway_activity::GatewayEvent>)),
)]
async fn list_gateway_activity() -> Json<Vec<crate::gateway_activity::GatewayEvent>> {
    Json(crate::gateway_activity::list(None, 300))
}

// ── Channels: the simple {slug, name, url, secret} connection model ───────────

/// List all channels — the built-in `metalcraft` default first, then custom
/// channels. Secrets are never included.
#[utoipa::path(
    get,
    path = "/api/v1/channels",
    tag = "gateway",
    responses((status = 200, body = Vec<crate::channels::Channel>)),
)]
async fn list_channels() -> Json<Vec<crate::channels::Channel>> {
    Json(crate::channels::list_channels())
}

/// Recent activity for a single channel (by slug), newest first.
#[utoipa::path(
    get,
    path = "/api/v1/channels/{slug}/events",
    tag = "gateway",
    params(("slug" = String, Path, description = "Channel slug")),
    responses((status = 200, body = Vec<crate::gateway_activity::GatewayEvent>)),
)]
async fn list_channel_events(
    Path(slug): Path<String>,
) -> Json<Vec<crate::gateway_activity::GatewayEvent>> {
    Json(crate::gateway_activity::list(Some(&slug), 200))
}

#[derive(Deserialize, utoipa::ToSchema)]
struct CreateChannelRequest {
    name: String,
    url: String,
    secret: String,
    #[serde(default)]
    slug: Option<String>,
}

/// Add a custom channel (its own gateway url + secret). The `metalcraft` slug is
/// reserved for the built-in channel.
#[utoipa::path(
    post,
    path = "/api/v1/channels",
    tag = "gateway",
    request_body = CreateChannelRequest,
    responses((status = 201, body = crate::channels::Channel), (status = 400, body = ErrorResponse)),
)]
async fn create_channel(Json(req): Json<CreateChannelRequest>) -> Response {
    match crate::channels::create_channel(&req.name, &req.url, &req.secret, req.slug.as_deref()) {
        Ok(ch) => (StatusCode::CREATED, Json(ch)).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct UpdateChannelRequest {
    name: String,
    url: String,
    #[serde(default = "crate::channels::default_enabled")]
    enabled: bool,
    /// New secret; omit or leave empty to keep the existing one.
    #[serde(default)]
    secret: Option<String>,
}

/// Update a custom channel. The built-in `metalcraft` channel can't be edited.
#[utoipa::path(
    put,
    path = "/api/v1/channels/{slug}",
    tag = "gateway",
    params(("slug" = String, Path, description = "Channel slug")),
    request_body = UpdateChannelRequest,
    responses((status = 200, body = crate::channels::Channel), (status = 400, body = ErrorResponse)),
)]
async fn update_channel(
    Path(slug): Path<String>,
    Json(req): Json<UpdateChannelRequest>,
) -> Response {
    match crate::channels::update_channel(
        &slug,
        &req.name,
        &req.url,
        req.enabled,
        req.secret.as_deref(),
    ) {
        Ok(ch) => Json(ch).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

/// Delete a custom channel and its secret. The built-in `metalcraft` channel
/// can't be deleted.
#[utoipa::path(
    delete,
    path = "/api/v1/channels/{slug}",
    tag = "gateway",
    params(("slug" = String, Path, description = "Channel slug")),
    responses((status = 200, description = "Deleted"), (status = 400, body = ErrorResponse)),
)]
async fn delete_channel(Path(slug): Path<String>) -> Response {
    match crate::channels::delete_channel(&slug) {
        Ok(removed) => Json(serde_json::json!({ "deleted": removed })).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

// ── Metalcraft Gateway: zero-copy connect ────────────────────────────────────

/// Registration/verification/connection state for the workshop's Connect panel.
#[utoipa::path(
    get,
    path = "/api/v1/gateway/metalcraft/status",
    tag = "gateway",
    responses((status = 200, body = crate::metalcraft_gateway::GatewayStatus)),
)]
async fn gateway_metalcraft_status() -> Json<crate::metalcraft_gateway::GatewayStatus> {
    Json(crate::metalcraft_gateway::status().await)
}

#[derive(Deserialize, utoipa::ToSchema)]
struct MgRegisterRequest {
    phone_number: String,
}

/// Inline register: proxy to the gateway with the pod's token; returns `verify_code`.
#[utoipa::path(
    post,
    path = "/api/v1/gateway/metalcraft/register",
    tag = "gateway",
    request_body = MgRegisterRequest,
    responses((status = 200, description = "Registered")),
)]
async fn gateway_metalcraft_register(Json(req): Json<MgRegisterRequest>) -> Response {
    match crate::metalcraft_gateway::register(&req.phone_number).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct MgConnectRequest {
    /// Override for the pod's public URL when `POD_PUBLIC_URL` isn't injected.
    #[serde(default)]
    webhook_base: Option<String>,
    /// Audience-scoped connection token from the k3 broker, adopted as the
    /// channel's outbound API key. Omitted on the Workshop path.
    #[serde(default)]
    connection_token: Option<String>,
}

/// Connect: fetch config + wire the webhook + enable the channel. 409 until verified.
#[utoipa::path(
    post,
    path = "/api/v1/gateway/metalcraft/connect",
    tag = "gateway",
    request_body = MgConnectRequest,
    responses((status = 200, description = "Connected")),
)]
async fn gateway_metalcraft_connect(Json(req): Json<MgConnectRequest>) -> Response {
    match crate::metalcraft_gateway::connect(req.webhook_base, req.connection_token).await {
        Ok(r) => Json(r).into_response(),
        Err(e) if e == crate::metalcraft_gateway::VERIFY_REQUIRED => err_json(
            StatusCode::CONFLICT,
            "Register and verify your phone number before connecting",
        ),
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

/// Disconnect: disable the metalcraft-gateway channel + drop its secrets. Idempotent.
#[utoipa::path(
    post,
    path = "/api/v1/gateway/metalcraft/disconnect",
    tag = "gateway",
    responses((status = 200, description = "Disconnected")),
)]
async fn gateway_metalcraft_disconnect() -> Response {
    match crate::metalcraft_gateway::disconnect().await {
        Ok(()) => Json(serde_json::json!({ "connected": false })).into_response(),
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

/// Unregister: give the number back at the gateway, then disconnect locally.
///
/// The exit that `disconnect` is not. Disconnecting stops this pod receiving and
/// leaves the account's registration standing — number still bound and verified,
/// dedicated number still out of the pool, managed integration still routing to
/// a consumer that left. This is the one that ends all of that, and until it
/// existed only a client holding the account PAT could do it, which excludes
/// every client that reaches the account through its pod.
#[utoipa::path(
    post,
    path = "/api/v1/gateway/metalcraft/unregister",
    tag = "gateway",
    responses((status = 200, description = "Unregistered"), (status = 502, body = ErrorResponse)),
)]
async fn gateway_metalcraft_unregister() -> Response {
    match crate::metalcraft_gateway::unregister().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

// ── Factory reset ────────────────────────────────────────────────────────
//
// The destructive one. See [`crate::factory_reset`] for why it wipes the data
// directory rather than any feature's idea of "my content", and why it exits
// the process afterwards instead of clearing caches in place.

/// Body for `POST /api/v1/factory-reset`.
#[derive(Deserialize, utoipa::ToSchema)]
struct FactoryResetRequest {
    /// Must be exactly `FACTORY RESET`. A typed phrase rather than a flag, so
    /// that nothing reaches this endpoint by accident — see
    /// [`crate::factory_reset::CONFIRM_PHRASE`].
    confirm: String,
    /// Defaults to [`ResetScope::Full`], the scope that actually reproduces a
    /// new pod. `keep_keys` is the convenience option and the weaker test.
    #[serde(default)]
    scope: ResetScope,
}

/// Erase this pod and restart it as if it had just been provisioned.
///
/// Answers **before** it exits, and the report it returns is the last word this
/// process says — anything the client wants to know about what was removed has
/// to come from here, because the next thing to answer on this port will be a
/// pod with no history of the request.
#[utoipa::path(
    post,
    path = "/api/v1/factory-reset",
    tag = "admin",
    request_body = FactoryResetRequest,
    responses(
        (status = 200, description = "Wiped; the pod is restarting", body = ResetReport),
        (status = 400, description = "Confirmation phrase missing or wrong", body = ErrorResponse),
        (status = 500, description = "Could not read the data directory; nothing was removed", body = ErrorResponse),
    ),
)]
async fn post_factory_reset(Json(req): Json<FactoryResetRequest>) -> Response {
    if req.confirm != crate::factory_reset::CONFIRM_PHRASE {
        return err_json(
            StatusCode::BAD_REQUEST,
            format!(
                "factory reset requires confirm: \"{}\"",
                crate::factory_reset::CONFIRM_PHRASE
            ),
        );
    }

    let report = match crate::factory_reset::wipe(req.scope) {
        Ok(r) => r,
        // Nothing was removed — the directory could not even be listed. Leave
        // the pod running; there is nothing to restart into.
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("factory reset could not read the data dir: {e}"),
            );
        }
    };

    // A partial wipe still restarts. The alternative is a pod left half-erased
    // *and* running on stale in-memory state, which is the worst of both; the
    // report carries `failed` so the operator knows not to trust what comes
    // back as factory-fresh.
    if !report.is_clean() {
        log::error!(
            "factory reset left {} entr{} behind: {:?}",
            report.failed.len(),
            if report.failed.len() == 1 { "y" } else { "ies" },
            report.failed,
        );
    }

    // Long enough for this response to be written and read; short enough that
    // nothing meaningful can be persisted from a stale cache in the gap.
    crate::factory_reset::seed_and_exit(std::time::Duration::from_millis(750));

    Json(report).into_response()
}

// ── Inbound gateway webhooks ─────────────────────────────────────────────

/// Cap concurrent agent runs triggered by inbound webhooks so a burst of
/// messages can't spawn unbounded tasks.
const MAX_WEBHOOK_TASKS: usize = 4;

fn webhook_semaphore() -> &'static std::sync::Arc<tokio::sync::Semaphore> {
    static SEM: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    SEM.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_WEBHOOK_TASKS)))
}

/// Whether inbound webhooks may be accepted without a verified signature.
/// Off by default — webhooks are **fail-closed**: a missing signing secret means
/// requests are rejected. Set `GATEWAY_ALLOW_UNSIGNED=1` to bypass for **local
/// testing only**.
fn allow_unsigned_webhooks() -> bool {
    matches!(
        std::env::var("GATEWAY_ALLOW_UNSIGNED").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Whether to run the legacy Inbound Pull long-poll. Off by default — inbound now arrives
/// via the gateway's push to `/webhook/gateway`. Set `GATEWAY_INBOUND_PULL=1` to
/// re-enable pulling for a dual-transport rollout bake.
fn gateway_inbound_pull_enabled() -> bool {
    matches!(
        std::env::var("GATEWAY_INBOUND_PULL").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Inbound gateway webhook (JSON `message.created`). Routes the message to
/// the enabled channel whose `integration_id` matches the payload `source_id`,
/// validates the `X-Metalcraft-Signature` (HMAC-SHA256 over the raw body)
/// against *that channel's* `WEBHOOK_SECRET`, then runs the channel's persona to
/// reply (via the channel's send). Returns 200 for accepted-but-unroutable cases
/// so the gateway doesn't retry.
async fn handle_gateway_webhook(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let Some(inbound) = crate::tools::gateway_webhook::parse_inbound(&payload) else {
        // Not an inbound message (log/status/outbound echo) — nothing to do.
        return StatusCode::OK;
    };
    let sig = headers
        .get("x-metalcraft-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    route_gateway_inbound(state, inbound, Some((body, sig))).await
}

/// Resolve the channel an inbound message routes to, by its gateway
/// `integration_id` (`source_id`). The `metalcraft` channel's link is populated
/// at boot (`migrate_instance_to_channel`) and on every connect/resync. `None`
/// when nothing matches.
fn resolve_inbound_channel(source_id: &str) -> Option<crate::channels::Channel> {
    crate::channels::resolve_by_integration(source_id)
}

/// Route + run one inbound `message.created`, shared by the unauthenticated
/// webhook (push) and the authenticated long-poll (pull, see [`inbound_pull_loop`]).
/// `verify = Some((body, sig))` enforces the per-channel HMAC for the webhook path;
/// `None` skips it because the pull transport already authenticated the pod via its
/// connection token.
async fn route_gateway_inbound(
    state: Arc<ApiState>,
    inbound: crate::tools::gateway_webhook::InboundMessage,
    verify: Option<(axum::body::Bytes, String)>,
) -> StatusCode {
    // Route on the gateway integration UUID (`source_id`) — stable and
    // unique, unlike phone-number matching — against each channel's
    // `integration_id` setting. Resolving the channel first lets us verify the
    // signature against *that channel's* webhook secret (secrets are per-channel
    // now, so there's no single global secret to check against up front).
    let Some(source_id) = inbound.source_id.clone() else {
        log::warn!("gateway webhook has no source_id; cannot route — ignoring");
        crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
            direction: "inbound".into(),
            platform: "text".into(),
            from: Some(inbound.from.clone()),
            from_name: inbound.from_name.clone(),
            body: crate::gateway_activity::truncate_body(&inbound.body),
            outcome: "no_matching_channel".into(),
            detail: Some("webhook had no source_id to route on".into()),
            ..Default::default()
        });
        return StatusCode::OK;
    };
    let Some(channel) = resolve_inbound_channel(&source_id) else {
        log::warn!("gateway webhook for integration '{source_id}' matched no channel — ignoring");
        crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
            direction: "inbound".into(),
            platform: "text".into(),
            from: Some(inbound.from.clone()),
            from_name: inbound.from_name.clone(),
            body: crate::gateway_activity::truncate_body(&inbound.body),
            source_id: Some(source_id.clone()),
            outcome: "no_matching_channel".into(),
            detail: Some(format!("no channel has integration_id = {source_id}")),
            ..Default::default()
        });
        return StatusCode::OK;
    };

    // Fail-closed: verify the signature with THIS channel's webhook secret — but only on
    // the webhook (push) path. The pull long-poll passes `verify = None` because the pod
    // authenticated the connection itself, so there is no HMAC to check.
    if let Some((body, signature)) = &verify {
        match crate::channels::webhook_secret(&channel.slug) {
            Some(secret) => {
                if !crate::tools::gateway_webhook::validate_signature(&secret, body, signature) {
                    log::warn!("Rejected gateway webhook: invalid or missing signature");
                    crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
                        direction: "inbound".into(),
                        platform: "text".into(),
                        source_id: Some(source_id.clone()),
                        channel_id: Some(channel.slug.clone()),
                        channel_name: Some(channel.name.clone()),
                        outcome: "signature_rejected".into(),
                        detail: Some("invalid or missing signature".into()),
                        ..Default::default()
                    });
                    // A rotated gateway secret is the usual cause — self-heal (rate-limited).
                    crate::metalcraft_gateway::maybe_reactive_resync();
                    return StatusCode::FORBIDDEN;
                }
            }
            None => {
                if !allow_unsigned_webhooks() {
                    log::warn!(
                        "Rejected gateway webhook: no WEBHOOK_SECRET configured for channel '{}'. \
                         Reconnect the gateway (or set GATEWAY_ALLOW_UNSIGNED=1 for local testing only).",
                        channel.name
                    );
                    return StatusCode::FORBIDDEN;
                }
                log::warn!(
                    "Accepting UNSIGNED gateway webhook because GATEWAY_ALLOW_UNSIGNED is set — \
                     do not use this in production."
                );
            }
        }
    }

    // Idempotency: the same inbound can reach us on both transports (the gateway's
    // `dual` mode delivers via push AND pull) or be re-delivered after a pod
    // restart (a long-poll pull that wasn't ACKed). Dedup on the gateway's message
    // UUID (falling back to the carrier SID) so the agent runs exactly once.
    // Checked after signature verification so a forged request can't poison the
    // window.
    let dedup_key = inbound
        .gateway_message_id
        .as_deref()
        .or(inbound.external_id.as_deref());
    if crate::inbound_dedup::is_duplicate(dedup_key) {
        log::info!(
            "duplicate inbound (id={}); skipping — already processed",
            dedup_key.unwrap_or("?")
        );
        crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
            direction: "inbound".into(),
            platform: "text".into(),
            from: Some(inbound.from.clone()),
            from_name: inbound.from_name.clone(),
            body: crate::gateway_activity::truncate_body(&inbound.body),
            source_id: Some(source_id.clone()),
            channel_id: Some(channel.slug.clone()),
            channel_name: Some(channel.name.clone()),
            outcome: "duplicate".into(),
            detail: Some("already processed (dedup)".into()),
            ..Default::default()
        });
        return StatusCode::OK;
    }

    // The orchestrator delegates to specialist personas as needed, so it's the
    // sensible default when a channel doesn't pin a specific persona.
    let persona_slug = channel
        .persona
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "orchestrator-agent".to_string());
    let model_name = channel
        .model
        .clone()
        .unwrap_or_else(crate::runtime::configured_default_model);

    crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
        direction: "inbound".into(),
        platform: "text".into(),
        from: Some(inbound.from.clone()),
        from_name: inbound.from_name.clone(),
        body: crate::gateway_activity::truncate_body(&inbound.body),
        source_id: Some(source_id.clone()),
        channel_id: Some(channel.slug.clone()),
        channel_name: Some(channel.name.clone()),
        outcome: "routed".into(),
        detail: Some(format!("persona {persona_slug}")),
        ..Default::default()
    });

    dispatch_inbound(
        state,
        NormalizedInbound {
            channel_slug: channel.slug.clone(),
            channel_name: channel.name.clone(),
            adapter: "gateway".into(),
            persona_slug,
            model_name,
            sender: inbound.from.clone(),
            sender_name: inbound.from_name.clone(),
            // Reply back through the same integration that received the message.
            from: Some(source_id.clone()),
            body: inbound.body.clone(),
            session_ttl_secs: None,
        },
    )
    .await
}

/// **Inbound Pull** long-poll client. While a `metalcraft-gateway` channel is enabled,
/// hold `GET {gateway}/api/v1/agent/inbound/next`, run each pulled inbound through the
/// same path as the webhook (`verify = None` — the connection is already authenticated),
/// then ACK it. Self-manages reconnect with backoff; a no-op while nothing is connected.
/// The held connection is the liveness signal surfaced as `GatewayStatus::streaming`.
/// See `metalcraft-gateway/docs/INBOUND_PULL_PLAN.md`.
async fn inbound_pull_loop(state: Arc<ApiState>) {
    use std::time::Duration;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90)) // > the server's max long-poll wait
        .user_agent("metalcraft-agent (inbound-pull)")
        .build()
        .unwrap_or_default();
    let mut backoff = 1u64;
    loop {
        let Some((base, bearer)) = crate::metalcraft_gateway::pull_target() else {
            // Not connected to the gateway — stay idle and re-check periodically.
            crate::metalcraft_gateway::set_streaming(false);
            tokio::time::sleep(Duration::from_secs(15)).await;
            continue;
        };
        let url = format!("{base}/api/v1/agent/inbound/next?wait=25");
        match client.get(&url).bearer_auth(&bearer).send().await {
            Ok(resp) if resp.status() == reqwest::StatusCode::NO_CONTENT => {
                // Idle timeout — connection is healthy, just nothing waiting.
                crate::metalcraft_gateway::set_streaming(true);
                backoff = 1;
            }
            Ok(resp) if resp.status().is_success() => {
                crate::metalcraft_gateway::set_streaming(true);
                backoff = 1;
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => {
                        let message_id = v
                            .get("message_id")
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                        if let Some(payload) = v.get("payload") {
                            if let Some(inbound) =
                                crate::tools::gateway_webhook::parse_inbound(payload)
                            {
                                let _ = route_gateway_inbound(state.clone(), inbound, None).await;
                            }
                        }
                        // ACK regardless of routing outcome: an unroutable message (no
                        // matching channel) must not wedge the queue — the webhook path
                        // also returns 200 for that case.
                        if let Some(id) = message_id {
                            ack_inbound(&client, &base, &bearer, &id).await;
                        }
                    }
                    Err(e) => log::warn!("inbound pull: malformed response body: {e}"),
                }
            }
            Ok(resp) => {
                // 401/403 (token stale — the heal loop refreshes it), 404 (no managed
                // integration yet), or 5xx. Back off; don't hot-loop.
                crate::metalcraft_gateway::set_streaming(false);
                log::warn!(
                    "inbound pull: gateway returned HTTP {}",
                    resp.status().as_u16()
                );
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
            Err(e) => {
                crate::metalcraft_gateway::set_streaming(false);
                log::warn!("inbound pull: request failed: {e}");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// ACK a pulled inbound so the gateway marks it delivered. Best-effort: a failed ACK just
/// means the message re-delivers on the next poll (the pod dedups via `external_id`).
async fn ack_inbound(client: &reqwest::Client, base: &str, bearer: &str, message_id: &str) {
    let url = format!("{base}/api/v1/agent/inbound/ack");
    if let Err(e) = client
        .post(&url)
        .bearer_auth(bearer)
        .json(&serde_json::json!({ "message_id": message_id }))
        .send()
        .await
    {
        log::warn!("inbound pull: ack failed for {message_id}: {e}");
    }
}

/// Shared tail for inbound webhook handlers: build the runtime context, acquire
/// a concurrency permit, and fire-and-forget a one-shot agent run that produces
/// (and sends) the reply. Returns the HTTP status the webhook should respond
/// with. `who` is a human label for logging.
/// A routed inbound gateway message, normalized across adapters. This is the
/// common shape both webhook handlers reduce to before handing off to the
/// shared [`dispatch_inbound`] — the same way `POST /turn` normalizes a workshop
/// message. The agent loop downstream is identical for every channel; only the
/// reply sink (built from these fields) differs.
struct NormalizedInbound {
    /// Channel the reply is sent through. `"gateway"` adapter → a channels-model
    /// slug (e.g. `"metalcraft"`); `"twilio"` adapter → a synthetic id.
    channel_slug: String,
    channel_name: String,
    /// Reply route: `"gateway"` (via `channels::send`) or `"twilio"`.
    adapter: String,
    persona_slug: String,
    model_name: String,
    /// The sender — the counterparty replies are sent back to.
    sender: String,
    sender_name: Option<String>,
    /// Outbound sender identity (integration id for the gateway, our number for
    /// Twilio). Passed to the sender.
    from: Option<String>,
    body: String,
    /// Idle window (seconds) after which a dormant conversation restarts fresh,
    /// resolved from the channel's `session_ttl_minutes` setting. `None` ⇒ use
    /// [`DEFAULT_GATEWAY_SESSION_TTL_SECS`]; `Some(0)` ⇒ never reset.
    session_ttl_secs: Option<u64>,
}

/// Stable key for one person on one channel — the identity a gateway
/// conversation belongs to, independent of how many sessions it has had.
///
/// Used both as the agent instance's `sender` and as the prefix every one of that
/// sender's session ids carries, so the sessions can be found again by id alone
/// after a restart. Filename- and URL-safe: phone numbers reduce to digits; other
/// ids keep ascii alphanumerics, so the same person formatted two different ways
/// ("+1 (555) 000-1234", "whatsapp:+15550001234") is still one key.
fn gateway_sender_key(channel_slug: &str, sender: &str) -> String {
    // Reduce a phone number to bare digits (dropping any `whatsapp:` prefix and
    // punctuation) for a stable, filename-safe suffix.
    let digits: String = sender
        .trim_start_matches("whatsapp:")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let suffix = if digits.is_empty() {
        let s: String = sender
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if s.is_empty() {
            "anon".to_string()
        } else {
            s.to_ascii_lowercase()
        }
    } else {
        digits
    };
    format!("gw-{channel_slug}-{suffix}")
}

/// A fresh session id for `sender_key`, ordered by when it started.
///
/// The timestamp is what makes "the newest session" answerable from the id alone,
/// which is how [`latest_gateway_session`] avoids opening every chat file on the
/// pod to answer it for each inbound message.
fn new_gateway_chat_id(sender_key: &str, started_at: i64) -> String {
    format!("{sender_key}-{started_at}")
}

/// This sender's most recent session, if they have one.
///
/// Reads only the keys of the chat store, which is loaded from disk at startup —
/// so it survives a restart, and it never takes a session lock while holding the
/// store's (the pod locks store-then-session everywhere, and reversing it here
/// would be the one place a deadlock could come from).
fn latest_gateway_session(chats: &HashMap<String, Arc<Mutex<ChatSession>>>, key: &str) -> Option<String> {
    let prefix = format!("{key}-");
    chats
        .keys()
        .filter_map(|id| {
            if id == key {
                // Written before sessions were per-conversation: one chat that
                // was meant to last forever. It sorts oldest so that any real
                // session supersedes it, and it is still adopted when it is all
                // this sender has.
                return Some((i64::MIN, id.clone()));
            }
            let started: i64 = id.strip_prefix(&prefix)?.parse().ok()?;
            Some((started, id.clone()))
        })
        .max_by_key(|(started, _)| *started)
        .map(|(_, id)| id)
}

/// Default idle window before a gateway conversation starts fresh, used when a
/// channel doesn't set its own `session_ttl_minutes`. 15 minutes.
///
/// After this much inactivity the next inbound starts a fresh session instead of
/// continuing the old one — matching the "new conversation after a gap" feel of
/// SMS/WhatsApp, and (usefully) shedding any stale pre-upgrade history a
/// reasoning model would otherwise reject. Per-channel `session_ttl_minutes` of
/// `0` disables the reset. Gateway-only; workshop chats never auto-reset.
const DEFAULT_GATEWAY_SESSION_TTL_SECS: u64 = 900;

/// Whether the gateway conversation `chat_id` has been idle longer than `ttl`.
/// Uses the persisted chat file's mtime — [`persist_chat`] rewrites it after every
/// turn, and it survives a pod restart on the PVC, so it is a reliable
/// last-activity clock without threading a timestamp through the session state.
fn gateway_session_is_stale(chat_id: &str, ttl: std::time::Duration) -> bool {
    let Ok(meta) = std::fs::metadata(chat_file_path(chat_id)) else {
        return false; // no file yet (brand-new session) — nothing to reset
    };
    meta.modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|idle| idle > ttl)
        .unwrap_or(false)
}

/// The tools whose output the user sees as a message rather than as a tool card.
fn is_reply_tool(name: &str) -> bool {
    matches!(name, "say_to_user" | "ask_user")
}

/// Flatten a reply envelope for a channel that can only carry text.
///
/// Options become a numbered list because a plain-text user answering "2" is
/// unambiguous in a way that echoing a phrase is not, and the numbering survives
/// SMS, WhatsApp, and anything else with no notion of a choice control.
fn render_for_text_channel(reply: &crate::tools::ReplyEnvelope) -> String {
    if reply.options.is_empty() {
        return reply.text.clone();
    }
    let choices = reply
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("{}. {o}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{}\n\n{choices}", reply.text)
}

/// Build the reply sink for a gateway session: the agent's message is sent back
/// out through the bound adapter to the original sender, and the send is logged
/// to the gateway activity feed (mirroring `gateway::record_outbound`).
///
/// A text channel has no way to render a question differently from an answer, so
/// `ask_user`'s options are appended to the body as a numbered list and the
/// `awaiting_reply` marker is dropped. The user replies as they would to any
/// other message, and the next inbound message continues the conversation —
/// which is exactly what `ask_user` needs.
fn gateway_reply_sink(
    adapter: String,
    recipient: String,
    from: Option<String>,
    channel_slug: String,
    channel_name: String,
) -> crate::tools::ReplySink {
    Arc::new(move |reply: crate::tools::ReplyEnvelope| {
        let adapter = adapter.clone();
        let recipient = recipient.clone();
        let from = from.clone();
        let channel_slug = channel_slug.clone();
        let channel_name = channel_name.clone();
        Box::pin(async move {
            let content = render_for_text_channel(&reply);
            let result = match adapter.as_str() {
                // Reply back out through the channel the message arrived on.
                "twilio" => {
                    crate::tools::twilio::send_whatsapp(&recipient, &content, from.as_deref()).await
                }
                _ => match crate::channels::resolve_channel(Some(&channel_slug)) {
                    Ok(ch) => {
                        crate::channels::send(&ch, &recipient, &content, None, from.as_deref())
                            .await
                    }
                    Err(e) => Err(e),
                },
            };
            let (outcome, detail) = match &result {
                Ok(_) => ("sent", None),
                Err(e) => ("send_failed", Some(e.clone())),
            };
            crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
                direction: "outbound".into(),
                platform: "text".into(),
                from: from.clone(),
                to: Some(recipient.clone()),
                body: crate::gateway_activity::truncate_body(&content),
                source_id: from.clone(),
                channel_id: Some(channel_slug.clone()),
                channel_name: Some(channel_name.clone()),
                outcome: outcome.into(),
                detail,
                ..Default::default()
            });
            result.map(|_| ())
        })
    })
}

/// Find an existing gateway chat for this sender, or create one (with a
/// diagnostics session) and persist it. Mirrors `post_create_chat` but keyed by
/// the deterministic gateway id and stamped with a `Gateway` preset.
/// Which session this inbound belongs to: the sender's most recent one, or a new
/// one when they have been quiet past the channel's TTL.
///
/// A gap does not end the *relationship*, so the agent — and everything it
/// remembers — carries across; it ends the conversation, and the next message
/// opens a new one under the same agent. That is where the fresh context comes
/// from: a new session simply has none, so there is nothing to clear.
///
/// TTL is per-channel (`session_ttl_minutes`); `0` keeps one session forever.
async fn gateway_session_for(
    state: &Arc<ApiState>,
    n: &NormalizedInbound,
    sender_key: &str,
) -> String {
    let latest = {
        let chats = state.chats.lock().await;
        latest_gateway_session(&chats, sender_key)
    };
    let ttl_secs = n
        .session_ttl_secs
        .unwrap_or(DEFAULT_GATEWAY_SESSION_TTL_SECS);
    let Some(latest) = latest else {
        return new_gateway_chat_id(sender_key, chrono::Utc::now().timestamp());
    };
    if ttl_secs == 0 || !gateway_session_is_stale(&latest, std::time::Duration::from_secs(ttl_secs))
    {
        return latest;
    }
    log::info!("Gateway {sender_key}: quiet for over {ttl_secs}s — starting a new session");
    // The one place the system can say "that conversation is over", so it is the
    // right place to let memory distill the episode instead of waiting for a
    // later gap to prove it. The memory belongs to the agent, not the session, so
    // the next conversation still starts knowing this person.
    crate::memory::capture::record_session_end(&latest);
    new_gateway_chat_id(sender_key, chrono::Utc::now().timestamp())
}

async fn get_or_create_gateway_session(
    state: &Arc<ApiState>,
    n: &NormalizedInbound,
    sender_key: &str,
    chat_id: &str,
) -> Arc<Mutex<ChatSession>> {
    {
        let chats = state.chats.lock().await;
        if let Some(existing) = chats.get(chat_id) {
            return existing.clone();
        }
    }
    // Bind this conversation to the sender's persistent agent first, so the
    // diagnostics session can record which agent it belongs to. The TTL ends a
    // *session*; the agent — and everything it remembers about this person —
    // carries across every session it has ever had with them.
    //
    // The channel's own agent, not whatever the pod defaults to. Hard-wiring
    // `DEFAULT_PRESET` here meant installing an agent pack and pointing a number at
    // it was not expressible: the channel answered as the default agent, and read
    // from a memory base that had never been built for it.
    let channel_preset = crate::channels::get_channel(&n.channel_slug)
        .and_then(|c| c.agent_preset)
        .unwrap_or_else(|| crate::agent_preset::DEFAULT_PRESET.to_string());
    let label = n.sender_name.clone().unwrap_or_else(|| n.sender.clone());
    let instance_id = match crate::agent_instance::for_gateway_sender(
        &n.channel_slug,
        sender_key,
        &label,
        &channel_preset,
    ) {
        Ok(i) => Some(i.id),
        Err(e) => {
            log::warn!(
                "gateway channel '{}': could not bind an agent instance for {}: {e}",
                n.channel_slug,
                n.sender
            );
            None
        }
    };
    let diagnostics = DiagnosticsLogger::new().ok().map(Arc::new);
    if let Some(logger) = &diagnostics {
        if let Ok(persona) = Persona::load(&n.persona_slug, &paths::personas_dir()) {
            let system_prompt = persona.build_system_prompt(&paths::skills_dir(), &state.cwd);
            logger.log_session_info(SessionInfo {
                persona_name: &persona.name,
                persona_slug: &n.persona_slug,
                model_name: &n.model_name,
                cwd: &state.cwd,
                system_prompt: &system_prompt,
                tools: &persona.resolved_tool_names(),
                skills: &persona.skills,
                auto_approve: true,
                flow_id: None,
                instance_id: instance_id.as_deref(),
            });
        }
    }
    let session = ChatSession {
        id: chat_id.to_string(),
        instance_id,
        persona_slug: n.persona_slug.clone(),
        model_name: n.model_name.clone(),
        cwd: state.cwd.clone(),
        preset: SessionPreset::Gateway {
            channel_slug: n.channel_slug.clone(),
            adapter: n.adapter.clone(),
            recipient: n.sender.clone(),
            from: n.from.clone(),
        },
        state: None,
        archived: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        diagnostics,
        trace: None,
        busy: false,
        interrupt: Arc::new(AtomicBool::new(false)),
        pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        plan: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let arc = Arc::new(Mutex::new(session));
    {
        let mut chats = state.chats.lock().await;
        // Re-check under the lock in case a concurrent inbound created it first.
        if let Some(existing) = chats.get(chat_id) {
            return existing.clone();
        }
        chats.insert(chat_id.to_string(), arc.clone());
    }
    persist_chat(&arc).await;
    arc
}

/// Run exactly one gateway turn against `session` with `body` as the user
/// message, logging to diagnostics and delivering the reply via `sink`. Writes
/// the resulting state back and persists. Headless analogue of `post_chat_turn`.
async fn run_one_gateway_turn(
    session: &Arc<Mutex<ChatSession>>,
    context: &AgentRuntimeContext,
    sink: crate::tools::ReplySink,
    body: String,
) {
    let (chat_id, persona_slug, model_name, cwd, agent_state, diagnostics, interrupt) = {
        let mut s = session.lock().await;
        let next_state = match s.state.take() {
            Some(prev) => prev.continue_with(body.clone()),
            None => AgentState::new(body.clone()),
        };
        // A gateway chat reloaded from disk has no diagnostics logger; recreate
        // it on the first turn so the Sessions pane resumes (like post_chat_turn).
        if s.diagnostics.is_none() {
            if let Some(logger) = DiagnosticsLogger::new().ok().map(Arc::new) {
                if let Ok(persona) = Persona::load(&s.persona_slug, &paths::personas_dir()) {
                    let system_prompt = persona.build_system_prompt(&paths::skills_dir(), &s.cwd);
                    logger.log_session_info(SessionInfo {
                        persona_name: &persona.name,
                        persona_slug: &s.persona_slug,
                        model_name: &s.model_name,
                        cwd: &s.cwd,
                        system_prompt: &system_prompt,
                        tools: &persona.resolved_tool_names(),
                        skills: &persona.skills,
                        auto_approve: true,
                        flow_id: None,
                        instance_id: s.instance_id.as_deref(),
                    });
                }
                s.diagnostics = Some(logger);
            }
        }
        s.interrupt.store(false, Ordering::Relaxed);
        (
            s.id.clone(),
            s.persona_slug.clone(),
            s.model_name.clone(),
            s.cwd.clone(),
            next_state,
            s.diagnostics.clone(),
            s.interrupt.clone(),
        )
    };

    let state_before_turn = agent_state.clone();

    let llm_call_hook: Option<metalcraft::LlmCallHook> = diagnostics.as_ref().map(|d| {
        let logger = d.clone();
        Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
            logger.log_llm_request(snapshot);
        }) as metalcraft::LlmCallHook
    });
    let step_guard = stoppable(
        crate::guard::build_agent_guard(crate::guard::GuardConfig::default(), diagnostics.clone()),
        interrupt.clone(),
    );

    let outcome = run_chat_turn(
        context,
        &persona_slug,
        &cwd,
        &model_name,
        Some(&chat_id),
        agent_state,
        step_guard,
        llm_call_hook,
        None,
        RuntimeOptions {
            reply_sink: Some(sink.clone()),
            tool_choice: metalcraft::ToolChoice::Required,
            terminal_tools: vec!["say_to_user".to_string(), "ask_user".to_string()],
            // Gateway follow-up delivery (rebuilding the adapter sink at fire
            // time from the channel binding) is wired in the delivery pass; for
            // now a follow-up armed in a gateway turn is unbound (logged).
            session_binding: None,
            reschedule_depth: 0,
            prompt_extras: crate::persona::PromptExtras::load().await,
            preset_personas: None,
            instance_id: None,
            interrupt: Some(interrupt.clone()),
            // Nothing renders a task list over SMS.
            plan_sink: None,
        },
        // This path emits no frames at all — a gateway turn answers over its
        // adapter — so there is nobody to tell.
        None,
        // A gateway sender who texts again mid-turn is handled by the
        // inbound queue, which starts a fresh turn — no mid-run injection here.
        None,
    )
    .await;

    // On failure, classify the error and — for terminal failures only (out of
    // credits / not premium) — send the user a reply through the same channel
    // the inbound arrived on. Transient/internal failures stay silent (logged
    // only) so a flaky upstream doesn't spam "try again" at the sender. The turn
    // only reaches the reply sink itself via `say_to_user`, so without this the
    // sender would get nothing at all on a failed turn.
    let terminal_error: Option<crate::runtime::ChatError> = {
        let mut s = session.lock().await;
        match outcome {
            Ok(RunOutcome::Completed(st)) => {
                s.state = Some(st);
                None
            }
            Ok(RunOutcome::Interrupted { state: st, .. }) => {
                s.state = Some(st);
                None
            }
            Ok(RunOutcome::Failed {
                state: st,
                node,
                error,
            }) => {
                let raw = format!("{node}: {error}");
                if let Some(logger) = &s.diagnostics {
                    logger.log_error(&raw);
                }
                s.state = Some(st);
                let ce = crate::runtime::classify_turn_error(&error);
                (!ce.retryable).then_some(ce)
            }
            Err(e) => {
                // Framework error with no recoverable state: roll back so the
                // conversation history isn't wiped.
                let raw = error_chain(e.as_ref());
                if let Some(logger) = &s.diagnostics {
                    logger.log_error(&raw);
                }
                s.state = Some(state_before_turn);
                let ce = crate::runtime::classify_turn_error(&raw);
                (!ce.retryable).then_some(ce)
            }
        }
    };
    if let Some(ce) = terminal_error {
        let _ = sink(crate::tools::ReplyEnvelope::message(ce.user_message)).await;
    }
    persist_chat(session).await;
}

/// Shared entry point for inbound gateway messages. Routes the message into the
/// same persistent `ChatSession` machinery the workshop UI uses — one chat per
/// sender — so gateway conversations show up in the Chats and Sessions panes and
/// accumulate history. Replies are delivered out through the bound adapter by
/// the session's reply sink (no platform-specific tool instruction). Returns the
/// HTTP status the webhook should answer with.
async fn dispatch_inbound(state: Arc<ApiState>, n: NormalizedInbound) -> StatusCode {
    // Build the runtime context up front. Its error is a non-`Send`
    // `Box<dyn Error>`, so it must be fully handled here — before any `.await` —
    // or the handler future stops being `Send`.
    let context = match AgentRuntimeContext::from_environment() {
        Ok(ctx) => ctx,
        Err(e) => {
            log::error!("Gateway webhook: failed to build runtime context: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    let sender_key = gateway_sender_key(&n.channel_slug, &n.sender);
    let chat_id = gateway_session_for(&state, &n, &sender_key).await;
    let session = get_or_create_gateway_session(&state, &n, &sender_key, &chat_id).await;

    // Claim the turn. If one is already running for this sender, enqueue the
    // body so it's processed when the in-flight turn finishes (no lost messages).
    {
        let mut s = session.lock().await;
        if s.busy {
            lock_pending(&s.pending).push_back(n.body.clone());
            log::info!(
                "Inbound from {} queued — chat {chat_id} is mid-turn",
                n.sender
            );
            crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
                direction: "inbound".into(),
                platform: "text".into(),
                from: Some(n.sender.clone()),
                from_name: n.sender_name.clone(),
                body: crate::gateway_activity::truncate_body(&n.body),
                channel_id: Some(n.channel_slug.clone()),
                channel_name: Some(n.channel_name.clone()),
                outcome: "queued".into(),
                detail: Some("a turn is already in flight for this sender".into()),
                ..Default::default()
            });
            return StatusCode::OK;
        }
        s.busy = true;
    }

    // Cap concurrent agent runs. If saturated, release the claim and enqueue so
    // the message isn't dropped; 503 invites the provider to retry.
    let permit = match webhook_semaphore().clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            let mut s = session.lock().await;
            s.busy = false;
            lock_pending(&s.pending).push_back(n.body.clone());
            log::warn!(
                "Inbound from {} deferred — max concurrent agent runs ({MAX_WEBHOOK_TASKS}) reached",
                n.sender
            );
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };

    // Process this message, then drain any messages that queued while we ran,
    // FIFO, before releasing the busy flag. Fire-and-forget: the webhook has
    // already been acknowledged.
    let sink = gateway_reply_sink(
        n.adapter.clone(),
        n.sender.clone(),
        n.from.clone(),
        n.channel_slug.clone(),
        n.channel_name.clone(),
    );
    tokio::spawn(async move {
        let _permit = permit;
        let mut body = n.body.clone();
        loop {
            run_one_gateway_turn(&session, &context, sink.clone(), body).await;
            let mut s = session.lock().await;
            let next = { lock_pending(&s.pending).pop_front() };
            match next {
                Some(next) => body = next,
                None => {
                    s.busy = false;
                    break;
                }
            }
        }
    });

    StatusCode::OK
}

#[cfg(test)]
mod summary_tests {
    use super::{ChatMessageWire, preview_of, turns_in};

    fn user(content: &str) -> ChatMessageWire {
        ChatMessageWire::User {
            content: content.into(),
        }
    }

    #[test]
    fn a_conversation_is_counted_in_the_times_the_user_spoke() {
        let messages = vec![
            user("hi"),
            ChatMessageWire::Assistant {
                content: "hello".into(),
            },
            ChatMessageWire::ToolCall {
                id: "t".into(),
                call_id: None,
                name: "bash".into(),
                args: serde_json::Value::Null,
            },
            ChatMessageWire::Reset {
                at: "2026-08-27T04:00:00Z".into(),
                reason: "reset".into(),
            },
            user("again"),
        ];
        // Not 5. The agent's tool chatter and the divider are not things anyone
        // said, and counting raw messages made the same conversation show two
        // different sizes on two screens.
        assert_eq!(turns_in(&messages), 2);
    }

    #[test]
    fn a_preview_is_the_last_thing_said() {
        let messages = vec![
            user("what is on my calendar"),
            ChatMessageWire::Assistant {
                content: "Two things: standup at 9, and the dentist at 4.".into(),
            },
        ];
        // Deliberately not the opening line: a row is read to find out where a
        // conversation got to, and the question that started it stops answering
        // that on the second turn.
        assert_eq!(
            preview_of(&messages).as_deref(),
            Some("Two things: standup at 9, and the dentist at 4.")
        );
    }

    #[test]
    fn machinery_after_the_last_word_does_not_become_the_preview() {
        let messages = vec![
            user("deploy it"),
            ChatMessageWire::ToolCall {
                id: "t".into(),
                call_id: None,
                name: "bash".into(),
                args: serde_json::Value::Null,
            },
            ChatMessageWire::ToolResult {
                id: "t".into(),
                call_id: None,
                name: "bash".into(),
                result: "ok".into(),
            },
        ];
        // A turn still mid-flight ends in tool traffic. Labelling the row with a
        // tool's output would show the user shell noise they never wrote.
        assert_eq!(preview_of(&messages).as_deref(), Some("deploy it"));
    }

    #[test]
    fn a_preview_is_one_line_and_bounded() {
        let long = "a ".repeat(200);
        let preview = preview_of(&[user(&format!("line one\n\n  line two {long}"))]).unwrap();
        assert!(!preview.contains('\n'), "a row is one line");
        assert!(preview.starts_with("line one line two"));
        assert_eq!(preview.chars().count(), 81, "80 characters plus the ellipsis");
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn a_conversation_with_nothing_said_has_no_preview() {
        assert_eq!(preview_of(&[]), None);
        assert_eq!(preview_of(&[user("   ")]), None);
        // An empty last message falls back to the last one with words in it,
        // rather than blanking a row that has plenty to show.
        assert_eq!(
            preview_of(&[user("still here"), ChatMessageWire::Assistant { content: String::new() }])
                .as_deref(),
            Some("still here")
        );
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::{
        AgentMessage, AgentState, ChatMessageWire, ChatSession, SessionPreset,
        context_from_transcript, mark_reset, transcript_of,
    };
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn session() -> ChatSession {
        ChatSession {
            id: "c1".into(),
            instance_id: None,
            persona_slug: String::new(),
            model_name: String::new(),
            cwd: String::new(),
            preset: SessionPreset::Workshop,
            state: None,
            archived: Vec::new(),
            created_at: String::new(),
            diagnostics: None,
            trace: None,
            busy: false,
            interrupt: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        plan: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// A transcript reduced to something readable, which is enough to identify
    /// every message these tests care about.
    fn said(ts: &[ChatMessageWire]) -> Vec<String> {
        ts.iter()
            .map(|m| match m {
                ChatMessageWire::User { content } => format!("u:{content}"),
                ChatMessageWire::Assistant { content } => format!("a:{content}"),
                ChatMessageWire::Reset { reason, .. } => format!("reset:{reason}"),
                _ => "?".into(),
            })
            .collect()
    }

    fn turn(s: &mut ChatSession, user: &str, assistant: &str) {
        let mut st = match s.state.take() {
            Some(prev) => prev.continue_with(user.to_string()),
            None => AgentState::new(user.to_string()),
        };
        st.messages
            .push(AgentMessage::Assistant(assistant.to_string()));
        st.is_done = true;
        s.state = Some(st);
    }

    #[test]
    fn a_conversation_reads_as_one_thing() {
        let mut s = session();
        turn(&mut s, "hi", "hello");
        assert_eq!(said(&transcript_of(&s)), ["u:hi", "a:hello"]);
    }

    #[test]
    fn a_reset_keeps_the_history_and_drops_the_context() {
        let mut s = session();
        turn(&mut s, "hi", "hello");
        mark_reset(&mut s, "reset");
        // The conversation is intact and says where it restarted...
        assert_eq!(said(&transcript_of(&s)), ["u:hi", "a:hello", "reset:reset"]);
        // ...and the model is starting from nothing. This is the whole point:
        // `/clear` used to achieve the second by throwing away the first.
        assert!(s.state.is_none());

        turn(&mut s, "who am i", "no idea");
        assert_eq!(
            said(&transcript_of(&s)),
            ["u:hi", "a:hello", "reset:reset", "u:who am i", "a:no idea"]
        );
        assert_eq!(s.state.as_ref().unwrap().messages.len(), 2);
    }

    #[test]
    fn a_reload_resumes_the_context_not_the_whole_file() {
        let mut s = session();
        turn(&mut s, "hi", "hello");
        mark_reset(&mut s, "reset");
        turn(&mut s, "who am i", "no idea");

        // What `load_persisted_chats` does with the file this session would write.
        let file = transcript_of(&s);
        let reloaded = context_from_transcript(&file).expect("a context");
        assert_eq!(reloaded.messages.len(), 2, "only the post-reset turn");
        assert!(matches!(&reloaded.messages[0], AgentMessage::User(u) if u == "who am i"));
        // Replaying the pre-reset messages here would quietly undo every reset on
        // the next restart, which is the failure this guards.
    }

    #[test]
    fn a_reload_after_a_reset_with_nothing_since_has_no_context() {
        let mut s = session();
        turn(&mut s, "hi", "hello");
        mark_reset(&mut s, "reset");
        let file = transcript_of(&s);
        assert!(context_from_transcript(&file).is_none());
        // The history is still on disk, though — it is a divider, not a delete.
        assert_eq!(said(&file), ["u:hi", "a:hello", "reset:reset"]);
    }

    #[test]
    fn only_the_last_reset_starts_the_context() {
        let mut s = session();
        turn(&mut s, "one", "1");
        mark_reset(&mut s, "reset");
        turn(&mut s, "two", "2");
        mark_reset(&mut s, "reset");
        turn(&mut s, "three", "3");
        let reloaded = context_from_transcript(&transcript_of(&s)).expect("a context");
        assert_eq!(reloaded.messages.len(), 2);
        assert!(matches!(&reloaded.messages[0], AgentMessage::User(u) if u == "three"));
    }

    #[test]
    fn compaction_cannot_reach_what_a_reset_closed_off() {
        let mut s = session();
        turn(&mut s, "one", "1");
        mark_reset(&mut s, "reset");
        turn(&mut s, "two", "2");

        // What compaction does, from inside a running turn: replace the live
        // messages with a shorter summary. Only the current segment is its to
        // rewrite — everything a reset closed off is frozen.
        let mut compacted = AgentState::new("summary of the above");
        compacted.is_done = true;
        s.state = Some(compacted);

        // The archived segment survives untouched; the live one is whatever the
        // context now holds. Compaction collapsing the *current* segment is not
        // fixed here — but it can no longer reach across a reset, and it can no
        // longer drop the newest messages, which an index into the message list
        // did whenever compaction ran mid-turn.
        assert_eq!(
            said(&transcript_of(&s)),
            ["u:one", "a:1", "reset:reset", "u:summary of the above"]
        );
    }

    #[test]
    fn a_legacy_transcript_with_no_reset_loads_whole() {
        let transcript = vec![
            ChatMessageWire::User {
                content: "hi".into(),
            },
            ChatMessageWire::Assistant {
                content: "hello".into(),
            },
        ];
        let st = context_from_transcript(&transcript).expect("a context");
        assert_eq!(st.messages.len(), 2);
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::{
        ChatSession, SessionPreset, gateway_sender_key, latest_gateway_session,
        new_gateway_chat_id,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Mutex;

    #[test]
    fn sender_key_is_deterministic_per_sender_and_channel() {
        let a = gateway_sender_key("chan-1", "+1 (555) 000-1234");
        let b = gateway_sender_key("chan-1", "whatsapp:+15550001234");
        // Same person (modulo formatting) on the same channel → same key, so
        // every session they ever have lands under one agent and one memory.
        assert_eq!(a, b);
        assert_eq!(a, "gw-chan-1-15550001234");
    }

    #[test]
    fn sender_key_separates_senders_and_channels() {
        assert_ne!(
            gateway_sender_key("chan-1", "+15550001234"),
            gateway_sender_key("chan-1", "+15550009999")
        );
        assert_ne!(
            gateway_sender_key("chan-1", "+15550001234"),
            gateway_sender_key("chan-2", "+15550001234")
        );
    }

    #[test]
    fn sender_key_handles_non_numeric_senders() {
        let id = gateway_sender_key("chan-1", "user@example.com");
        assert_eq!(id, "gw-chan-1-userexamplecom");
        // Empty/symbol-only sender falls back to a stable placeholder.
        assert_eq!(gateway_sender_key("chan-1", "!!!"), "gw-chan-1-anon");
    }

    /// A chat store holding nothing but ids — everything `latest_gateway_session`
    /// reads.
    fn store(ids: &[&str]) -> HashMap<String, Arc<Mutex<ChatSession>>> {
        ids.iter()
            .map(|id| {
                let s = ChatSession {
                    id: (*id).to_string(),
                    instance_id: None,
                    persona_slug: String::new(),
                    model_name: String::new(),
                    cwd: String::new(),
                    preset: SessionPreset::Workshop,
                    state: None,
                    archived: Vec::new(),
                    created_at: String::new(),
                    diagnostics: None,
                    trace: None,
                    busy: false,
                    interrupt: Arc::new(AtomicBool::new(false)),
                    pending: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        plan: Arc::new(std::sync::Mutex::new(Vec::new())),
                };
                ((*id).to_string(), Arc::new(Mutex::new(s)))
            })
            .collect()
    }

    #[test]
    fn the_newest_session_is_the_current_one() {
        let key = "gw-chan-1-15550001234";
        let chats = store(&[
            &new_gateway_chat_id(key, 1_000),
            &new_gateway_chat_id(key, 3_000),
            &new_gateway_chat_id(key, 2_000),
        ]);
        assert_eq!(
            latest_gateway_session(&chats, key).as_deref(),
            Some(new_gateway_chat_id(key, 3_000).as_str())
        );
    }

    #[test]
    fn another_senders_sessions_are_not_mine() {
        let mine = "gw-chan-1-15550001234";
        // Deliberately a sender whose key is *almost* a prefix of the other's:
        // without the separator, "…123" would match "…1234-<time>" and two people
        // would share one conversation.
        let theirs = "gw-chan-1-1555000123";
        let chats = store(&[
            &new_gateway_chat_id(mine, 5_000),
            &new_gateway_chat_id(theirs, 9_000),
        ]);
        assert_eq!(
            latest_gateway_session(&chats, mine).as_deref(),
            Some(new_gateway_chat_id(mine, 5_000).as_str())
        );
        assert_eq!(
            latest_gateway_session(&chats, theirs).as_deref(),
            Some(new_gateway_chat_id(theirs, 9_000).as_str())
        );
    }

    #[test]
    fn a_sender_with_no_history_has_no_session() {
        assert_eq!(latest_gateway_session(&store(&[]), "gw-chan-1-1"), None);
    }

    #[test]
    fn the_pre_sessions_chat_is_adopted_and_then_superseded() {
        let key = "gw-chan-1-15550001234";
        // A pod upgraded from one-chat-per-sender-forever: the old conversation
        // is the current one, so the next message continues it rather than
        // appearing to lose it.
        let chats = store(&[key]);
        assert_eq!(latest_gateway_session(&chats, key).as_deref(), Some(key));
        // Once it goes stale and a real session starts, that one wins.
        let chats = store(&[key, &new_gateway_chat_id(key, 1)]);
        assert_eq!(
            latest_gateway_session(&chats, key).as_deref(),
            Some(new_gateway_chat_id(key, 1).as_str())
        );
    }
}

#[cfg(test)]
mod consent_tests {
    use super::changes_something;

    #[test]
    fn a_read_is_not_a_change_and_everything_else_is() {
        for read in [
            "read_file",
            "list_files",
            "grep",
            "find_files",
            "load_skill",
        ] {
            assert!(!changes_something(read), "{read} only reads");
        }
        // `write_file` classifies as auto-approving *WriteNewFile* when the path
        // does not exist, which answers "would this prompt?" — the wrong question
        // here. Creating a file is still an effect, and an agent about to run
        // unwatched should be described by what it can do, not by what it would
        // interrupt you for.
        for change in ["bash", "write_file", "edit_file", "web_fetch", "sub_agent"] {
            assert!(changes_something(change), "{change} changes something");
        }
    }
}

#[cfg(test)]
mod queue_tests {
    use super::{ChatEvent, chat_mailbox, claim_next_queued, plan_sink, queue_message};
    use crate::turn_plan::{PlanStep, StepStatus};
    use metalcraft::{AgentState, AgentUpdate, StepEvent, StepOutcome};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn step_to(next: &str) -> StepEvent {
        StepEvent {
            node: "tools".into(),
            next: next.into(),
            duration: std::time::Duration::from_millis(1),
            outcome: StepOutcome::Success,
        }
    }

    /// A queue with one message in it, and a recorder for the frames the
    /// mailbox announces.
    fn mailbox_with(
        message: &str,
    ) -> (
        metalcraft::Mailbox<AgentState>,
        Arc<Mutex<VecDeque<String>>>,
        Arc<Mutex<Vec<ChatEvent>>>,
    ) {
        let pending = Arc::new(Mutex::new(VecDeque::from([message.to_string()])));
        let seen: Arc<Mutex<Vec<ChatEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let mailbox = chat_mailbox(pending.clone(), {
            let seen = seen.clone();
            Arc::new(move |ev| seen.lock().unwrap().push(ev))
        });
        (mailbox, pending, seen)
    }

    /// The rule the whole feature rests on. A user message appended between a
    /// tool call and its result orphans the call, and the Responses API rejects
    /// the next request with an opaque 400 — so the mailbox delivers only when
    /// the agent node is next, by which point every result in the batch has
    /// landed.
    #[test]
    fn a_message_is_only_delivered_when_the_agent_node_is_next() {
        let (mailbox, pending, seen) = mailbox_with("actually, do X instead");
        let state = AgentState::new("original");

        // About to run tools: the history is mid-batch. Deliver nothing, and
        // leave the message queued rather than dropping it.
        assert!(mailbox(&state, &step_to("tools")).is_empty());
        assert_eq!(pending.lock().unwrap().len(), 1, "a held message is not a lost one");
        assert!(seen.lock().unwrap().is_empty(), "nothing was delivered to announce");

        // The turn is ending: too late to be answered, so it stays queued for
        // `drain_queued_turns` to run as its own turn.
        assert!(mailbox(&state, &step_to("__end__")).is_empty());
        assert_eq!(pending.lock().unwrap().len(), 1);

        // Back to the agent: now it is safe, and the queue empties.
        let updates = mailbox(&state, &step_to("agent"));
        assert_eq!(updates.len(), 1);
        assert!(matches!(&updates[0], AgentUpdate::UserMessage(m) if m == "actually, do X instead"));
        assert!(pending.lock().unwrap().is_empty(), "a delivered message must not be redelivered");
    }

    /// The client showed the message as pending when it was queued; it has to
    /// learn when that stopped being true.
    #[test]
    fn delivering_announces_the_message_it_took() {
        let (mailbox, _pending, seen) = mailbox_with("hurry up");
        let _ = mailbox(&AgentState::new("x"), &step_to("agent"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(matches!(&seen[0], ChatEvent::Injected { message } if message == "hurry up"));
    }

    /// The frame reaches whoever is watching now; the recorded copy is what a
    /// client arriving late is handed. A sink doing only the first leaves a
    /// reconnecting client with no plan until the next `update_plan`.
    #[test]
    fn a_plan_is_both_recorded_and_announced() {
        let held = Arc::new(Mutex::new(Vec::new()));
        let seen: Arc<Mutex<Vec<ChatEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = plan_sink(held.clone(), {
            let seen = seen.clone();
            Arc::new(move |ev| seen.lock().unwrap().push(ev))
        });

        let steps = vec![PlanStep {
            step: "read the repo".into(),
            persona: Some("research-agent".into()),
            status: StepStatus::InProgress,
        }];
        sink(&steps);

        assert_eq!(held.lock().unwrap().len(), 1, "the session holds the plan");
        assert!(matches!(&seen.lock().unwrap()[0], ChatEvent::Plan { steps } if steps.len() == 1));
    }

    /// A turn starting clears what the last one left, so a reconnecting client
    /// is never handed a finished turn's plan as though it were live.
    #[test]
    fn an_empty_plan_clears_what_was_held() {
        let held = Arc::new(Mutex::new(vec![PlanStep {
            step: "stale".into(),
            persona: None,
            status: StepStatus::Done,
        }]));
        let sink = plan_sink(held.clone(), Arc::new(|_| {}));
        sink(&[]);
        assert!(held.lock().unwrap().is_empty());
    }

    /// The common case: nothing typed, so the hook costs one call and changes
    /// nothing about the run.
    #[test]
    fn an_empty_queue_delivers_nothing() {
        let pending = Arc::new(Mutex::new(VecDeque::new()));
        let mailbox = chat_mailbox(pending, Arc::new(|_| {}));
        assert!(mailbox(&AgentState::new("x"), &step_to("agent")).is_empty());
    }

    #[test]
    fn a_message_sent_mid_turn_gets_a_place_in_line() {
        let mut pending = VecDeque::new();
        assert_eq!(queue_message(&mut pending, "first".into()), 1);
        assert_eq!(queue_message(&mut pending, "second".into()), 2);
    }

    /// The claim and the pop are one operation. If they were not, two drains
    /// running at once would both take a message and run the same conversation
    /// twice from two different states.
    #[test]
    fn claiming_takes_the_turn_and_the_message_together() {
        let mut busy = false;
        let mut pending = VecDeque::from(["first".to_string(), "second".to_string()]);

        assert_eq!(claim_next_queued(&mut busy, &mut pending).as_deref(), Some("first"));
        assert!(busy, "claiming a message must also claim the turn");

        // A second drain arriving while the first holds the turn gets nothing,
        // and must not consume "second".
        assert_eq!(claim_next_queued(&mut busy, &mut pending), None);
        assert_eq!(pending.len(), 1, "a refused claim must not eat a message");

        busy = false;
        assert_eq!(claim_next_queued(&mut busy, &mut pending).as_deref(), Some("second"));
    }

    /// An empty queue leaves the turn unclaimed — otherwise a drain that found
    /// nothing to do would strand the chat as permanently busy.
    #[test]
    fn an_empty_queue_does_not_claim_the_turn() {
        let mut busy = false;
        let mut pending = VecDeque::new();
        assert_eq!(claim_next_queued(&mut busy, &mut pending), None);
        assert!(!busy, "nothing ran, so nothing may be left holding the chat");
    }
}

#[cfg(test)]
mod interrupt_tests {
    use super::{AtomicBool, GuardAction, Ordering, continue_unless_stopped};

    #[test]
    fn a_pressed_stop_stops_the_run_and_nothing_else_does() {
        let flag = AtomicBool::new(false);
        assert!(matches!(
            continue_unless_stopped(&flag),
            GuardAction::Continue
        ));

        flag.store(true, Ordering::Relaxed);
        let GuardAction::Stop(reason) = continue_unless_stopped(&flag) else {
            panic!("a pressed stop must halt the executor");
        };
        // The reason rides out in `done{status:"interrupted", reason}` and the
        // client prints it verbatim in the transcript, so it is a sentence
        // addressed to a person — not a note about which flag was set.
        assert_eq!(reason, "Stopped by the user.");
    }
}

#[cfg(test)]
mod scheduled_flow_schema_tests {
    use utoipa::PartialSchema;

    /// The field names a schema advertises, so a mirror can be compared to the
    /// thing it mirrors without hand-listing them in two places.
    fn schema_fields(v: &serde_json::Value) -> Vec<String> {
        let mut out: Vec<String> = v
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    fn json_of<T: PartialSchema>() -> serde_json::Value {
        serde_json::to_value(T::schema()).expect("schema serializes")
    }

    /// A described mirror can drift from what it describes, and a mirror that has
    /// drifted is worse than the `Object` it replaced: it states something wrong
    /// with a straight face, and every generated client believes it.
    ///
    /// So the mirror is checked against the real artifact's own serialization
    /// rather than against a list somebody remembered to update. Add a field to
    /// `metalcraft_flows::ScheduledFlow` and this fails naming it.
    #[test]
    fn scheduled_flow_schema_matches_the_artifact() {
        let real = metalcraft_flows::ScheduledFlow {
            id: "sf_test".into(),
            flow_id: "f1".into(),
            enabled: true,
            schedule: metalcraft_flows::ScheduleSpec {
                trigger: metalcraft_flows::ScheduleTrigger::Cron {
                    cron: "0 0 9 * * *".into(),
                },
                name: Some("Morning brief".into()),
                timezone: Some("America/Detroit".into()),
                inputs: None,
                persona: None,
            },
            instance_id: Some("i1".into()),
            from_suggestion: Some("morning".into()),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        };

        // Serialized with every optional present, because `skip_serializing_if`
        // would otherwise hide exactly the fields most likely to be forgotten.
        let wire = serde_json::to_value(&real).expect("artifact serializes");
        let mut actual: Vec<String> = wire
            .as_object()
            .expect("an object")
            .keys()
            .filter(|k| *k != "type" && *k != "cron")
            .cloned()
            .collect();
        actual.sort();

        let mirrored = schema_fields(&json_of::<super::ScheduledFlowDoc>());
        assert_eq!(
            mirrored, actual,
            "ScheduledFlowDoc has drifted from metalcraft_flows::ScheduledFlow — \
             update the mirror (or delete it, if the crate now derives ToSchema)"
        );
    }

    /// Same guard for the trigger, which is the half a client has to *construct*
    /// rather than merely read — a missing field here is a request nobody can
    /// build from the document.
    #[test]
    fn schedule_spec_schema_matches_the_artifact() {
        let spec = metalcraft_flows::ScheduleSpec {
            trigger: metalcraft_flows::ScheduleTrigger::Minutes { interval: 30 },
            name: Some("n".into()),
            timezone: Some("UTC".into()),
            inputs: Some(serde_json::json!({})),
            persona: Some("p".into()),
        };
        let wire = serde_json::to_value(&spec).expect("spec serializes");
        let obj = wire.as_object().expect("an object");

        // The trigger is flattened and tagged, so `type` is a real wire field and
        // `interval`/`cron` are whichever the variant carries. The mirror declares
        // all three; the artifact only ever shows the ones this variant uses.
        assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("minutes"));
        assert_eq!(obj.get("interval").and_then(|v| v.as_u64()), Some(30));

        let mirrored = schema_fields(&json_of::<super::ScheduleSpecDoc>());
        for key in obj.keys() {
            assert!(
                mirrored.contains(key),
                "ScheduleSpecDoc is missing '{key}', which the artifact serializes"
            );
        }
        for declared in [
            "type", "interval", "cron", "name", "timezone", "inputs", "persona",
        ] {
            assert!(
                mirrored.contains(&declared.to_string()),
                "ScheduleSpecDoc no longer declares '{declared}'"
            );
        }
    }

    /// The bug that started this: `value_type = Object` generates
    /// `Record<string, never>`, whose index signature poisons every field it is
    /// intersected with in TypeScript. Nothing in the scheduled-flow surface may
    /// go back to describing itself as a bare object.
    #[test]
    fn the_scheduled_flow_surface_describes_itself() {
        for (name, json) in [
            ("ScheduledFlowDoc", json_of::<super::ScheduledFlowDoc>()),
            ("ScheduleSpecDoc", json_of::<super::ScheduleSpecDoc>()),
        ] {
            assert!(
                !schema_fields(&json).is_empty(),
                "{name} publishes no properties — a client generating from this \
                 document would see an opaque object"
            );
        }
    }
}
