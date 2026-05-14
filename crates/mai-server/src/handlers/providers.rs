use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};

use mai_protocol::*;

use super::state::{ApiError, AppState};
use crate::services::providers::ProviderService;

pub(crate) async fn get_providers(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    let service = ProviderService::new(Arc::clone(&state.store));
    Ok(Json(service.providers_response().await?))
}

pub(crate) async fn save_providers(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProvidersConfigRequest>,
) -> std::result::Result<Json<ProvidersResponse>, ApiError> {
    let service = ProviderService::new(Arc::clone(&state.store));
    Ok(Json(service.save_providers(request).await?))
}

pub(crate) async fn get_mcp_servers(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<McpServersConfigRequest>, ApiError> {
    let service = ProviderService::new(Arc::clone(&state.store));
    Ok(Json(service.mcp_servers().await?))
}

pub(crate) async fn save_mcp_servers(
    State(state): State<Arc<AppState>>,
    Json(request): Json<McpServersConfigRequest>,
) -> std::result::Result<Json<McpServersConfigRequest>, ApiError> {
    let service = ProviderService::new(Arc::clone(&state.store));
    Ok(Json(service.save_mcp_servers(&request.servers).await?))
}

pub(crate) async fn test_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ProviderTestRequest>,
) -> std::result::Result<Response, ApiError> {
    let service = ProviderService::new(Arc::clone(&state.store));
    let result = service.test_provider(&id, request).await;
    Ok((result.status, Json(result.response)).into_response())
}
