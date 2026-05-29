use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::persona::{Persona, PersonaSummary};
use crate::tools::http_api::HttpApiToolConfig;

/// Configuration for the workshop API server.
pub struct WorkshopApiConfig {
    pub port: u16,
    pub api_key: String,
}

struct ApiState {
    api_key: String,
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
    layout: ProjectLayout,
}

#[derive(Serialize)]
struct ProjectLayout {
    data_dir: String,
    personas_dir: String,
    skills_dir: String,
    flows_dir: String,
    logs_dir: String,
    api_tools_dir: String,
}

#[derive(Serialize)]
struct SkillSummary {
    slug: String,
    description: String,
}

#[derive(Serialize, Deserialize)]
struct Skill {
    slug: String,
    description: String,
    body: String,
}

#[derive(Serialize)]
struct DiagnosticsSessionSummary {
    id: String,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    persona_slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_name: Option<String>,
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
/// `metalcraft-flowd --api` mounts it alongside the event listener and the
/// flow scheduler.
pub fn build_router(api_key: String) -> Router {
    let state = Arc::new(ApiState { api_key });

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
        .route("/api/v1/diagnostics", get(list_diagnostics))
        .route("/api/v1/diagnostics/{id}", get(get_diagnostics_session))
        .route("/api/v1/api-tools", get(list_api_tools))
        .route("/api/v1/api-tools/{name}", get(get_api_tool))
        .route("/api/v1/api-tools/{name}", put(put_api_tool))
        .route("/api/v1/api-tools/{name}", delete(delete_api_tool))
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
    let personas = Persona::list_summaries(&paths::personas_dir());
    let skills = list_skill_summaries();
    let flows = metalcraft_flows::list_flows(&paths::flows_dir());
    let sessions = list_diagnostics_sessions();
    let api_tools = list_api_tool_summaries();

    Json(ProjectSnapshot {
        personas,
        skills,
        flows,
        sessions,
        api_tools,
        layout: ProjectLayout {
            data_dir: paths::data_dir().display().to_string(),
            personas_dir: paths::personas_dir().display().to_string(),
            skills_dir: paths::skills_dir().display().to_string(),
            flows_dir: paths::flows_dir().display().to_string(),
            logs_dir: paths::logs_dir().display().to_string(),
            api_tools_dir: paths::api_tools_dir().display().to_string(),
        },
    })
}

// ── Persona handlers ────────────────────────────────────────────────────

async fn get_persona(Path(slug): Path<String>) -> Response {
    match Persona::load(&slug, &paths::personas_dir()) {
        Ok(persona) => Json(persona).into_response(),
        Err(_) => err_json(StatusCode::NOT_FOUND, format!("persona '{slug}' not found")),
    }
}

async fn put_persona(Path(slug): Path<String>, Json(persona): Json<Persona>) -> Response {
    match persona.save(&slug, &paths::personas_dir()) {
        Ok(()) => Json(persona).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

async fn delete_persona(Path(slug): Path<String>) -> Response {
    match Persona::delete(&slug, &paths::personas_dir()) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => err_json(StatusCode::NOT_FOUND, format!("persona '{slug}' not found")),
    }
}

// ── Skill handlers ──────────────────────────────────────────────────────

fn list_skill_summaries() -> Vec<SkillSummary> {
    let dir = paths::skills_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return vec![],
    };

    let mut summaries: Vec<SkillSummary> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) == Some("md") {
                let slug = path.file_stem()?.to_str()?.to_string();
                let content = std::fs::read_to_string(&path).ok()?;
                let description = crate::persona::parse_frontmatter_description(&content)
                    .unwrap_or_else(|| "No description".to_string());
                Some(SkillSummary { slug, description })
            } else {
                None
            }
        })
        .collect();

    summaries.sort_by(|a, b| a.slug.cmp(&b.slug));
    summaries
}

fn load_skill(slug: &str) -> Option<Skill> {
    let path = paths::skills_dir().join(format!("{slug}.md"));
    let content = std::fs::read_to_string(&path).ok()?;
    let description = crate::persona::parse_frontmatter_description(&content)
        .unwrap_or_default();
    let body = crate::persona::strip_frontmatter(&content).to_string();
    Some(Skill { slug: slug.to_string(), description, body })
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
    match save_skill(&slug, &skill) {
        Ok(()) => Json(Skill { slug, ..skill }).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e),
    }
}

async fn delete_skill(Path(slug): Path<String>) -> Response {
    let path = paths::skills_dir().join(format!("{slug}.md"));
    if !path.exists() {
        return err_json(StatusCode::NOT_FOUND, format!("skill '{slug}' not found"));
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
    let dir = paths::logs_dir();
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
            let (persona_slug, model_name) = if let Ok(content) = std::fs::read_to_string(&info_path) {
                let info: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
                (
                    info.get("persona_slug").and_then(|v| v.as_str()).map(String::from),
                    info.get("model_name").and_then(|v| v.as_str()).map(String::from),
                )
            } else {
                (None, None)
            };

            Some(DiagnosticsSessionSummary {
                id: dir_name.clone(),
                timestamp: dir_name,
                persona_slug,
                model_name,
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
    let session_dir = paths::logs_dir().join(&id);
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
    let dir = paths::api_tools_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return vec![],
    };

    let mut summaries: Vec<ApiToolSummary> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) == Some("json") {
                let content = std::fs::read_to_string(&path).ok()?;
                let config: HttpApiToolConfig = serde_json::from_str(&content).ok()?;
                Some(ApiToolSummary {
                    name: config.name,
                    description: config.description,
                })
            } else {
                None
            }
        })
        .collect();

    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    summaries
}

async fn list_api_tools() -> Json<Vec<ApiToolSummary>> {
    Json(list_api_tool_summaries())
}

async fn get_api_tool(Path(name): Path<String>) -> Response {
    let path = paths::api_tools_dir().join(format!("{name}.json"));
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
    let path = paths::api_tools_dir().join(format!("{name}.json"));
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
        return err_json(StatusCode::NOT_FOUND, format!("api-tool '{name}' not found"));
    }
    match std::fs::remove_file(&path) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete: {e}")),
    }
}
