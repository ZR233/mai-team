use crate::delivery;
use crate::error::{RelayErrorKind, RelayResult, error_code};
use crate::github;
use crate::state::AppState;
use crate::update;
use mai_protocol::{
    GithubAppInstallationStartRequest, GithubAppManifestStartRequest, RelayAck, RelayError,
    RelayGithubInstallationTokenRequest, RelayGithubRepositoriesRequest,
    RelayGithubRepositoryPackagesRequest, RelayRequest, RelayResponse, RelaySettingsRequest,
    RelayUpdateApplyRequest, RelayUpdateCheckRequest, RelayUpdateRestartRequest,
    RelayUpdateRollbackRequest,
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
        "github.app.save" => {
            match parse_params::<mai_protocol::GithubAppSettingsRequest>(request.params).await {
                Ok(request) => github::app::save_github_app_settings(state, request)
                    .await
                    .and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "relay.config.get" => state
            .store
            .relay_config(&state.public_url)
            .and_then(to_value),
        "relay.config.save" => match parse_params::<RelaySettingsRequest>(request.params).await {
            Ok(request) => match request.url {
                Some(url) => state.store.save_relay_config(&url).and_then(to_value),
                None => Err(RelayErrorKind::InvalidInput(
                    "relay public URL is required".to_string(),
                )),
            },
            Err(err) => Err(err),
        },
        "relay.update.check" => match parse_params::<RelayUpdateCheckRequest>(request.params).await
        {
            Ok(request) => update::check(&state.http, request).await.and_then(to_value),
            Err(err) => Err(err),
        },
        "relay.update.apply" => {
            match parse_params::<RelayUpdateApplyRequest>(request.params).await {
                Ok(_request) => update::apply(&state.http).await.and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "relay.update.rollback" => {
            match parse_params::<RelayUpdateRollbackRequest>(request.params).await {
                Ok(_request) => update::rollback().and_then(to_value),
                Err(err) => Err(err),
            }
        }
        "relay.update.restart" => {
            match parse_params::<RelayUpdateRestartRequest>(request.params).await {
                Ok(_request) => to_value(update::restart()),
                Err(err) => Err(err),
            }
        }
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

pub(crate) async fn parse_params<T>(params: Value) -> RelayResult<T>
where
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(params)?)
}

pub(crate) fn to_value<T>(value: T) -> RelayResult<Value>
where
    T: Serialize,
{
    Ok(serde_json::to_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RelayConfig;
    use crate::store::RelayStore;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn update_rollback_returns_relay_error_when_backup_is_missing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let state = Arc::new(
            AppState::new(
                RelayConfig {
                    bind_addr: "127.0.0.1:0".parse().expect("bind addr"),
                    public_url: "http://127.0.0.1:8090".to_string(),
                    token: "token".to_string(),
                    db_path: temp_dir.path().join("relay.sqlite3"),
                    github_api_base_url: "https://api.github.com".to_string(),
                    github_web_base_url: "https://github.com".to_string(),
                },
                Arc::new(RelayStore::open(temp_dir.path().join("relay.sqlite3")).expect("store")),
                reqwest::Client::new(),
            )
            .expect("state"),
        );

        let response = handle_client_request(
            &state,
            RelayRequest {
                id: "request-1".to_string(),
                method: "relay.update.rollback".to_string(),
                params: json!({}),
            },
        )
        .await;

        assert_eq!(response.id, "request-1");
        assert!(response.result.is_none());
        let error = response.error.expect("relay error");
        assert_eq!(error.code, "invalid_input");
        assert!(error.message.contains("backup"));
    }
}
