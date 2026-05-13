use crate::callback_page;
use crate::delivery::{self, QueuedDelivery};
use crate::error::RelayResult;
use crate::github;
use crate::github::types::{GithubInstallationCallbackQuery, GithubManifestCallbackQuery};
use crate::session;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mai_protocol::{RelayEnvelope, RelayStatusResponse};
use serde_json::Value;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/relay/v1/connect", get(connect))
        .route("/relay/v1/status", get(status))
        .route("/github/app-manifest/callback", get(app_manifest_callback))
        .route(
            "/github/app-installation/callback",
            get(app_installation_callback),
        )
        .route("/github/webhook", post(webhook))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub(crate) async fn health() -> &'static str {
    "ok"
}

pub(crate) async fn status(
    State(state): State<Arc<AppState>>,
) -> RelayResult<Json<RelayStatusResponse>> {
    let connection = state.connection.lock().await.clone();
    let queued = state.store.queued_count()?;
    Ok(Json(RelayStatusResponse {
        enabled: true,
        connected: connection.is_some(),
        relay_url: Some(state.public_url.clone()),
        node_id: connection
            .as_ref()
            .map(|connection| connection.node_id.clone()),
        last_heartbeat_at: connection.map(|connection| connection.last_heartbeat_at),
        queued_deliveries: Some(queued),
        message: None,
    }))
}

pub(crate) async fn connect(
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session::handle_socket(state, socket))
}

pub(crate) async fn app_manifest_callback(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GithubManifestCallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        let message = query.error_description.unwrap_or(error);
        return callback_page::callback_page(false, "GitHub App setup was cancelled", &message);
    }
    let code = query.code.unwrap_or_default();
    let state_value = query.state.unwrap_or_default();
    match github::flow::complete_manifest(&state, &code, &state_value).await {
        Ok(_) => callback_page::callback_page(
            true,
            "GitHub App connected",
            "Mai Relay saved the GitHub App.",
        ),
        Err(err) => {
            callback_page::callback_page(false, "GitHub App setup failed", &err.to_string())
        }
    }
}

pub(crate) async fn app_installation_callback(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GithubInstallationCallbackQuery>,
) -> Response {
    match github::flow::complete_app_installation(&state, &query).await {
        Ok(response) => response,
        Err(err) => callback_page::callback_page(
            false,
            "GitHub App installation could not be completed",
            &err.to_string(),
        ),
    }
}

pub(crate) async fn webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let delivery_id = header_string(&headers, "x-github-delivery").unwrap_or_default();
    let event_name = header_string(&headers, "x-github-event").unwrap_or_default();
    if delivery_id.trim().is_empty() || event_name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "missing GitHub webhook headers").into_response();
    }
    let config = match state.store.github_app_config() {
        Ok(Some(config)) => config,
        Ok(None) => {
            return (StatusCode::BAD_REQUEST, "GitHub App is not configured").into_response();
        }
        Err(err) => return err.into_response(),
    };
    let signature = header_string(&headers, "x-hub-signature-256").unwrap_or_default();
    if !delivery::verify_signature(&config.webhook_secret, &body, &signature) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON payload").into_response(),
    };
    let sequence = state.next_delivery_sequence();
    match state
        .store
        .insert_delivery(sequence, &delivery_id, &event_name, &payload)
    {
        Ok(inserted) => {
            if inserted {
                let delivery = QueuedDelivery {
                    sequence,
                    delivery_id: delivery_id.clone(),
                    event_name: event_name.clone(),
                    payload,
                };
                if let Some(connection) = state.connection.lock().await.clone() {
                    let _ = connection
                        .sender
                        .send(RelayEnvelope::Event(delivery.into_event()));
                }
            }
            (StatusCode::ACCEPTED, "accepted").into_response()
        }
        Err(err) => err.into_response(),
    }
}
pub(crate) fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}
