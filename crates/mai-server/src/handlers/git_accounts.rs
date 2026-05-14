use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use mai_protocol::*;

use super::state::{ApiError, AppState};

pub(crate) async fn list_git_accounts(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<GitAccountsResponse>, ApiError> {
    Ok(Json(state.runtime.list_git_accounts().await?))
}

pub(crate) async fn save_git_account(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GitAccountRequest>,
) -> std::result::Result<Json<GitAccountResponse>, ApiError> {
    Ok(Json(state.runtime.save_git_account(request).await?))
}

pub(crate) async fn save_git_account_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut request): Json<GitAccountRequest>,
) -> std::result::Result<Json<GitAccountResponse>, ApiError> {
    request.id = Some(id);
    Ok(Json(state.runtime.save_git_account(request).await?))
}

pub(crate) async fn verify_git_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<GitAccountResponse>, ApiError> {
    Ok(Json(GitAccountResponse {
        account: state.runtime.verify_git_account(&id).await?,
    }))
}

pub(crate) async fn delete_git_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<GitAccountsResponse>, ApiError> {
    Ok(Json(state.runtime.delete_git_account(&id).await?))
}

pub(crate) async fn set_default_git_account(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GitAccountDefaultRequest>,
) -> std::result::Result<Json<GitAccountsResponse>, ApiError> {
    Ok(Json(
        state
            .runtime
            .set_default_git_account(&request.account_id)
            .await?,
    ))
}

pub(crate) async fn list_git_account_repositories(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<GithubRepositoriesResponse>, ApiError> {
    Ok(Json(
        state.runtime.list_git_account_repositories(&id).await?,
    ))
}

pub(crate) async fn list_git_account_repository_packages(
    State(state): State<Arc<AppState>>,
    Path((id, owner, repo)): Path<(String, String, String)>,
) -> std::result::Result<Json<RepositoryPackagesResponse>, ApiError> {
    Ok(Json(
        state
            .runtime
            .list_git_account_repository_packages(&id, &owner, &repo)
            .await?,
    ))
}
