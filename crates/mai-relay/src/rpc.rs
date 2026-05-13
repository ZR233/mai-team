use crate::delivery;
use crate::error::{RelayErrorKind, RelayResult, error_code};
use crate::github;
use crate::state::AppState;
use mai_protocol::{
    GithubAppInstallationStartRequest, GithubAppManifestStartRequest, RelayAck, RelayError,
    RelayGithubInstallationTokenRequest, RelayGithubRepositoriesRequest,
    RelayGithubRepositoryPackagesRequest, RelayRequest, RelayResponse,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) async fn handle_client_request(
    state: &Arc<AppState>,
    request: RelayRequest,
) -> RelayResponse {
    let id = request.id.clone();
    let result = match request.method.as_str() {
        "github_app_manifest.start" => {
            match parse_params::<GithubAppManifestStartRequest>(request.params).await {
                Ok(request) => github::flow::start_manifest(state, request).await,
                Err(err) => Err(err),
            }
        }
        "github.app.get" => github::app::github_app_settings(state).and_then(to_value),
        "github.app_installation.start" => {
            match parse_params::<GithubAppInstallationStartRequest>(request.params).await {
                Ok(request) => github::flow::start_app_installation(state, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "github.installations.list" => github::api::list_installations(state)
            .await
            .and_then(to_value),
        "github.repositories.list" => {
            match parse_params::<RelayGithubRepositoriesRequest>(request.params).await {
                Ok(request) => github::api::list_repositories(state, request.installation_id).await,
                Err(err) => Err(err),
            }
        }
        "github.repository.get" => {
            match parse_params::<mai_protocol::RelayGithubRepositoryGetRequest>(request.params)
                .await
            {
                Ok(request) => github::api::get_repository(state, request).await,
                Err(err) => Err(err),
            }
        }
        "github.installation_token.create" => {
            match parse_params::<RelayGithubInstallationTokenRequest>(request.params).await {
                Ok(request) => github::api::create_installation_token(state, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "github.repository_packages.list" => {
            match parse_params::<RelayGithubRepositoryPackagesRequest>(request.params).await {
                Ok(request) => github::packages::list_repository_packages(state, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "github.webhook_delivery.ack" => match parse_params::<RelayAck>(request.params).await {
            Ok(ack) => match delivery::handle_ack(state, ack).await {
                Ok(()) => Ok(json!({ "ok": true })),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        other => Err(RelayErrorKind::InvalidInput(format!(
            "unknown relay method `{other}`"
        ))),
    };
    relay_response(id, result)
}

pub(crate) fn relay_response(id: String, result: RelayResult<Value>) -> RelayResponse {
    match result {
        Ok(result) => RelayResponse {
            id,
            result: Some(result),
            error: None,
        },
        Err(error) => RelayResponse {
            id,
            result: None,
            error: Some(RelayError {
                code: error_code(&error).to_string(),
                message: error.to_string(),
            }),
        },
    }
}

pub(crate) fn parse_params<T>(params: Value) -> impl std::future::Future<Output = RelayResult<T>>
where
    T: DeserializeOwned,
{
    async move { Ok(serde_json::from_value(params)?) }
}

pub(crate) fn to_value<T>(value: T) -> RelayResult<Value>
where
    T: Serialize,
{
    Ok(serde_json::to_value(value)?)
}
