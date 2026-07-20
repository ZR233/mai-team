use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
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

pub(crate) async fn get_provider_catalog(
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let snapshot = mai_runtime::provider_catalog_snapshot()?;
    let etag = format!("\"{}\"", snapshot.revision);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == etag)
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(
            header::ETAG,
            HeaderValue::from_str(&etag).expect("catalog revision is a valid header value"),
        );
        return Ok(response);
    }
    let mut response = Json(snapshot).into_response();
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("catalog revision is a valid header value"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use pretty_assertions::assert_eq;

    use super::get_provider_catalog;

    #[tokio::test]
    async fn provider_catalog_response_is_canonical_and_etag_cacheable() {
        let response = get_provider_catalog(HeaderMap::new()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers()[header::ETAG].clone();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let snapshot: mai_protocol::ProviderCatalogSnapshot =
            serde_json::from_slice(&body).unwrap();

        assert_eq!(
            snapshot.schema_version,
            mai_protocol::PROVIDER_CATALOG_SCHEMA_VERSION
        );
        for preset in &snapshot.presets {
            serde_json::from_value::<mai_protocol::ProviderWireProtocol>(serde_json::json!(
                preset.transport.protocol
            ))
            .unwrap_or_else(|error| {
                panic!(
                    "catalog preset `{}` uses an unsupported wire protocol `{}`: {error}",
                    preset.id, preset.transport.protocol
                )
            });
        }

        assert_eq!(etag.to_str().unwrap(), format!("\"{}\"", snapshot.revision));
        assert!(
            snapshot
                .presets
                .iter()
                .any(|preset| preset.id == "mimo-api")
        );
        assert!(
            snapshot
                .presets
                .iter()
                .any(|preset| preset.id == "mimo-token-plan")
        );
        assert!(
            snapshot.model_catalogs["openai"]
                .models
                .iter()
                .any(|model| model.id.starts_with("gpt-5.6"))
        );

        let mut conditional_headers = HeaderMap::new();
        conditional_headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_bytes(etag.as_bytes()).unwrap(),
        );
        let cached = get_provider_catalog(conditional_headers).await.unwrap();
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(cached.headers()[header::ETAG], etag);
    }
}
