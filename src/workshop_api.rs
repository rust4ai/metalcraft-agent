use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json,
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use crate::approval::ApprovalMode;
use crate::diagnostics::DiagnosticsLogger;
use crate::trace::TraceLogger;
use crate::flows;
use crate::paths;
use crate::persona::{Persona, PersonaSummary};
use crate::runtime::{AgentRuntimeContext, RuntimeOptions};
use crate::session_io::SessionPreset;
use crate::diagnostics_browse::{
    list_diagnostics_sessions, read_diagnostics_session, DiagnosticsSessionSummary,
};
use crate::skill::{list_skill_summaries, load_skill, save_skill, Skill, SkillSummary};
use crate::tools::http_api::HttpApiToolConfig;
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
    B.get_or_init(|| Arc::new(Mutex::new(HashMap::new()))).clone()
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

struct ChatSession {
    id: String,
    persona_slug: String,
    model_name: String,
    cwd: String,
    /// The session's I/O type — workshop chat vs. a bound gateway conversation.
    /// Decides where `say_to_user` replies are delivered.
    preset: SessionPreset,
    state: Option<AgentState>,
    created_at: String,
    diagnostics: Option<Arc<DiagnosticsLogger>>,
    /// OTLP trace writer, parallel to `diagnostics`. Shares its session id.
    trace: Option<Arc<TraceLogger>>,
    /// True while a turn is mid-flight. Prevents two concurrent turns from
    /// stomping on the same state.
    busy: bool,
    /// Inbound gateway messages that arrived while a turn was already running,
    /// drained FIFO when the in-flight turn finishes. Empty for workshop chats.
    pending: std::collections::VecDeque<String>,
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

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorResponse { error: msg.into() })).into_response()
}

// ── Snapshot types ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct ProjectSnapshot {
    personas: Vec<PersonaSummary>,
    skills: Vec<SkillSummary>,
    flows: Vec<metalcraft_flows::FlowSummary>,
    sessions: Vec<DiagnosticsSessionSummary>,
    api_tools: Vec<ApiToolSummary>,
    keys: Vec<KeySummary>,
    layout: ProjectLayout,
}

#[derive(Serialize)]
struct ProjectLayout {
    data_dir: String,
    personas_dir: String,
    skills_dir: String,
    flows_dir: String,
    sessions_dir: String,
    api_tools_dir: String,
}

#[derive(Serialize)]
struct ApiToolSummary {
    name: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    read_only: bool,
}

/// A stored API key, exposed to the workshop with its value masked — the
/// raw secret is never sent over the wire.
#[derive(Serialize)]
struct KeySummary {
    name: String,
    masked: String,
}

/// A key recommended by one or more *enabled* integration packs (from their
/// `requires_env`), with whether it currently resolves (key store or env) and
/// which packs declare it. Drives the "keys these packs still need" list in
/// the key store UI — `configured: false` is the hint to add it.
#[derive(Serialize)]
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

    if provided != state.api_key {
        return (StatusCode::UNAUTHORIZED, Json(ErrorResponse { error: "unauthorized".into() })).into_response();
    }

    next.run(request).await
}

// ── Router + server ────────────────────────────────────────────────────

