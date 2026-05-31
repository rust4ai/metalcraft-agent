use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
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
use crate::flows;
use crate::paths;
use crate::persona::{Persona, PersonaSummary};
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest, DEFAULT_MODEL};
use crate::tools::http_api::HttpApiToolConfig;
use metalcraft::{AgentMessage, AgentState, Executor, GuardAction, RunOutcome, StepGuard};

/// Configuration for the workshop API server.
pub struct WorkshopApiConfig {
    pub port: u16,
    pub api_key: String,
}

/// Active chat sessions, keyed by chat id. Lost on restart — chats live for
/// the lifetime of the daemon process.
type ChatStore = Arc<Mutex<HashMap<String, Arc<Mutex<ChatSession>>>>>;

struct ChatSession {
    id: String,
    persona_slug: String,
    model_name: String,
    cwd: String,
    state: Option<AgentState>,
    created_at: String,
    diagnostics: Option<Arc<DiagnosticsLogger>>,
    /// True while a turn is mid-flight. Prevents two concurrent turns from
    /// stomping on the same state.
    busy: bool,
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
struct SkillSummary {
    slug: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    read_only: bool,
}

#[derive(Serialize, Deserialize)]
struct Skill {
    slug: String,
    description: String,
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_id: Option<String>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    read_only: bool,
}

#[derive(Serialize)]
struct DiagnosticsSessionSummary {
    id: String,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    persona_slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_name: Option<String>,
    /// "session" for a normal one-shot/diagnostics run, "flow" for a flow run.
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// Present (and `kind == "flow"`) when this session was produced by a flow run.
    #[serde(skip_serializing_if = "Option::is_none")]
    flow_id: Option<String>,
    /// Number of `turn_NNN.json` files in the session directory. Counted fresh
    /// from disk on each list so it's correct after the agent appends turns.
    turn_count: usize,
}

#[derive(Serialize)]
struct DiagnosticsSession {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_info: Option<serde_json::Value>,
    timeline: Vec<TimelineEvent>,
}

#[derive(Serialize)]
struct TimelineEvent {
    kind: String,
    file: String,
    data: serde_json::Value,
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
    // Rehydrate chats from disk so they survive restarts.
    let persisted = load_persisted_chats();
    if !persisted.is_empty() {
        log::info!("Loaded {} persisted chat(s) from disk", persisted.len());
    }
    let state = Arc::new(ApiState {
        api_key,
        chats: Arc::new(Mutex::new(persisted)),
        cwd,
    });

    Router::new()
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
        .route("/api/v1/integration-packs", get(list_integration_packs))
        .route("/api/v1/integration-packs/{id}", get(get_integration_pack))
        .route("/api/v1/integration-packs/{id}/enabled", put(put_pack_enabled))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state)
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

fn list_skill_summaries() -> Vec<SkillSummary> {
    let layered = crate::integration_packs::list_files_layered(
        &paths::skills_dir(),
        "skills",
        "md",
    );
    let mut summaries: Vec<SkillSummary> = layered
        .into_iter()
        .filter_map(|(path, origin)| {
            let slug = path.file_stem()?.to_str()?.to_string();
            let content = std::fs::read_to_string(&path).ok()?;
            let description = crate::persona::parse_frontmatter_description(&content)
                .unwrap_or_else(|| "No description".to_string());
            Some(SkillSummary {
                slug,
                description,
                pack_id: origin.pack_id().map(String::from),
                read_only: origin.is_read_only(),
            })
        })
        .collect();
    summaries.sort_by(|a, b| a.slug.cmp(&b.slug));
    summaries
}

fn load_skill(slug: &str) -> Option<Skill> {
    let filename = format!("{slug}.md");
    let (path, origin) = crate::integration_packs::resolve_file(
        &paths::skills_dir(),
        "skills",
        &filename,
    )?;
    let content = std::fs::read_to_string(&path).ok()?;
    let description = crate::persona::parse_frontmatter_description(&content)
        .unwrap_or_default();
    let body = crate::persona::strip_frontmatter(&content).to_string();
    Some(Skill {
        slug: slug.to_string(),
        description,
        body,
        pack_id: origin.pack_id().map(String::from),
        read_only: origin.is_read_only(),
    })
}

fn save_skill(slug: &str, skill: &Skill) -> Result<(), String> {
    let path = paths::skills_dir().join(format!("{slug}.md"));
    let content = format!("---\ndescription: {}\n---\n\n{}", skill.description, skill.body);
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

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

fn list_diagnostics_sessions() -> Vec<DiagnosticsSessionSummary> {
    let dir = paths::sessions_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return vec![],
    };

    let mut sessions: Vec<DiagnosticsSessionSummary> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let dir_name = path.file_name()?.to_str()?.to_string();

            // Try to read session_info.json for metadata
            let info_path = path.join("session_info.json");
            let (persona_slug, model_name, kind, flow_id) =
                if let Ok(content) = std::fs::read_to_string(&info_path) {
                    let info: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
                    (
                        info.get("persona_slug").and_then(|v| v.as_str()).map(String::from),
                        info.get("model_name").and_then(|v| v.as_str()).map(String::from),
                        info.get("kind").and_then(|v| v.as_str()).map(String::from),
                        info.get("flow_id").and_then(|v| v.as_str()).map(String::from),
                    )
                } else {
                    (None, None, None, None)
                };

            // Count turn_NNN.json files so the summary reports how far the
            // session actually got.
            let turn_count = std::fs::read_dir(&path)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| {
                            e.file_name()
                                .to_str()
                                .map(|n| n.starts_with("turn_") && n.ends_with(".json"))
                                .unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0);

            Some(DiagnosticsSessionSummary {
                id: dir_name.clone(),
                timestamp: dir_name,
                persona_slug,
                model_name,
                kind,
                flow_id,
                turn_count,
            })
        })
        .collect();

    sessions.sort_by(|a, b| b.id.cmp(&a.id)); // newest first
    sessions
}

