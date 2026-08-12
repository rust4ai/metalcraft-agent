//! The Drive REST API + `/ws`, nested at `/apps/metalcraft-drive`, behind the
//! shared pod-token auth. Backend-only; external clients upload via multipart
//! and download the raw bytes.

use std::collections::HashMap;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use super::store::DriveStore;
use super::DrvError;

impl IntoResponse for DrvError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, Json(json!({ "error": self.message }))).into_response()
    }
}

pub fn router(store: DriveStore) -> Router {
    Router::new()
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/folders", post(create_folder))
        .route("/api/v1/folders/{folder}/contents", get(list_folder))
        .route("/api/v1/files", post(upload))
        .route("/api/v1/files/{id}", get(get_file).patch(update_file).delete(delete_file))
        .route("/api/v1/files/{id}/download", get(download))
        .route("/api/v1/files/{id}/share", post(share).delete(unshare))
        .route("/api/v1/starred", get(list_starred))
        .route("/api/v1/trash", get(list_trash))
        .route("/ws", get(ws))
        .layer(axum::middleware::from_fn(crate::apps::require_pod_auth))
        // Public share passthrough (C4) — after the auth layer (unauthenticated;
        // the unguessable token authorizes). The coordinator fetches this and
        // relays it under a neutral domain.
        .route("/p/{token}", get(public_page))
        .with_state(store)
}

/// Share a file: mark it public, register the token with the coordinator, and
/// return the neutral share URL.
async fn share(State(s): State<DriveStore>, Path(id): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.share(&id).await {
        Ok(token) => {
            crate::apps::coordinator::register_share(&token, "file", &id).await;
            let url = crate::apps::coordinator::share_url("metalcraft-drive", &token);
            Json(json!({ "url": url, "token": token })).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn unshare(State(s): State<DriveStore>, Path(id): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.unshare(&id).await {
        Ok(token) => {
            if let Some(t) = token {
                crate::apps::coordinator::unregister_share(&t).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Public: serve a shared file's bytes by token (content-type + download name).
async fn public_page(State(s): State<DriveStore>, Path(token): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.public_file(&token).await {
        Ok((content_type, name, bytes)) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CONTENT_DISPOSITION, format!("inline; filename=\"{}\"", name.replace('"', ""))),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn ready(s: &DriveStore) -> Result<(), Response> {
    s.ensure_ready().await.map_err(IntoResponse::into_response)
}
fn b_str<'a>(b: &'a Value, k: &str) -> Option<&'a str> {
    b.get(k).and_then(|v| v.as_str())
}

async fn whoami(State(s): State<DriveStore>) -> Response {
    Json(s.whoami()).into_response()
}

async fn create_folder(State(s): State<DriveStore>, Json(b): Json<Value>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.create_folder(b_str(&b, "name").unwrap_or(""), b_str(&b, "parent_id")).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_folder(State(s): State<DriveStore>, Path(folder): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_folder(Some(&folder)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Multipart upload: `file` part (bytes), optional `name` / `folder_id` /
/// `content_type` text parts.
async fn upload(State(s): State<DriveStore>, mut mp: Multipart) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    let mut bytes: Option<Vec<u8>> = None;
    let mut name: Option<String> = None;
    let mut folder_id: Option<String> = None;
    let mut content_type: Option<String> = None;
    while let Ok(Some(field)) = mp.next_field().await {
        match field.name() {
            Some("file") => {
                if name.is_none() {
                    name = field.file_name().map(str::to_string);
                }
                let ct = field.content_type().map(str::to_string);
                if content_type.is_none() {
                    content_type = ct;
                }
                match field.bytes().await {
                    Ok(b) => bytes = Some(b.to_vec()),
                    Err(_) => {
                        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "failed to read file" }))).into_response()
                    }
                }
            }
            Some("name") => name = field.text().await.ok(),
            Some("folder_id") => folder_id = field.text().await.ok(),
            Some("content_type") => content_type = field.text().await.ok(),
            _ => {}
        }
    }
    let Some(bytes) = bytes else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "no file in upload" }))).into_response();
    };
    let name = name.unwrap_or_else(|| "file".to_string());
    match s.upload(&name, bytes, content_type.as_deref(), folder_id.as_deref()).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn get_file(State(s): State<DriveStore>, Path(id): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.get_file(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn download(State(s): State<DriveStore>, Path(id): Path<String>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.download(&id).await {
        Ok((view, bytes)) => (
            [
                (header::CONTENT_TYPE, view.content_type),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", view.name)),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn update_file(State(s): State<DriveStore>, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    let folder_id = b.get("folder_id").map(|v| {
        let x = v.as_str().unwrap_or("").trim();
        if x.is_empty() { None } else { Some(x) }
    });
    match s
        .update_file(
            &id,
            b_str(&b, "name"),
            folder_id,
            b.get("starred").and_then(|v| v.as_bool()),
            b.get("trashed").and_then(|v| v.as_bool()),
        )
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_file(
    State(s): State<DriveStore>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    let permanent = q.get("permanent").map(|v| v == "true" || v == "1").unwrap_or(false);
    match s.delete_file(&id, permanent).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_starred(State(s): State<DriveStore>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_starred().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_trash(State(s): State<DriveStore>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_trash().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn ws(State(s): State<DriveStore>, upgrade: WebSocketUpgrade) -> Response {
    let rx = s.events().subscribe();
    upgrade.on_upgrade(move |socket| pump(socket, rx))
}

async fn pump(mut socket: WebSocket, mut rx: tokio::sync::broadcast::Receiver<Value>) {
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(v) => { if socket.send(Message::Text(v.to_string().into())).await.is_err() { break; } }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::{AppEventHub, BlobStore, LocalBlobStore, OwnerIdentity};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn rejects_request_without_token() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let blobs: Arc<dyn BlobStore> = Arc::new(LocalBlobStore::new(dir.path().to_path_buf()));
        let store = DriveStore::new(pool, OwnerIdentity::default(), AppEventHub::new(), blobs);
        store.ensure_ready().await.unwrap();
        let resp = router(store)
            .oneshot(HttpRequest::builder().uri("/api/v1/starred").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
