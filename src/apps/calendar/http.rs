//! The Calendar REST API + `/ws`, nested at `/apps/metalcraft-calendar`. Same
//! paths/shapes as the cloud `metalcraft-calendar` core, behind the shared
//! pod-token auth layer ([`crate::apps::require_pod_auth`]). Backend-only;
//! consumed by external clients.

use std::collections::HashMap;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use super::store::{CalendarStore, EventInput};
use super::{tz, CalError};

impl IntoResponse for CalError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, Json(json!({ "error": self.message }))).into_response()
    }
}

pub fn router(store: CalendarStore) -> Router {
    Router::new()
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/now", get(now))
        .route("/api/v1/calendars", get(list_calendars).post(create_calendar))
        .route("/api/v1/calendars/{slug}", axum::routing::patch(update_calendar))
        .route("/api/v1/calendars/{slug}/events", get(list_events).post(create_event))
        .route(
            "/api/v1/calendars/{slug}/events/{id}",
            get(get_event).patch(update_event).delete(delete_event),
        )
        .route("/ws", get(ws))
        .layer(axum::middleware::from_fn(crate::apps::require_pod_auth))
        .with_state(store)
}

async fn ready(s: &CalendarStore) -> Result<(), Response> {
    s.ensure_ready().await.map_err(IntoResponse::into_response)
}

fn q_str<'a>(q: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    q.get(k).map(String::as_str)
}
fn b_str<'a>(b: &'a Value, k: &str) -> Option<&'a str> {
    b.get(k).and_then(|v| v.as_str())
}
fn event_input<'a>(b: &'a Value) -> EventInput<'a> {
    EventInput {
        title: b_str(b, "title").unwrap_or(""),
        starts_at: b_str(b, "starts_at").unwrap_or(""),
        ends_at: b_str(b, "ends_at").unwrap_or(""),
        all_day: b.get("all_day").and_then(|v| v.as_bool()).unwrap_or(false),
        description: b_str(b, "description"),
        location: b_str(b, "location"),
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn whoami(State(s): State<CalendarStore>) -> Response {
    Json(s.whoami()).into_response()
}

async fn now(Query(q): Query<HashMap<String, String>>) -> Response {
    match tz::now_response(q_str(&q, "tz")) {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_calendars(State(s): State<CalendarStore>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_calendars().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn create_calendar(State(s): State<CalendarStore>, Json(b): Json<Value>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.create_calendar(b_str(&b, "name").unwrap_or(""), b_str(&b, "timezone").unwrap_or(""), b_str(&b, "slug")).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

/// PATCH a calendar's settings — including `reminders_enabled` /
/// `reminder_lead_minutes` (the reminder scheduler's config; default on/60).
async fn update_calendar(
    State(s): State<CalendarStore>,
    Path(slug): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s
        .update_calendar(
            &slug,
            b_str(&b, "name"),
            b_str(&b, "timezone"),
            b.get("reminders_enabled").and_then(|v| v.as_bool()),
            b.get("reminder_lead_minutes").and_then(|v| v.as_i64()),
        )
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_events(
    State(s): State<CalendarStore>,
    Path(slug): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.list_events(&slug, q_str(&q, "day"), q_str(&q, "from"), q_str(&q, "to")).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn create_event(
    State(s): State<CalendarStore>,
    Path(slug): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.create_event(&slug, event_input(&b)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn get_event(State(s): State<CalendarStore>, Path((slug, id)): Path<(String, String)>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.get_event(&slug, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn update_event(
    State(s): State<CalendarStore>,
    Path((slug, id)): Path<(String, String)>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.update_event(&slug, &id, event_input(&b)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_event(State(s): State<CalendarStore>, Path((slug, id)): Path<(String, String)>) -> Response {
    if let Err(r) = ready(&s).await {
        return r;
    }
    match s.delete_event(&slug, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// Live-push WebSocket (event.upserted / event.deleted / calendar.upserted).
async fn ws(State(s): State<CalendarStore>, upgrade: WebSocketUpgrade) -> Response {
    let rx = s.events().subscribe();
    upgrade.on_upgrade(move |socket| pump(socket, rx))
}

async fn pump(mut socket: WebSocket, mut rx: tokio::sync::broadcast::Receiver<Value>) {
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(v) => {
                    if socket.send(Message::Text(v.to_string().into())).await.is_err() {
                        break;
                    }
                }
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
    use crate::apps::{AppEventHub, OwnerIdentity};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    #[tokio::test]
    async fn rejects_request_without_token() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let store = CalendarStore::new(pool, OwnerIdentity::default(), AppEventHub::new());
        store.ensure_ready().await.unwrap();
        let app = router(store);
        let resp = app
            .oneshot(HttpRequest::builder().uri("/api/v1/calendars").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