async fn list_diagnostics() -> Json<Vec<DiagnosticsSessionSummary>> {
    Json(list_diagnostics_sessions())
}

async fn get_diagnostics_session(Path(id): Path<String>) -> Response {
    let session_dir = paths::sessions_dir().join(&id);
    if !session_dir.is_dir() {
        return err_json(StatusCode::NOT_FOUND, format!("diagnostics session '{id}' not found"));
    }

    // Read session_info.json
    let session_info = std::fs::read_to_string(session_dir.join("session_info.json"))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok());

    // Read all turn/llm_request/config/compaction files as timeline
    let mut timeline = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&session_dir) {
        let mut files: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        files.sort_by_key(|e| e.file_name());

        for entry in files {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "session_info.json" {
                continue;
            }
            if !name.ends_with(".json") {
                continue;
            }

            let kind = if name.starts_with("turn_") {
                "turn"
            } else if name.starts_with("llm_request_") {
                "llm_request"
            } else if name.contains("compaction") {
                "compaction"
            } else if name.starts_with("error_") {
                "error"
            } else {
                "config_change"
            };

            let data = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|c| serde_json::from_str(&c).ok())
                .unwrap_or(serde_json::Value::Null);

            timeline.push(TimelineEvent {
                kind: kind.to_string(),
                file: name,
                data,
            });
        }
    }

    Json(DiagnosticsSession {
        id,
        session_info,
        timeline,
    })
    .into_response()
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
    let out = crate::integration_packs::recommended_env()
        .into_iter()
        .map(|(name, packs)| RecommendedKey {
            configured: crate::key_store::lookup(&name).is_some(),
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
}

#[derive(Serialize)]
struct RunFlowPromptResult {
    prompt_index: usize,
    status: String, // "completed" | "interrupted" | "failed"
    answer: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct RunFlowResponse {
    flow_id: String,
    prompts: Vec<RunFlowPromptResult>,
}

async fn post_run_flow(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    body: Option<Json<RunFlowRequest>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();

    let flow = match metalcraft_flows::load_flow(&paths::flows_dir(), &id) {
        Some(f) => f,
        None => return err_json(StatusCode::NOT_FOUND, format!("flow '{id}' not found")),
    };

    let prompts = match flows::collect_reachable_prompts(&flow) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, format!("unrunnable flow: {e}")),
    };

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
    let model_name = req.model_name.unwrap_or_else(|| DEFAULT_MODEL.to_string());

    // Create a single session for the whole flow run so it shows
    // up in the Sessions list. All prompts log their turns into this one
    // session directory; prompt boundaries are recorded as config-change
    // events. The session is tagged with the flow id (kind == "flow").
    let logger = match DiagnosticsLogger::new() {
        Ok(l) => {
            if let Ok(persona) = Persona::load(&persona_slug, &context.personas_dir) {
                let system_prompt = persona.build_system_prompt(&context.skills_dir, &state.cwd);
                l.log_session_info(
                    &persona.name,
                    &persona_slug,
                    &model_name,
                    &state.cwd,
                    &system_prompt,
                    &persona.resolved_tool_names(),
                    &persona.skills,
                    true,
                    Some(&id),
                );
            }
            Some(Arc::new(l))
        }
        Err(e) => {
            eprintln!("flow run: failed to create session logger: {e}");
            None
        }
    };

    let mut results = Vec::with_capacity(prompts.len());
    for (i, fp) in prompts.iter().enumerate() {
        // Each prompt node can override the persona; fall back to the
        // request-level persona if it doesn't.
        let effective_persona = fp.persona.as_deref().unwrap_or(&persona_slug);
        if let Some(l) = &logger {
            l.log_config_change(
                "flow_prompt",
                serde_json::json!({
                    "index": i,
                    "persona": effective_persona,
                    "prompt": fp.prompt,
                }),
            );
        }
        let outcome = runtime::run_one_shot_task(
            &context,
            RunOneShotRequest {
                persona_slug: effective_persona,
                cwd: &state.cwd,
                model_name: &model_name,
                task: &fp.prompt,
                approval_mode: ApprovalMode::AutoApprove,
                diagnostics: logger.clone(),
            },
        )
        .await;
        results.push(match outcome {
            Ok(RunOutcome::Completed(s)) => RunFlowPromptResult {
                prompt_index: i,
                status: "completed".into(),
                answer: s.final_answer().map(String::from),
                error: None,
            },
            Ok(RunOutcome::Interrupted { reason, .. }) => RunFlowPromptResult {
                prompt_index: i,
                status: "interrupted".into(),
                answer: None,
                error: Some(reason),
            },
            Ok(RunOutcome::Failed { node, error, .. }) => RunFlowPromptResult {
                prompt_index: i,
                status: "failed".into(),
                answer: None,
                error: Some(format!("{node}: {error}")),
            },
            Err(e) => RunFlowPromptResult {
                prompt_index: i,
                status: "failed".into(),
                answer: None,
                error: Some(e.to_string()),
            },
        });
    }

    Json(RunFlowResponse {
        flow_id: id,
        prompts: results,
    })
    .into_response()
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
            state,
            created_at: pc.created_at,
            diagnostics: None,
            busy: false, // anything that was busy at shutdown couldn't have
                          // finished cleanly; reset so the user can retry.
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
    let model_name = req.model_name.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let session = ChatSession {
        id: id.clone(),
        persona_slug: req.persona_slug,
        model_name: model_name.clone(),
        cwd: state.cwd.clone(),
        state: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        diagnostics: DiagnosticsLogger::new().ok().map(Arc::new),
        busy: false,
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
#[derive(Serialize)]
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
    let (persona_slug, model_name, cwd, agent_state, turn_index, diagnostics) = {
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
        (
            s.persona_slug.clone(),
            s.model_name.clone(),
            s.cwd.clone(),
            next_state,
            prior_turns, // new turn's index = prior count (0-based)
            s.diagnostics.clone(),
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
            Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
                if let Some(logger) = &diagnostics {
                    logger.log_llm_request(snapshot);
                }
                *started.lock().unwrap() = Some(std::time::Instant::now());
                let _ = tx.try_send(ChatEvent::LlmStarted);
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
                let mut new_tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();
                let mut new_tool_results: Vec<(String, String, ChatMessageWire)> = Vec::new();

                for m in new {
                    match m {
                        AgentMessage::Assistant(_) => assistant_msgs.push(ChatMessageWire::from(m)),
                        AgentMessage::ToolCall { id, name, args, .. } => {
                            new_tool_calls.push((id.clone(), name.clone(), args.clone()));
                        }
                        AgentMessage::ToolResult { id, name, .. } => {
                            new_tool_results.push((id.clone(), name.clone(), ChatMessageWire::from(m)));
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
                    let _ = tx.try_send(ChatEvent::LlmCompleted {
                        messages: assistant_msgs,
                        duration_ms,
                    });
                    for (tool_call_id, name, args) in new_tool_calls {
                        tools_in_flight
                            .lock()
                            .unwrap()
                            .insert(tool_call_id.clone(), std::time::Instant::now());
                        let _ = tx.try_send(ChatEvent::ToolStarted {
                            tool_call_id,
                            name,
                            args,
                        });
                    }
                }

                for (tool_call_id, name, result) in new_tool_results {
                    let duration_ms = tools_in_flight
                        .lock()
                        .unwrap()
                        .remove(&tool_call_id)
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    let _ = tx.try_send(ChatEvent::ToolCompleted {
                        tool_call_id,
                        name,
                        duration_ms,
                        result,
                    });
                }

                GuardAction::Continue
            })
        };

        // Keep the pre-turn state so a hard failure can be rolled back. The
        // session's state was `take()`n before this task started, so without
        // this restore a failed turn would silently wipe the entire chat
        // history (the next turn would start from scratch).
        let state_before_turn = agent_state.clone();

        let outcome = run_chat_turn(
            &context,
            &persona_slug,
            &cwd,
            &model_name,
            agent_state,
            step_guard,
            Some(llm_call_hook),
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
                    let _ = tx.send(ChatEvent::Done {
                        status: "completed".into(),
                        reason: None,
                    })
                    .await;
                }
                Ok(RunOutcome::Interrupted { state, reason, .. }) => {
                    s.state = Some(state);
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

async fn run_chat_turn(
    context: &AgentRuntimeContext,
    persona_slug: &str,
    cwd: &str,
    model_name: &str,
    initial_state: AgentState,
    step_guard: StepGuard<AgentState>,
    llm_call_hook: Option<metalcraft::LlmCallHook>,
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
        |client, name| client.completion_model(name),
    )
    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
    let executor = Executor::new_from_arc(runtime.graph)
        .max_steps(90)
        .with_step_guard(step_guard);
    // Box the real error rather than stringifying it, so its `source()` chain
    // survives for `error_chain` to walk when building the failed-turn reason.
    executor
        .run(initial_state, "agent")
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
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
            api_tools: count_files(&p.api_tools_dir(), "json"),
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
    let api_tools = list_file_stems(&pack.api_tools_dir(), "json");
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
