use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use mai_protocol::*;

use super::state::{ApiError, AppState};

pub(crate) async fn get_runtime_defaults(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<RuntimeDefaultsResponse>, ApiError> {
    Ok(Json(state.runtime.runtime_defaults()))
}

pub(crate) async fn list_skills(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.list_skills().await?))
}

pub(crate) async fn list_agent_profiles(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<AgentProfilesResponse>, ApiError> {
    Ok(Json(state.runtime.list_agent_profiles().await?))
}

pub(crate) async fn save_skills_config(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SkillsConfigRequest>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.update_skills_config(request).await?))
}

pub(crate) async fn get_agent_config(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<AgentConfigResponse>, ApiError> {
    Ok(Json(state.runtime.agent_config().await?))
}

pub(crate) async fn save_agent_config(
    State(state): State<Arc<AppState>>,
    Json(request): Json<AgentConfigRequest>,
) -> std::result::Result<Json<AgentConfigResponse>, ApiError> {
    Ok(Json(state.runtime.update_agent_config(request).await?))
}

pub(crate) async fn get_provider_presets(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<ProviderPresetsResponse>, ApiError> {
    Ok(Json(state.store.provider_presets_response()))
}