/// Build the workshop API router. Callable from any binary that wants to
/// host the admin API — `metalcraft-agent --api` runs it stand-alone while
/// `metalcraft-daemon --api` mounts it alongside the event listener and the
/// flow scheduler.
pub fn build_router(api_key: String) -> Router {
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

    Router::new()
        .route("/api/v1/info", get(agent_info))
        .route("/api/v1/snapshot", get(get_snapshot))
        .route("/api/v1/personas/{slug}", get(get_persona))
        .route("/api/v1/personas/{slug}", put(put_persona))
        .route("/api/v1/personas/{slug}", delete(delete_persona))
        .route("/api/v1/skills/{slug}", get(get_skill))
        .route("/api/v1/skills/{slug}", put(put_skill))
        .route("/api/v1/skills/{slug}", delete(delete_skill))
        .route("/api/v1/flows/{id}", get(get_flow))
        .route("/api/v1/flows/{id}", put(put_flow))
        .route("/api/v1/flows/{id}", delete(delete_flow))
        .route("/api/v1/flows/{id}/run", post(post_run_flow))
        .route("/api/v1/flow-runs", get(list_flow_runs))
        .route("/api/v1/flow-runs/{run_id}", get(get_flow_run))
        .route("/api/v1/flow-runs/{run_id}/resume", post(post_resume_flow_run))
        .route("/api/v1/flow-templates", get(list_flow_templates))
        .route("/api/v1/flow-templates/{slug}", get(get_flow_template))
        .route("/api/v1/diagnostics", get(list_diagnostics))
        .route("/api/v1/diagnostics/{id}", get(get_diagnostics_session))
        .route("/api/v1/api-tools", get(list_api_tools))
        .route("/api/v1/api-tools/{name}", get(get_api_tool))
        .route("/api/v1/api-tools/{name}", put(put_api_tool))
        .route("/api/v1/api-tools/{name}", delete(delete_api_tool))
        .route("/api/v1/keys", get(list_keys))
        .route("/api/v1/keys/recommended", get(list_recommended_keys))
        .route("/api/v1/keys/{name}", put(put_key))
        .route("/api/v1/keys/{name}", delete(delete_key))
        .route("/api/v1/chats", get(list_chats).post(post_create_chat))
        .route("/api/v1/chats/{id}", get(get_chat).delete(delete_chat))
        .route("/api/v1/chats/{id}/turn", post(post_chat_turn))
        .route("/api/v1/chats/{id}/events", get(get_chat_events))
        .route("/api/v1/scheduled-tasks", get(list_scheduled_tasks))
        .route("/api/v1/scheduled-tasks/{id}", delete(delete_scheduled_task))
        .route("/api/v1/integration-packs", get(list_integration_packs))
        .route("/api/v1/integration-packs/{id}", get(get_integration_pack))
        .route("/api/v1/integration-packs/{id}/enabled", put(put_pack_enabled))
        // Gateway channels: declarative channel *types* + user-created channel
        // *instances*. Inbound messages arrive on the unauthenticated webhook
        // routes below; these manage configuration.
        .route("/api/v1/gateway/types", get(list_gateway_types))
        .route("/api/v1/gateway/channels", get(list_gateway_channels).post(post_create_gateway_channel))
        .route("/api/v1/gateway/channels/{id}", put(put_gateway_channel).delete(delete_gateway_channel))
        .route("/api/v1/gateway/channels/{id}/enabled", put(put_gateway_channel_enabled))
        .route("/api/v1/gateway/channels/{id}/events", get(list_gateway_channel_events))
        .route("/api/v1/gateway/activity", get(list_gateway_activity))
        // Metalcraft Gateway — zero-copy connect (status / inline register / connect).
        .route("/api/v1/gateway/metalcraft/status", get(gateway_metalcraft_status))
        .route("/api/v1/gateway/metalcraft/register", post(gateway_metalcraft_register))
        .route("/api/v1/gateway/metalcraft/connect", post(gateway_metalcraft_connect))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        // Health check — registered after the auth layer so it stays
        // unauthenticated (for Railway / DO App Platform probes).
        .route("/health", get(health))
        // Public landing page. Also after the auth layer, so hitting the pod's
        // ingress host in a browser shows a friendly status page instead of a
        // 401 or a bare JSON blob. No secrets — just "this agent is alive".
        .route("/", get(landing))
        // Inbound gateway webhooks — unauthenticated like /health; each adapter
        // verifies provenance its own way (signed requests). PipeStreamr is the
        // active path; the Twilio route stays mounted but no channel type ships
        // for it right now.
        .route("/webhook/pipestreamr", post(handle_pipestreamr_webhook))
        .route("/webhook/twilio", post(handle_twilio_webhook))
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
async fn agent_info() -> impl IntoResponse {
    let default_persona = std::env::var("METALCRAFT_DEFAULT_PERSONA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "orchestrator-agent".to_string());
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

async fn get_snapshot() -> Json<ProjectSnapshot> {
    let personas = list_persona_summaries();
    let skills = list_skill_summaries();
    let flows = metalcraft_flows::list_flows(&paths::flows_dir());
    let sessions = list_diagnostics_sessions();
    let api_tools = list_api_tool_summaries();
    let keys = list_key_summaries();

    Json(ProjectSnapshot {
        personas,
        skills,
        flows,
        sessions,
        api_tools,
        keys,
        layout: ProjectLayout {
            data_dir: paths::data_dir().display().to_string(),
            personas_dir: paths::personas_dir().display().to_string(),
            skills_dir: paths::skills_dir().display().to_string(),
            flows_dir: paths::flows_dir().display().to_string(),
            sessions_dir: paths::sessions_dir().display().to_string(),
            api_tools_dir: paths::api_tools_dir().display().to_string(),
        },
    })
}

// ── Persona handlers ────────────────────────────────────────────────────

/// User-local personas plus enabled-pack personas, with locals shadowing
/// packs on slug collision.
fn list_persona_summaries() -> Vec<PersonaSummary> {
    let layered = crate::integration_packs::list_files_layered(
        &paths::personas_dir(),
        "personas",
        "json",
    );
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

async fn get_persona(Path(slug): Path<String>) -> Response {
    let filename = format!("{slug}.json");
    let Some((path, _origin)) = crate::integration_packs::resolve_file(
        &paths::personas_dir(),
        "personas",
        &filename,
    ) else {
        return err_json(StatusCode::NOT_FOUND, format!("persona '{slug}' not found"));
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return err_json(StatusCode::NOT_FOUND, format!("persona '{slug}' not found"));
    };
    match serde_json::from_str::<Persona>(&content) {
        Ok(persona) => Json(persona).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse: {e}")),
    }
}

async fn put_persona(Path(slug): Path<String>, Json(persona): Json<Persona>) -> Response {
    // Reject if this slug is currently owned by a pack — the user must pick
    // a different slug instead of trying to shadow a read-only entry through
    // this endpoint. (Genuine local shadows happen when the user creates a
    // local file with the same slug via the filesystem.)
    let filename = format!("{slug}.json");
    let local_exists = paths::personas_dir().join(&filename).exists();
    if !local_exists {
        if let Some((_, origin)) = crate::integration_packs::resolve_file(
            &paths::personas_dir(),
            "personas",
            &filename,
        ) {
            if let Some(pack_id) = origin.pack_id() {
                return err_json(
                    StatusCode::CONFLICT,
                    format!(
                        "persona '{slug}' is provided by the '{pack_id}' integration pack and is read-only. Choose a different slug."
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

async fn get_skill(Path(slug): Path<String>) -> Response {
    match load_skill(&slug) {
        Some(skill) => Json(skill).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("skill '{slug}' not found")),
    }
}

async fn put_skill(Path(slug): Path<String>, Json(skill): Json<Skill>) -> Response {
    // Block writing to a slug that's currently provided by a pack (the user
    // would otherwise be shadowing read-only content silently).
    let filename = format!("{slug}.md");
    let local_exists = paths::skills_dir().join(&filename).exists();
    if !local_exists {
        if let Some((_, origin)) = crate::integration_packs::resolve_file(
            &paths::skills_dir(),
            "skills",
            &filename,
        ) {
            if let Some(pack_id) = origin.pack_id() {
                return err_json(
                    StatusCode::CONFLICT,
                    format!(
                        "skill '{slug}' is provided by the '{pack_id}' integration pack and is read-only. Choose a different slug."
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
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete: {e}")),
    }
}

// ── Flow handlers ───────────────────────────────────────────────────────

async fn get_flow(Path(id): Path<String>) -> Response {
    match metalcraft_flows::load_flow(&paths::flows_dir(), &id) {
        Some(flow) => Json(flow).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found")),
    }
}

async fn put_flow(Path(id): Path<String>, Json(mut flow): Json<metalcraft_flows::SavedFlow>) -> Response {
    flow.id = id;
    match metalcraft_flows::save_flow(&paths::flows_dir(), &flow) {
        Ok(()) => Json(flow).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn delete_flow(Path(id): Path<String>) -> Response {
    if metalcraft_flows::delete_flow(&paths::flows_dir(), &id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found"))
    }
}

// ── Diagnostics handlers ────────────────────────────────────────────────

async fn list_diagnostics() -> Json<Vec<DiagnosticsSessionSummary>> {
    Json(list_diagnostics_sessions())
}

async fn get_diagnostics_session(Path(id): Path<String>) -> Response {
    match read_diagnostics_session(&id) {
        Some(session) => Json(session).into_response(),
        None => err_json(
            StatusCode::NOT_FOUND,
            format!("diagnostics session '{id}' not found"),
        ),
    }
}

// ── API Tool handlers ───────────────────────────────────────────────────

fn list_api_tool_summaries() -> Vec<ApiToolSummary> {
    let layered = crate::integration_packs::list_files_layered(
        &paths::api_tools_dir(),
        "api_tools",
        "json",
    );
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

async fn list_api_tools() -> Json<Vec<ApiToolSummary>> {
    Json(list_api_tool_summaries())
}

async fn get_api_tool(Path(name): Path<String>) -> Response {
    let filename = format!("{name}.json");
    let Some((path, _)) = crate::integration_packs::resolve_file(
        &paths::api_tools_dir(),
        "api_tools",
        &filename,
    ) else {
        return err_json(StatusCode::NOT_FOUND, format!("api-tool '{name}' not found"));
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<HttpApiToolConfig>(&content) {
            Ok(config) => Json(config).into_response(),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse: {e}")),
        },
        Err(_) => err_json(StatusCode::NOT_FOUND, format!("api-tool '{name}' not found")),
    }
}

async fn put_api_tool(Path(name): Path<String>, Json(mut config): Json<HttpApiToolConfig>) -> Response {
    config.name = name.clone();
    let filename = format!("{name}.json");
    let local_exists = paths::api_tools_dir().join(&filename).exists();
    if !local_exists {
        if let Some((_, origin)) = crate::integration_packs::resolve_file(
            &paths::api_tools_dir(),
            "api_tools",
            &filename,
        ) {
            if let Some(pack_id) = origin.pack_id() {
                return err_json(
                    StatusCode::CONFLICT,
                    format!(
                        "api-tool '{name}' is provided by the '{pack_id}' integration pack and is read-only. Choose a different name."
                    ),
                );
            }
        }
    }
    let path = paths::api_tools_dir().join(&filename);
    match serde_json::to_string_pretty(&config) {
        Ok(content) => match std::fs::write(&path, content) {
            Ok(()) => Json(config).into_response(),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write: {e}")),
        },
        Err(e) => err_json(StatusCode::BAD_REQUEST, format!("Failed to serialize: {e}")),
    }
}

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
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete: {e}")),
    }
}

// ── API key store handlers ──────────────────────────────────────────────
//
// The key store (`<data>/keys.json`) holds the secrets that HTTP-API tools
// reference via `$NAME`. The workshop manages them here; raw values only ever
// flow inward (on PUT) — list/get responses are always masked.

#[derive(Deserialize)]
struct KeyValueBody {
    value: String,
}

fn list_key_summaries() -> Vec<KeySummary> {
    crate::key_store::KeyStore::load(&paths::keys_file())
        .list_masked()
        .into_iter()
        .map(|(name, masked)| KeySummary { name, masked })
        .collect()
}

async fn list_keys() -> Json<Vec<KeySummary>> {
    Json(list_key_summaries())
}

/// Keys recommended by enabled packs, each flagged configured/missing. Lets the
/// key store UI surface "these enabled packs need these keys" without the user
/// having to read each pack's manifest.
async fn list_recommended_keys() -> Json<Vec<RecommendedKey>> {
    // Merge recommendations from enabled integration packs and enabled gateway
    // channel types. The `packs` field carries the source label (pack id or
    // channel-type name) so the key-store UI can show "who wants this".
    let mut merged: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for (name, sources) in crate::integration_packs::recommended_env() {
        merged.entry(name).or_default().extend(sources);
    }
    for (name, sources) in crate::gateway_channels::recommended_env() {
        let entry = merged.entry(name).or_default();
        for s in sources {
            if !entry.contains(&s) {
                entry.push(s);
            }
        }
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

/// Upsert a key. The name comes from the path; the raw value from the body.
async fn put_key(Path(name): Path<String>, Json(body): Json<KeyValueBody>) -> Response {
    if name.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "key name must not be empty");
    }
    if body.value.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "key value must not be empty");
    }
    let path = paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    store.upsert(&name, &body.value);
    match store.save(&path) {
        Ok(()) => Json(KeySummary {
            masked: crate::key_store::mask(&body.value),
            name,
        })
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write: {e}")),
    }
}

async fn delete_key(Path(name): Path<String>) -> Response {
    let path = paths::keys_file();
    let mut store = crate::key_store::KeyStore::load(&path);
    if !store.delete(&name) {
        return err_json(StatusCode::NOT_FOUND, format!("key '{name}' not found"));
    }
    match store.save(&path) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write: {e}")),
    }
}

// ── Flow template handlers ──────────────────────────────────────────────

#[derive(Serialize)]
struct FlowTemplateSummary {
    slug: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
}

#[derive(Serialize)]
struct FlowTemplate {
    slug: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
    /// The raw template flow JSON, ready to be cloned and edited as a new flow.
    flow: serde_json::Value,
}

fn list_flow_template_summaries() -> Vec<FlowTemplateSummary> {
    let layered = crate::integration_packs::list_files_layered(
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

async fn list_flow_templates() -> Json<Vec<FlowTemplateSummary>> {
    Json(list_flow_template_summaries())
}

async fn get_flow_template(Path(slug): Path<String>) -> Response {
    let filename = format!("{slug}.json");
    let Some((path, origin)) = crate::integration_packs::resolve_file(
        &paths::flow_templates_dir(),
        "flow_templates",
        &filename,
    ) else {
        return err_json(StatusCode::NOT_FOUND, format!("template '{slug}' not found"));
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return err_json(StatusCode::NOT_FOUND, format!("template '{slug}' not found")),
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("parse error: {e}"));
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

#[derive(Deserialize, Default)]
struct RunFlowRequest {
    /// Persona to run the flow's prompts as. Defaults to `coding-agent` if
    /// the caller doesn't specify one.
    #[serde(default)]
    persona_slug: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
    /// Values for the flow's declared entry `inputs`. Object of `{ name: value }`;
    /// missing inputs fall back to their declared defaults. Ignored by v1 flows.
    #[serde(default)]
    inputs: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct RunFlowResponse {
    flow_id: String,
    prompts: Vec<flows::FlowPromptResult>,
}

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
    let persona_slug = req
        .persona_slug
        .unwrap_or_else(|| "coding-agent".to_string());
    let model_name = req.model_name.unwrap_or_else(crate::runtime::configured_default_model);

    let Some(flow) = metalcraft_flows::load_flow(&crate::paths::flows_dir(), &id) else {
        return err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found"));
    };

    // v2 flows run on the stateful executor and return a run summary; v1 flows
    // keep the legacy per-prompt response.
    if crate::flow_exec::is_v2_flow(&flow) {
        let inputs = req.inputs.clone().unwrap_or_else(|| serde_json::json!({}));
        return match crate::flow_exec::run_flow_v2(
            &context,
            flow,
            &state.cwd,
            &persona_slug,
            &model_name,
            &inputs,
        )
        .await
        {
            Ok(summary) => Json(summary).into_response(),
            Err(e) => err_json(StatusCode::BAD_REQUEST, e),
        };
    }

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
async fn get_flow_run(Path(run_id): Path<String>) -> Response {
    match crate::flow_runs::load_run(&paths::runs_dir(), &run_id) {
        Some(run) => Json(run).into_response(),
        None => err_json(StatusCode::NOT_FOUND, format!("run '{run_id}' not found")),
    }
}

#[derive(Deserialize)]
struct ResumeFlowRunRequest {
    /// Handle to take (an approval decision, or `"after"` for a wait).
    handle: String,
    /// Optional value to set as the resumed node's `_last` input.
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// Resume a paused flow run.
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

#[derive(Serialize)]
struct ChatSummary {
    id: String,
    persona_slug: String,
    model_name: String,
    created_at: String,
    turn_count: usize,
}

#[derive(Serialize)]
struct ChatDetail {
    id: String,
    persona_slug: String,
    model_name: String,
    created_at: String,
    messages: Vec<ChatMessageWire>,
}

/// Wire form for `metalcraft::AgentMessage` — the in-memory enum isn't
/// `Serialize`, so we convert before responding. Also used as the on-disk
/// format for persisted chats, so it derives `Deserialize` too.
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "role", rename_all = "snake_case")]
enum ChatMessageWire {
    User { content: String },
    Assistant { content: String },
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
}

impl From<&AgentMessage> for ChatMessageWire {
    fn from(m: &AgentMessage) -> Self {
        match m {
            AgentMessage::User(s) => Self::User { content: s.clone() },
            AgentMessage::Assistant(s) => Self::Assistant { content: s.clone() },
            AgentMessage::ToolCall { id, call_id, name, args } => Self::ToolCall {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                args: args.clone(),
            },
            AgentMessage::ToolResult { id, call_id, name, result } => Self::ToolResult {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                result: result.clone(),
            },
        }
    }
}

impl From<ChatMessageWire> for AgentMessage {
    fn from(w: ChatMessageWire) -> Self {
        match w {
            ChatMessageWire::User { content } => AgentMessage::User(content),
            ChatMessageWire::Assistant { content } => AgentMessage::Assistant(content),
            ChatMessageWire::ToolCall { id, call_id, name, args } => {
                AgentMessage::ToolCall { id, call_id, name, args }
            }
            ChatMessageWire::ToolResult { id, call_id, name, result } => {
                AgentMessage::ToolResult { id, call_id, name, result }
            }
        }
    }
}

/// On-disk shape for a persisted chat. Mirrors [`ChatSession`] but flattens
/// the optional `AgentState` to a plain message vec so the file is human-
/// readable and tolerant of metalcraft API changes.
#[derive(Serialize, Deserialize)]
struct PersistedChat {
    id: String,
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
async fn persist_chat(session: &Arc<Mutex<ChatSession>>) {
    let snapshot = {
        let s = session.lock().await;
        PersistedChat {
            id: s.id.clone(),
            persona_slug: s.persona_slug.clone(),
            model_name: s.model_name.clone(),
            cwd: s.cwd.clone(),
            created_at: s.created_at.clone(),
            preset: s.preset.clone(),
            messages: s
                .state
                .as_ref()
                .map(|st| st.messages.iter().map(ChatMessageWire::from).collect())
                .unwrap_or_default(),
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
        let state = if pc.messages.is_empty() {
            None
        } else {
            // Reconstruct an AgentState from the persisted messages. Rebuild
            // it from scratch using `new` + push so we don't have to mark
            // every field of AgentState public.
            let mut iter = pc.messages.into_iter();
            let first = match iter.next() {
                Some(m) => m,
                None => {
                    log::warn!("empty messages on chat {} after non-empty check", pc.id);
                    continue;
                }
            };
            // First message should be a User in normal flow; if not, seed with
            // an empty user message so AgentState::new is satisfied.
            let mut st = match first {
                ChatMessageWire::User { ref content } => AgentState::new(content.clone()),
                _ => {
                    let mut s = AgentState::new("");
                    s.messages.clear();
                    s.messages.push(first.into());
                    s
                }
            };
            for m in iter {
                st.messages.push(m.into());
            }
            st.is_done = true; // turns are completed when persisted
            Some(st)
        };
        let session = ChatSession {
            id: pc.id.clone(),
            persona_slug: pc.persona_slug,
            model_name: pc.model_name,
            cwd: pc.cwd,
            preset: pc.preset,
            state,
            created_at: pc.created_at,
            diagnostics: None,
            trace: None, // recreated lazily on the first turn, like diagnostics
            busy: false, // anything that was busy at shutdown couldn't have
                          // finished cleanly; reset so the user can retry.
            pending: std::collections::VecDeque::new(),
        };
        out.insert(pc.id.clone(), Arc::new(Mutex::new(session)));
    }
    out
}

#[derive(Deserialize)]
struct CreateChatRequest {
    persona_slug: String,
    #[serde(default)]
    model_name: Option<String>,
}

async fn list_chats(State(_state): State<Arc<ApiState>>) -> Response {
    // Read the chat list straight from `<data>/chats/*.json` rather than the
    // in-memory store. The two are kept in sync (every create/turn/delete
    // persists), but reading disk means the list is correct across restarts
    // and reflects any out-of-band edits — the in-memory map is only the
    // authority for *live* per-turn state, not the catalog.
    let mut out: Vec<ChatSummary> = read_persisted_chats()
        .into_iter()
        .map(|pc| ChatSummary {
            id: pc.id,
            persona_slug: pc.persona_slug,
            model_name: pc.model_name,
            created_at: pc.created_at,
            turn_count: pc
                .messages
                .iter()
                .filter(|m| matches!(m, ChatMessageWire::User { .. }))
                .count(),
        })
        .collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(out).into_response()
}

/// Read and parse every `<data>/chats/*.json` into [`PersistedChat`]s.
/// Malformed files are logged and skipped. Shared by the list endpoint and
/// startup load.
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

async fn post_create_chat(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<CreateChatRequest>,
) -> Response {
    // Validate persona exists before creating a chat — fail fast instead of
    // surfacing the error mid-stream.
    if Persona::load(&req.persona_slug, &paths::personas_dir()).is_err() {
        return err_json(
            StatusCode::BAD_REQUEST,
            format!("persona '{}' not found", req.persona_slug),
        );
    }
    let id = uuid::Uuid::new_v4().to_string();
    let model_name = req.model_name.unwrap_or_else(crate::runtime::configured_default_model);
    let diagnostics = DiagnosticsLogger::new().ok().map(Arc::new);
    // The OTLP trace shares the diagnostics session-dir name so traces/<id> and
    // sessions/<id> line up. Best-effort, like diagnostics: never block a chat.
    let trace = trace_for(diagnostics.as_deref(), &model_name);
    let session = ChatSession {
        id: id.clone(),
        persona_slug: req.persona_slug,
        model_name: model_name.clone(),
        cwd: state.cwd.clone(),
        preset: SessionPreset::Workshop,
        state: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        diagnostics,
        trace,
        busy: false,
        pending: std::collections::VecDeque::new(),
    };
    let session_arc = Arc::new(Mutex::new(session));
    {
        let s = session_arc.lock().await;
        if let Some(logger) = &s.diagnostics {
            if let Ok(persona) = Persona::load(&s.persona_slug, &paths::personas_dir()) {
                let system_prompt = persona.build_system_prompt(&paths::skills_dir(), &s.cwd);
                logger.log_session_info(
                    &persona.name,
                    &s.persona_slug,
                    &s.model_name,
                    &s.cwd,
                    &system_prompt,
                    &persona.resolved_tool_names(),
                    &persona.skills,
                    true,
                    None,
                );
            }
        }
    }
    state.chats.lock().await.insert(id.clone(), session_arc.clone());
    persist_chat(&session_arc).await;
    let s = session_arc.lock().await;
    Json(ChatSummary {
        id: s.id.clone(),
        persona_slug: s.persona_slug.clone(),
        model_name: s.model_name.clone(),
        created_at: s.created_at.clone(),
        turn_count: 0,
    })
    .into_response()
}

async fn get_chat(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let chats = state.chats.lock().await;
    let Some(session) = chats.get(&id).cloned() else {
        return err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"));
    };
    drop(chats);
    let s = session.lock().await;
    let messages = s
        .state
        .as_ref()
        .map(|st| st.messages.iter().map(ChatMessageWire::from).collect())
        .unwrap_or_default();
    Json(ChatDetail {
        id: s.id.clone(),
        persona_slug: s.persona_slug.clone(),
        model_name: s.model_name.clone(),
        created_at: s.created_at.clone(),
        messages,
    })
    .into_response()
}

async fn delete_chat(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let mut chats = state.chats.lock().await;
    if chats.remove(&id).is_some() {
        drop(chats);
        remove_chat_file(&id);
        StatusCode::NO_CONTENT.into_response()
    } else {
        err_json(StatusCode::NOT_FOUND, format!("chat '{id}' not found"))
    }
}

#[derive(Deserialize)]
struct ChatTurnRequest {
    message: String,
}

/// SSE event wire format. One JSON object per event. The `kind` field
/// discriminates; payloads vary by kind. Events form a lifecycle:
///   `turn_started` → (`llm_started` → `llm_completed`
///                   → `tool_started`* → `tool_completed`*)+
///                   → `done`
/// (`tool_started` and `tool_completed` can repeat per LLM step.)
#[derive(Serialize, Clone)]
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
    /// The agent's user-facing reply, produced by a `say_to_user` tool call.
    /// In tool-only mode this — not free-text `LlmCompleted` content — is the
    /// assistant's message; the workshop renders it as the reply bubble. The
    /// underlying `say_to_user` tool start/finish events are suppressed so the
    /// reply isn't also shown as a raw tool card.
    Reply { content: String },
    /// Terminal event. `status` is "completed" | "interrupted" | "failed".
    Done {
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Run one turn against the chat session. Streams new messages as Server-Sent
/// Events as the agent steps; closes the connection when the executor returns.
#[axum::debug_handler]
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
    let (persona_slug, model_name, cwd, agent_state, turn_index, diagnostics, trace) = {
        let mut s = session.lock().await;
        if s.busy {
            return err_json(StatusCode::CONFLICT, "chat is already mid-turn");
        }
        s.busy = true;
        let prior_turns = s
            .state
            .as_ref()
            .map(|st| st.messages.iter().filter(|m| matches!(m, AgentMessage::User(_))).count())
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
                    logger.log_session_info(
                        &persona.name,
                        &s.persona_slug,
                        &s.model_name,
                        &s.cwd,
                        &system_prompt,
                        &persona.tools,
                        &persona.skills,
                        true,
                        None,
                    );
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
            Arc::new(move |state: &AgentState, _ev| {
                if let Some(logger) = &diagnostics {
                    logger.log_turn(state);
                }
                let mut guard = seen.lock().unwrap();
                if *guard >= state.messages.len() {
                    return GuardAction::Continue;
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
                let mut new_tool_results: Vec<(String, String, ChatMessageWire, String)> = Vec::new();

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
                        AgentMessage::ToolResult { id, name, result, .. } => {
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
                        // `say_to_user` is surfaced as a `Reply` (emitted by the
                        // reply sink during execution), not as a tool card.
                        if name != "say_to_user" {
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
                    // See the matching suppression for `ToolStarted`.
                    if name != "say_to_user" {
                        let _ = tx.try_send(ChatEvent::ToolCompleted {
                            tool_call_id,
                            name,
                            duration_ms,
                            result,
                        });
                    }
                }

                GuardAction::Continue
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
            Arc::new(move |content: String| {
                let tx = tx.clone();
                Box::pin(async move {
                    tx.send(ChatEvent::Reply { content })
                        .await
                        .map_err(|e| e.to_string())
                })
            })
        };

        let outcome = run_chat_turn(
            &context,
            &persona_slug,
            &cwd,
            &model_name,
            agent_state,
            step_guard,
            Some(llm_call_hook),
            Some(llm_response_hook),
            RuntimeOptions {
                reply_sink: Some(reply_sink),
                tool_choice: metalcraft::ToolChoice::Required,
                terminal_tools: vec!["say_to_user".to_string()],
                // A follow-up armed during this turn is delivered back to this
                // chat when it fires.
                session_binding: Some(crate::scheduled_tasks::IoBinding::WorkshopChat {
                    chat_id: id.clone(),
                }),
                reschedule_depth: 0,
            },
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
                    let _ = tx.send(ChatEvent::Done {
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
                    let _ = tx.send(ChatEvent::Done {
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
                    let _ = tx.send(ChatEvent::Done {
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
                    let _ = tx.send(ChatEvent::Done {
                        status: "failed".into(),
                        reason: Some(reason),
                    })
                    .await;
                }
            }
        }
        persist_chat(&session_for_task).await;
    });

    let stream = ReceiverStream::new(rx).map(|ev| -> Result<Event, Infallible> {
        Ok(Event::default().json_data(&ev).unwrap_or_else(|_| {
            Event::default().data("{\"kind\":\"done\",\"status\":\"failed\",\"reason\":\"serialize\"}")
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
async fn get_chat_events(State(_state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    let sender = chat_event_sender(&id).await;
    let rx = sender.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|res| async move {
        match res {
            Ok(ev) => Some(Ok::<Event, Infallible>(
                Event::default().json_data(&ev).unwrap_or_else(|_| {
                    Event::default().data("{\"kind\":\"done\",\"status\":\"failed\",\"reason\":\"serialize\"}")
                }),
            )),
            // A lagged subscriber just skips missed events rather than erroring.
            Err(_) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new()).into_response()
}

/// List scheduled follow-ups (pending + recently completed), newest first.
async fn list_scheduled_tasks(State(_state): State<Arc<ApiState>>) -> Response {
    Json(crate::scheduled_tasks::list()).into_response()
}

/// Cancel a pending scheduled follow-up.
async fn delete_scheduled_task(
    State(_state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Response {
    match crate::scheduled_tasks::cancel(&id) {
        Ok(true) => (StatusCode::OK, Json(serde_json::json!({ "cancelled": true }))).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "no pending follow-up with that id"),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
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
    let (persona_slug, model_name, cwd, agent_state, diagnostics) = {
        let mut s = session.lock().await;
        if s.busy {
            return FollowupDelivery::ChatBusy;
        }
        s.busy = true;
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
        )
    };

    // Reply sink: publish the agent's say_to_user text to the chat's live bus.
    let sender = chat_event_sender(chat_id).await;
    let reply_sink: crate::tools::ReplySink = {
        let sender = sender.clone();
        Arc::new(move |content: String| {
            let sender = sender.clone();
            Box::pin(async move {
                // A send error just means no live subscriber; the reply is still
                // persisted below, so that's not a failure.
                let _ = sender.send(ChatEvent::Reply { content });
                Ok(())
            })
        })
    };

    let step_guard =
        crate::guard::build_agent_guard(crate::guard::GuardConfig::default(), diagnostics.clone());
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
        agent_state,
        step_guard,
        llm_call_hook,
        None,
        RuntimeOptions {
            reply_sink: Some(reply_sink),
            tool_choice: metalcraft::ToolChoice::Required,
            terminal_tools: vec!["say_to_user".to_string()],
            session_binding: Some(crate::scheduled_tasks::IoBinding::WorkshopChat {
                chat_id: chat_id.to_string(),
            }),
            // A follow-up may schedule one more; the tool caps the chain depth.
            reschedule_depth: 0,
        },
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

    FollowupDelivery::Delivered
}

async fn run_chat_turn(
    context: &AgentRuntimeContext,
    persona_slug: &str,
    cwd: &str,
    model_name: &str,
    initial_state: AgentState,
    step_guard: StepGuard<AgentState>,
    llm_call_hook: Option<metalcraft::LlmCallHook>,
    llm_response_hook: Option<metalcraft::LlmResponseHook>,
    options: crate::runtime::RuntimeOptions,
) -> Result<RunOutcome<AgentState>, Box<dyn std::error::Error + Send + Sync>> {
    use crate::runtime::build_agent_runtime;
    use rig::client::CompletionClient;
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

// ── Integration pack handlers ───────────────────────────────────────────

#[derive(Serialize)]
struct IntegrationPackSummary {
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

#[derive(Serialize)]
struct IntegrationPackDetail {
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

async fn list_integration_packs() -> Json<Vec<IntegrationPackSummary>> {
    let state = crate::integration_packs::load_state();
    let packs = crate::integration_packs::list_installed();
    let summaries = packs
        .into_iter()
        .map(|p| IntegrationPackSummary {
            enabled: state.get(&p.manifest.id).map(|s| s.enabled).unwrap_or(false),
            personas: count_files(&p.personas_dir(), "json"),
            skills: count_files(&p.skills_dir(), "md"),
            // Declarative HTTP-API tools (api_tools/*.json) plus any native Rust
            // tools the pack contributes (e.g. digitalocean_spaces' S3 tools,
            // which ship no api_tools/ files). See `tools::native_pack_tool_names`.
            api_tools: count_files(&p.api_tools_dir(), "json")
                + crate::tools::native_pack_tool_names(&p.manifest.id).len(),
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

async fn get_integration_pack(Path(id): Path<String>) -> Response {
    let Some(pack) = crate::integration_packs::list_installed()
        .into_iter()
        .find(|p| p.manifest.id == id)
    else {
        return err_json(StatusCode::NOT_FOUND, format!("pack '{id}' not found"));
    };
    let enabled = crate::integration_packs::is_enabled(&id);
    // Read file lists before moving the manifest fields out of `pack`.
    let personas = list_file_stems(&pack.personas_dir(), "json");
    let skills = list_file_stems(&pack.skills_dir(), "md");
    // Declarative HTTP-API tools plus any native Rust tools the pack ships (see
    // the summary builder above and `tools::native_pack_tool_names`).
    let mut api_tools = list_file_stems(&pack.api_tools_dir(), "json");
    api_tools.extend(crate::tools::native_pack_tool_names(&id));
    api_tools.sort();
    let flow_templates = list_file_stems(&pack.flow_templates_dir(), "json");
    Json(IntegrationPackDetail {
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

#[derive(Deserialize)]
struct SetEnabledRequest {
    enabled: bool,
}

async fn put_pack_enabled(
    Path(id): Path<String>,
    Json(req): Json<SetEnabledRequest>,
) -> Response {
    match crate::integration_packs::set_enabled(&id, req.enabled) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

// ── Gateway channel handlers ────────────────────────────────────────────
//
// Channel *types* are read-only declarative manifests (serialized straight from
// `gateway_channels::ChannelType`). Channel *instances* are user-created configs
// (`gateway_channels::ChannelInstance`) — listed, created, edited, enabled, and
// deleted here. Inbound messages flow through the unauthenticated webhook
// handlers further below.

/// List the installed gateway channel types (with their per-instance settings
/// schema), so the workshop can render a "new channel" form.
async fn list_gateway_types() -> Json<Vec<crate::gateway_channels::ChannelType>> {
    Json(crate::gateway_channels::list_types())
}

/// List all configured gateway channel instances.
async fn list_gateway_channels() -> Json<Vec<crate::gateway_channels::ChannelInstance>> {
    Json(crate::gateway_channels::load_instances())
}

/// Recent inbound/outbound activity for a single channel (newest first).
async fn list_gateway_channel_events(
    Path(id): Path<String>,
) -> Json<Vec<crate::gateway_activity::GatewayEvent>> {
    Json(crate::gateway_activity::list(Some(&id), 200))
}

/// Recent gateway activity across all channels, including inbound messages that
/// matched no channel (the global Network view). Newest first.
async fn list_gateway_activity() -> Json<Vec<crate::gateway_activity::GatewayEvent>> {
    Json(crate::gateway_activity::list(None, 300))
}

// ── Metalcraft Gateway: zero-copy connect ────────────────────────────────────

/// Registration/verification/connection state for the workshop's Connect panel.
async fn gateway_metalcraft_status() -> Json<crate::metalcraft_gateway::GatewayStatus> {
    Json(crate::metalcraft_gateway::status().await)
}

#[derive(Deserialize)]
struct MgRegisterRequest {
    phone_number: String,
}

/// Inline register: proxy to the gateway with the pod's token; returns `verify_code`.
async fn gateway_metalcraft_register(Json(req): Json<MgRegisterRequest>) -> Response {
    match crate::metalcraft_gateway::register(&req.phone_number).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct MgConnectRequest {
    /// Override for the pod's public URL when `POD_PUBLIC_URL` isn't injected.
    #[serde(default)]
    webhook_base: Option<String>,
}

/// Connect: fetch config + wire the webhook + enable the channel. 409 until verified.
async fn gateway_metalcraft_connect(Json(req): Json<MgConnectRequest>) -> Response {
    match crate::metalcraft_gateway::connect(req.webhook_base).await {
        Ok(r) => Json(r).into_response(),
        Err(e) if e == crate::metalcraft_gateway::VERIFY_REQUIRED => {
            err_json(StatusCode::CONFLICT, "Register and verify your phone number before connecting")
        }
        Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct CreateGatewayChannelRequest {
    type_id: String,
    name: String,
    #[serde(default)]
    settings: HashMap<String, String>,
}

async fn post_create_gateway_channel(Json(req): Json<CreateGatewayChannelRequest>) -> Response {
    match crate::gateway_channels::create_instance(&req.type_id, &req.name, req.settings) {
        Ok(instance) => (StatusCode::CREATED, Json(instance)).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct UpdateGatewayChannelRequest {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    settings: HashMap<String, String>,
}

async fn put_gateway_channel(
    Path(id): Path<String>,
    Json(req): Json<UpdateGatewayChannelRequest>,
) -> Response {
    match crate::gateway_channels::update_instance(&id, &req.name, req.enabled, req.settings) {
        Ok(instance) => Json(instance).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

async fn put_gateway_channel_enabled(
    Path(id): Path<String>,
    Json(req): Json<SetEnabledRequest>,
) -> Response {
    match crate::gateway_channels::set_enabled(&id, req.enabled) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

async fn delete_gateway_channel(Path(id): Path<String>) -> Response {
    match crate::gateway_channels::delete_instance(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, format!("channel '{id}' not found")),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ── Inbound gateway webhooks ─────────────────────────────────────────────

/// Cap concurrent agent runs triggered by inbound webhooks so a burst of
/// messages can't spawn unbounded tasks.
const MAX_WEBHOOK_TASKS: usize = 4;

fn webhook_semaphore() -> &'static std::sync::Arc<tokio::sync::Semaphore> {
    static SEM: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
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

/// Inbound Twilio WhatsApp webhook (form-encoded). Validates Twilio's signature
/// when `TWILIO_WEBHOOK_BASE_URL` is configured, parses the message, routes it
/// to the enabled channel instance whose `from` number received it, and runs
/// that channel's persona to reply (via the `whatsapp_send_message` tool).
///
/// Always returns 200 for accepted-but-unroutable cases so Twilio doesn't retry
/// a message we've deliberately ignored.
async fn handle_twilio_webhook(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> StatusCode {
    // Optional signature verification. We can only validate against the exact
    // public URL Twilio signed, so this is gated on TWILIO_WEBHOOK_BASE_URL
    // being set to that URL's origin (e.g. https://agent.example.com).
    if let Some(base) = std::env::var("TWILIO_WEBHOOK_BASE_URL").ok().filter(|s| !s.is_empty()) {
        let token = crate::key_store::lookup("TWILIO_AUTH_TOKEN").unwrap_or_default();
        let signature = headers
            .get("x-twilio-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let url = format!("{}/webhook/twilio", base.trim_end_matches('/'));
        if token.is_empty() || !crate::tools::twilio::validate_signature(&token, &url, &form, signature) {
            log::warn!("Rejected Twilio webhook: invalid or missing X-Twilio-Signature");
            crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
                direction: "inbound".into(),
                platform: "twilio".into(),
                outcome: "signature_rejected".into(),
                detail: Some("invalid or missing X-Twilio-Signature".into()),
                ..Default::default()
            });
            return StatusCode::FORBIDDEN;
        }
    }

    let Some(inbound) = crate::tools::twilio::parse_inbound(&form) else {
        // Status callback or non-message payload — nothing to do.
        return StatusCode::OK;
    };

    let Some(channel) = crate::gateway_channels::resolve_by_inbound_to(&inbound.to) else {
        log::warn!(
            "Twilio webhook for '{}' matched no enabled WhatsApp channel — ignoring",
            inbound.to
        );
        crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
            direction: "inbound".into(),
            platform: "twilio".into(),
            from: Some(inbound.from.clone()),
            from_name: inbound.profile_name.clone(),
            to: Some(inbound.to.clone()),
            body: crate::gateway_activity::truncate_body(&inbound.body),
            outcome: "no_matching_channel".into(),
            detail: Some(format!("no enabled WhatsApp channel for number {}", inbound.to)),
            ..Default::default()
        });
        return StatusCode::OK;
    };

    // Default to the orchestrator (which delegates to specialists) when a
    // channel doesn't pin a specific persona.
    let persona_slug = channel
        .setting("persona")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "orchestrator-agent".to_string());
    let model_name = channel
        .setting("model")
        .map(str::to_string)
        .unwrap_or_else(crate::runtime::configured_default_model);
    let reply_from = channel.setting("from").unwrap_or(&inbound.to).to_string();

    crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
        direction: "inbound".into(),
        platform: "twilio".into(),
        from: Some(inbound.from.clone()),
        from_name: inbound.profile_name.clone(),
        to: Some(inbound.to.clone()),
        body: crate::gateway_activity::truncate_body(&inbound.body),
        channel_id: Some(channel.id.clone()),
        channel_name: Some(channel.name.clone()),
        outcome: "routed".into(),
        detail: Some(format!("persona {persona_slug}")),
        ..Default::default()
    });

    let adapter = crate::gateway_channels::find_type(&channel.type_id)
        .map(|t| t.adapter)
        .unwrap_or_else(|| channel.type_id.clone());

    dispatch_inbound(
        state,
        NormalizedInbound {
            channel_instance_id: channel.id.clone(),
            channel_name: channel.name.clone(),
            adapter,
            persona_slug,
            model_name,
            sender: inbound.from.clone(),
            sender_name: inbound.profile_name.clone(),
            from: Some(reply_from),
            body: inbound.body.clone(),
        },
    )
    .await
}

/// Inbound PipeStreamr webhook (JSON `message.created`). Validates the
/// `X-PipeStreamr-Signature` (HMAC-SHA256 over the raw body) when
/// `PIPESTREAMR_WEBHOOK_SECRET` is set, routes the message to the enabled
/// channel whose `from` number received it, and runs that channel's persona to
/// reply (via `gateway_send_message` with platform "pipestreamr"). Returns 200
/// for accepted-but-unroutable cases so PipeStreamr doesn't retry.
async fn handle_pipestreamr_webhook(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    // Fail-closed: require a configured signing secret and a valid signature.
    match crate::key_store::lookup("PIPESTREAMR_WEBHOOK_SECRET").filter(|s| !s.is_empty()) {
        Some(secret) => {
            let signature = headers
                .get("x-pipestreamr-signature")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if !crate::tools::pipestreamr::validate_signature(&secret, &body, signature) {
                log::warn!("Rejected PipeStreamr webhook: invalid or missing X-PipeStreamr-Signature");
                crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
                    direction: "inbound".into(),
                    platform: "pipestreamr".into(),
                    outcome: "signature_rejected".into(),
                    detail: Some("invalid or missing X-PipeStreamr-Signature".into()),
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
                    "Rejected PipeStreamr webhook: PIPESTREAMR_WEBHOOK_SECRET is not set. \
                     Set it to the secret configured on your PipeStreamr webhook (or set \
                     GATEWAY_ALLOW_UNSIGNED=1 for local testing only)."
                );
                return StatusCode::FORBIDDEN;
            }
            log::warn!(
                "Accepting UNSIGNED PipeStreamr webhook because GATEWAY_ALLOW_UNSIGNED is set — \
                 do not use this in production."
            );
        }
    }

    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let Some(inbound) = crate::tools::pipestreamr::parse_inbound(&payload) else {
        // Not an inbound message (log/status/outbound echo) — nothing to do.
        return StatusCode::OK;
    };

    // Route on the PipeStreamr integration UUID (`source_id`) — stable and
    // unique, unlike phone-number matching — against each channel's
    // `integration_id` setting.
    let Some(source_id) = inbound.source_id.clone() else {
        log::warn!("PipeStreamr webhook has no source_id; cannot route — ignoring");
        crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
            direction: "inbound".into(),
            platform: "pipestreamr".into(),
            from: Some(inbound.from.clone()),
            from_name: inbound.from_name.clone(),
            body: crate::gateway_activity::truncate_body(&inbound.body),
            outcome: "no_matching_channel".into(),
            detail: Some("webhook had no source_id to route on".into()),
            ..Default::default()
        });
        return StatusCode::OK;
    };
    let Some(channel) = crate::gateway_channels::resolve_by_setting("integration_id", &source_id) else {
        log::warn!("PipeStreamr webhook for integration '{source_id}' matched no enabled channel — ignoring");
        crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
            direction: "inbound".into(),
            platform: "pipestreamr".into(),
            from: Some(inbound.from.clone()),
            from_name: inbound.from_name.clone(),
            body: crate::gateway_activity::truncate_body(&inbound.body),
            source_id: Some(source_id.clone()),
            outcome: "no_matching_channel".into(),
            detail: Some(format!(
                "no enabled channel has integration_id = {source_id}"
            )),
            ..Default::default()
        });
        return StatusCode::OK;
    };

    // The orchestrator delegates to specialist personas as needed, so it's the
    // sensible default when a channel doesn't pin a specific persona.
    let persona_slug = channel
        .setting("persona")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "orchestrator-agent".to_string());
    let model_name = channel
        .setting("model")
        .map(str::to_string)
        .unwrap_or_else(crate::runtime::configured_default_model);

    crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
        direction: "inbound".into(),
        platform: "pipestreamr".into(),
        from: Some(inbound.from.clone()),
        from_name: inbound.from_name.clone(),
        body: crate::gateway_activity::truncate_body(&inbound.body),
        source_id: Some(source_id.clone()),
        channel_id: Some(channel.id.clone()),
        channel_name: Some(channel.name.clone()),
        outcome: "routed".into(),
        detail: Some(format!("persona {persona_slug}")),
        ..Default::default()
    });

    let adapter = crate::gateway_channels::find_type(&channel.type_id)
        .map(|t| t.adapter)
        .unwrap_or_else(|| channel.type_id.clone());

    dispatch_inbound(
        state,
        NormalizedInbound {
            channel_instance_id: channel.id.clone(),
            channel_name: channel.name.clone(),
            adapter,
            persona_slug,
            model_name,
            sender: inbound.from.clone(),
            sender_name: inbound.from_name.clone(),
            // Reply back through the same integration that received the message.
            from: Some(source_id.clone()),
            body: inbound.body.clone(),
        },
    )
    .await
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
    channel_instance_id: String,
    channel_name: String,
    /// Native send adapter, e.g. `"pipestreamr"` / `"twilio"`.
    adapter: String,
    persona_slug: String,
    model_name: String,
    /// The sender — the counterparty replies are sent back to.
    sender: String,
    sender_name: Option<String>,
    /// Outbound sender identity (integration id for PipeStreamr, our number for
    /// Twilio). Passed to the adapter's `send`.
    from: Option<String>,
    body: String,
}

/// Deterministic chat id for a gateway conversation: one chat per sender per
/// channel instance, stable across restarts (the persisted file is named by id,
/// so `load_persisted_chats` rehydrates the same conversation). Filename- and
/// URL-safe: phone numbers reduce to digits; other ids keep ascii alphanumerics.
fn gateway_chat_id(channel_instance_id: &str, sender: &str) -> String {
    let digits = crate::gateway_channels::normalize_number(sender);
    let suffix = if digits.is_empty() {
        let s: String = sender.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        if s.is_empty() { "anon".to_string() } else { s.to_ascii_lowercase() }
    } else {
        digits
    };
    format!("gw-{channel_instance_id}-{suffix}")
}

/// Build the reply sink for a gateway session: `say_to_user` text is sent back
/// out through the bound adapter to the original sender, and the send is logged
/// to the gateway activity feed (mirroring `gateway::record_outbound`).
fn gateway_reply_sink(
    adapter: String,
    recipient: String,
    from: Option<String>,
    channel_id: String,
    channel_name: String,
) -> crate::tools::ReplySink {
    Arc::new(move |content: String| {
        let adapter = adapter.clone();
        let recipient = recipient.clone();
        let from = from.clone();
        let channel_id = channel_id.clone();
        let channel_name = channel_name.clone();
        Box::pin(async move {
            let result = match adapter.as_str() {
                "pipestreamr" => {
                    crate::tools::pipestreamr::send(&recipient, &content, from.as_deref()).await
                }
                "twilio" => {
                    crate::tools::twilio::send_whatsapp(&recipient, &content, from.as_deref()).await
                }
                other => Err(format!("no send adapter for '{other}'")),
            };
            let (outcome, detail) = match &result {
                Ok(_) => ("sent", None),
                Err(e) => ("send_failed", Some(e.clone())),
            };
            crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
                direction: "outbound".into(),
                platform: adapter.clone(),
                from: from.clone(),
                to: Some(recipient.clone()),
                body: crate::gateway_activity::truncate_body(&content),
                source_id: from.clone(),
                channel_id: Some(channel_id.clone()),
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
async fn get_or_create_gateway_session(
    state: &Arc<ApiState>,
    n: &NormalizedInbound,
    chat_id: &str,
) -> Arc<Mutex<ChatSession>> {
    {
        let chats = state.chats.lock().await;
        if let Some(existing) = chats.get(chat_id) {
            return existing.clone();
        }
    }
    let diagnostics = DiagnosticsLogger::new().ok().map(Arc::new);
    if let Some(logger) = &diagnostics {
        if let Ok(persona) = Persona::load(&n.persona_slug, &paths::personas_dir()) {
            let system_prompt = persona.build_system_prompt(&paths::skills_dir(), &state.cwd);
            logger.log_session_info(
                &persona.name,
                &n.persona_slug,
                &n.model_name,
                &state.cwd,
                &system_prompt,
                &persona.resolved_tool_names(),
                &persona.skills,
                true,
                None,
            );
        }
    }
    let session = ChatSession {
        id: chat_id.to_string(),
        persona_slug: n.persona_slug.clone(),
        model_name: n.model_name.clone(),
        cwd: state.cwd.clone(),
        preset: SessionPreset::Gateway {
            channel_instance_id: n.channel_instance_id.clone(),
            adapter: n.adapter.clone(),
            recipient: n.sender.clone(),
            from: n.from.clone(),
        },
        state: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        diagnostics,
        trace: None,
        busy: false,
        pending: std::collections::VecDeque::new(),
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
    let (persona_slug, model_name, cwd, agent_state, diagnostics) = {
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
                    logger.log_session_info(
                        &persona.name,
                        &s.persona_slug,
                        &s.model_name,
                        &s.cwd,
                        &system_prompt,
                        &persona.resolved_tool_names(),
                        &persona.skills,
                        true,
                        None,
                    );
                }
                s.diagnostics = Some(logger);
            }
        }
        (
            s.persona_slug.clone(),
            s.model_name.clone(),
            s.cwd.clone(),
            next_state,
            s.diagnostics.clone(),
        )
    };

    let state_before_turn = agent_state.clone();

    let llm_call_hook: Option<metalcraft::LlmCallHook> = diagnostics.as_ref().map(|d| {
        let logger = d.clone();
        Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
            logger.log_llm_request(snapshot);
        }) as metalcraft::LlmCallHook
    });
    let step_guard =
        crate::guard::build_agent_guard(crate::guard::GuardConfig::default(), diagnostics.clone());

    let outcome = run_chat_turn(
        context,
        &persona_slug,
        &cwd,
        &model_name,
        agent_state,
        step_guard,
        llm_call_hook,
        None,
        RuntimeOptions {
            reply_sink: Some(sink),
            tool_choice: metalcraft::ToolChoice::Required,
            terminal_tools: vec!["say_to_user".to_string()],
            // Gateway follow-up delivery (rebuilding the adapter sink at fire
            // time from the channel binding) is wired in the delivery pass; for
            // now a follow-up armed in a gateway turn is unbound (logged).
            session_binding: None,
            reschedule_depth: 0,
        },
    )
    .await;

    {
        let mut s = session.lock().await;
        match outcome {
            Ok(RunOutcome::Completed(st)) => s.state = Some(st),
            Ok(RunOutcome::Interrupted { state: st, .. }) => s.state = Some(st),
            Ok(RunOutcome::Failed { state: st, node, error }) => {
                if let Some(logger) = &s.diagnostics {
                    logger.log_error(&format!("{node}: {error}"));
                }
                s.state = Some(st);
            }
            Err(e) => {
                // Framework error with no recoverable state: roll back so the
                // conversation history isn't wiped.
                if let Some(logger) = &s.diagnostics {
                    logger.log_error(&error_chain(e.as_ref()));
                }
                s.state = Some(state_before_turn);
            }
        }
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

    let chat_id = gateway_chat_id(&n.channel_instance_id, &n.sender);
    let session = get_or_create_gateway_session(&state, &n, &chat_id).await;

    // Claim the turn. If one is already running for this sender, enqueue the
    // body so it's processed when the in-flight turn finishes (no lost messages).
    {
        let mut s = session.lock().await;
        if s.busy {
            s.pending.push_back(n.body.clone());
            log::info!("Inbound from {} queued — chat {chat_id} is mid-turn", n.sender);
            crate::gateway_activity::record(crate::gateway_activity::GatewayEvent {
                direction: "inbound".into(),
                platform: n.adapter.clone(),
                from: Some(n.sender.clone()),
                from_name: n.sender_name.clone(),
                body: crate::gateway_activity::truncate_body(&n.body),
                channel_id: Some(n.channel_instance_id.clone()),
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
            s.pending.push_back(n.body.clone());
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
        n.channel_instance_id.clone(),
        n.channel_name.clone(),
    );
    tokio::spawn(async move {
        let _permit = permit;
        let mut body = n.body.clone();
        loop {
            run_one_gateway_turn(&session, &context, sink.clone(), body).await;
            let mut s = session.lock().await;
            match s.pending.pop_front() {
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
mod gateway_tests {
    use super::gateway_chat_id;

    #[test]
    fn chat_id_is_deterministic_per_sender_and_channel() {
        let a = gateway_chat_id("chan-1", "+1 (555) 000-1234");
        let b = gateway_chat_id("chan-1", "whatsapp:+15550001234");
        // Same sender (modulo formatting) on the same channel → same chat,
        // so a conversation accumulates and survives restarts.
        assert_eq!(a, b);
        assert_eq!(a, "gw-chan-1-15550001234");
    }

    #[test]
    fn chat_id_separates_senders_and_channels() {
        assert_ne!(
            gateway_chat_id("chan-1", "+15550001234"),
            gateway_chat_id("chan-1", "+15550009999")
        );
        assert_ne!(
            gateway_chat_id("chan-1", "+15550001234"),
            gateway_chat_id("chan-2", "+15550001234")
        );
    }

    #[test]
    fn chat_id_handles_non_numeric_senders() {
        let id = gateway_chat_id("chan-1", "user@example.com");
        assert_eq!(id, "gw-chan-1-userexamplecom");
        // Empty/symbol-only sender falls back to a stable placeholder.
        assert_eq!(gateway_chat_id("chan-1", "!!!"), "gw-chan-1-anon");
    }
}
