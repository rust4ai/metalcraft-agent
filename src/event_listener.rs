use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json,
};
use tokio::sync::Semaphore;

use crate::approval::ApprovalMode;
use crate::events::GatewayEvent;
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest};

/// Maximum number of concurrent agent tasks processing events.
const MAX_CONCURRENT_TASKS: usize = 4;

/// Configuration for the event listener.
#[derive(Clone)]
pub struct EventListenerConfig {
    pub port: u16,
    pub host: String,
    pub persona_slug: String,
    pub model_name: String,
    pub events: Vec<String>,
    pub platforms: Option<Vec<String>>,
    pub webhook_secret: Option<String>,
    pub approval_mode: ApprovalMode,
    pub cwd: String,
}

struct ListenerState {
    config: EventListenerConfig,
    semaphore: Arc<Semaphore>,
}

/// Start the event listener HTTP server and register with the gateway.
///
/// This function blocks until the server shuts down — call via `tokio::spawn`.
pub async fn start(config: EventListenerConfig, _context: AgentRuntimeContext) {
    let port = config.port;

    let state = Arc::new(ListenerState {
        config: config.clone(),
        semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_TASKS)),
    });

    // Register with gateway
    let subscriber_id = register_subscriber(&config).await;

    let app = Router::new()
        .route("/webhook/events", post(handle_event))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("Failed to bind event listener");

    log::info!("Event listener serving on 0.0.0.0:{port}");

    // Set up graceful shutdown to unregister subscriber
    let shutdown_signal = async {
        tokio::signal::ctrl_c().await.ok();
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .expect("Event listener server error");

    // Unregister on shutdown
    if let Some(id) = subscriber_id {
        unregister_subscriber(&id).await;
    }
}

async fn handle_event(
    State(state): State<Arc<ListenerState>>,
    headers: HeaderMap,
    Json(event): Json<GatewayEvent>,
) -> StatusCode {
    // Verify webhook secret if configured
    if let Some(ref expected) = state.config.webhook_secret {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();

        if provided != expected {
            log::warn!("Rejected event with invalid webhook secret");
            return StatusCode::UNAUTHORIZED;
        }
    }

    // Skip bot messages to prevent self-reply loops
    if event
        .author
        .as_ref()
        .map(|a| a.is_bot)
        .unwrap_or(false)
    {
        log::debug!("Skipping bot event from {}", event.author.as_ref().unwrap().username);
        return StatusCode::OK;
    }

    let prompt = event.to_agent_prompt();
    let persona_slug = state.config.persona_slug.clone();
    let model_name = state.config.model_name.clone();
    let cwd = state.config.cwd.clone();
    let approval_mode = state.config.approval_mode.clone();

    // Rebuild context for each task (AgentRuntimeContext is cheap)
    let context = match AgentRuntimeContext::from_environment() {
        Ok(ctx) => ctx,
        Err(e) => {
            log::error!("Failed to create runtime context: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    // Acquire semaphore permit to limit concurrency
    let semaphore = state.semaphore.clone();
    let permit = match semaphore.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            log::warn!(
                "Dropping event {} — max concurrent tasks ({MAX_CONCURRENT_TASKS}) reached",
                event.id
            );
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };

    // Fire-and-forget: spawn agent task (permit released on drop)
    tokio::spawn(async move {
        let _permit = permit;

        log::info!(
            "Processing {} event from {} in channel {}",
            event.event_type,
            event.author.as_ref().map(|a| a.username.as_str()).unwrap_or("unknown"),
            event.channel_id.as_deref().unwrap_or("unknown"),
        );

        let outcome = runtime::run_one_shot_task(
            &context,
            RunOneShotRequest {
                persona_slug: &persona_slug,
                cwd: &cwd,
                model_name: &model_name,
                task: &prompt,
                approval_mode,
                diagnostics: None,
            },
        )
        .await;

        match outcome {
            Ok(metalcraft::RunOutcome::Completed(state)) => {
                log::info!(
                    "Event {} handled: {}",
                    event.id,
                    state.final_answer().unwrap_or("(no answer)")
                );
            }
            Ok(metalcraft::RunOutcome::Interrupted { reason, .. }) => {
                log::warn!("Event {} interrupted: {reason}", event.id);
            }
            Err(e) => {
                log::error!("Event {} failed: {e}", event.id);
            }
        }
    });

    StatusCode::OK
}

/// Register this listener as a subscriber with the gateway.
async fn register_subscriber(config: &EventListenerConfig) -> Option<String> {
    let gateway_url = match std::env::var("AGENT_GATEWAY_URL") {
        Ok(url) => url,
        Err(_) => {
            log::warn!("AGENT_GATEWAY_URL not set, skipping subscriber registration");
            return None;
        }
    };

    let api_key = std::env::var("AGENT_GATEWAY_API_KEY").unwrap_or_default();
    let callback_url = format!("http://{}:{}/webhook/events", config.host, config.port);

    let mut body = serde_json::json!({
        "url": callback_url,
        "events": config.events,
    });

    if let Some(ref platforms) = config.platforms {
        body["platforms"] = serde_json::json!(platforms);
    }

    if let Some(ref secret) = config.webhook_secret {
        body["secret"] = serde_json::json!(secret);
    }

    let client = reqwest::Client::new();
    match client
        .post(format!("{gateway_url}/api/v1/subscribers"))
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let json: serde_json::Value = resp.json().await.unwrap_or_default();
            let id = json["id"].as_str().map(String::from);
            log::info!(
                "Registered as gateway subscriber (id: {}, url: {callback_url})",
                id.as_deref().unwrap_or("unknown")
            );
            id
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            log::error!("Failed to register subscriber: HTTP {status} — {body}");
            None
        }
        Err(e) => {
            log::error!("Failed to register subscriber: {e}");
            None
        }
    }
}

/// Unregister this listener from the gateway.
async fn unregister_subscriber(id: &str) {
    let gateway_url = match std::env::var("AGENT_GATEWAY_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    let api_key = std::env::var("AGENT_GATEWAY_API_KEY").unwrap_or_default();
    let client = reqwest::Client::new();

    match client
        .delete(format!("{gateway_url}/api/v1/subscribers/{id}"))
        .bearer_auth(&api_key)
        .send()
        .await
    {
        Ok(_) => log::info!("Unregistered subscriber {id}"),
        Err(e) => log::warn!("Failed to unregister subscriber {id}: {e}"),
    }
}
