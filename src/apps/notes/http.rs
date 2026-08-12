//! The Notes REST API, nested at `/apps/metalcraft-notes` on the pod's Workshop
//! server. Same paths and shapes as the cloud `metalcraft-notes` service, so the
//! existing SPA and clients work unchanged once pointed at the mount path.
//!
//! These routes carry their **own auth layer** — the pod mounts app routers
//! outside the main Workshop auth middleware, so each app re-checks the pod
//! Bearer token (static `WORKSHOP_API_KEY` or a hub `mck_` token scoped to this
//! pod, via [`crate::hub_auth::verify_pod_bearer`]).
//!
//! The embedded SPA + WebSocket live-push land in a follow-up; this is the REST
//! backend the UI talks to.

use std::collections::HashMap;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use super::store::NotesStore;
use super::NotesError;

impl IntoResponse for NotesError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, Json(json!({ "error": self.message }))).into_response()
    }
}

/// Build the Notes router (auth-layered, state = the pod's [`NotesStore`]).
pub fn router(store: NotesStore) -> Router {
    Router::new()
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/categories", get(list_categories).post(create_category))
        .route("/api/v1/categories/{id}", patch(update_category).delete(delete_category))
        .route("/api/v1/notes", get(list_notes).post(create_note))
        .route("/api/v1/notes/{slug}", get(get_note).patch(update_note).delete(delete_note))
        .route("/api/v1/notes/{slug}/favorite", post(favorite_note))
        .route("/api/v1/favorites", get(list_favorites))
        .route("/api/v1/search", get(search))
        .layer(axum::middleware::from_fn(require_pod_auth))
        .with_state(store)
}

/// Reject requests without a valid pod Bearer token (mirrors the Workshop
/// `auth_middleware`; the static key is read from the pod's environment).
async fn require_pod_auth(headers: HeaderMap, req: Request, next: Next) -> Response {
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let static_key = std::env::var("WORKSHOP_API_KEY").unwrap_or_default();
    let ok = (!static_key.is_empty() && provided == static_key)
        || crate::hub_auth::verify_pod_bearer(provided).await;
    if !ok {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response();
    }
    next.run(req).await
}

// ── request-body helpers ─────────────────────────────────────────────────────

fn body_str<'a>(b: &'a Value, key: &str) -> Option<&'a str> {
    b.get(key).and_then(|v| v.as_str())
}
fn body_present(b: &Value, key: &str) -> bool {
    b.get(key).map_or(false, |v| !v.is_null())
}
fn body_category_ids(b: &Value) -> Option<Vec<String>> {
    b.get("categories")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}

/// Apply schema/seed before serving (idempotent, cheap). Maps failure to a response.
async fn ready(s: &NotesStore) -> Result<(), Response> {
    s.ensure_ready().await.map_err(IntoResponse::into_response)
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn whoami(State(s): State<NotesStore>) -> Response {
    Json(s.whoami()).into_response()
}

async fn list_categories(State(s): State<NotesStore>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_categories().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn create_category(State(s): State<NotesStore>, Json(b): Json<Value>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.create_category(body_str(&b, "name").unwrap_or("")).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn update_category(
    State(s): State<NotesStore>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.update_category(&id, body_str(&b, "name"), body_str(&b, "color")).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_category(State(s): State<NotesStore>, Path(id): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.delete_category(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_notes(State(s): State<NotesStore>, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_notes(q.get("sort").map(String::as_str), q.get("category").map(String::as_str)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn create_note(State(s): State<NotesStore>, Json(b): Json<Value>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.create_note(body_str(&b, "title"), body_str(&b, "body"), body_category_ids(&b)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn get_note(State(s): State<NotesStore>, Path(slug): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.get_note(&slug).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn update_note(
    State(s): State<NotesStore>,
    Path(slug): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    let base_version = b.get("base_version").and_then(|v| v.as_i64());
    match s
        .update_note(
            &slug,
            body_str(&b, "title"),
            body_str(&b, "body"),
            base_version,
            body_category_ids(&b),
            body_present(&b, "title"),
            body_present(&b, "body"),
        )
        .await
    {
        // A stale base_version yields 409 with the current note so the client merges.
        Ok((status, v)) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            (code, Json(v)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn delete_note(State(s): State<NotesStore>, Path(slug): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.delete_note(&slug).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn favorite_note(
    State(s): State<NotesStore>,
    Path(slug): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    let on = b.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
    match s.favorite_note(&slug, on).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_favorites(State(s): State<NotesStore>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_favorites().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn search(State(s): State<NotesStore>, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    let query = q.get("q").map(String::as_str).unwrap_or("");
    match s.search(query, q.get("category").map(String::as_str)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::OwnerIdentity;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    async fn test_router() -> Router {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let store = NotesStore::new(pool, OwnerIdentity::default());
        store.ensure_ready().await.unwrap();
        router(store)
    }

    /// The app router carries its own pod-token auth (it is mounted outside the
    /// Workshop auth layer), so an un-tokened request must be rejected — the
    /// notes data is never served unauthenticated.
    #[tokio::test]
    async fn rejects_request_without_token() {
        let app = test_router().await;
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/whoami")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
