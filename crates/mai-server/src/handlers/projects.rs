use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use mai_protocol::{
    AgentId, CreateProjectRequest, CreateProjectResponse, ProjectId, ProjectReviewQueueResponse,
    ProjectReviewRunDetail, ProjectReviewRunsResponse, SendMessageRequest, SendMessageResponse,
    SessionId, SkillsListResponse, UpdateProjectRequest, UpdateProjectResponse,
};
use mai_runtime::ProjectReviewQueueRequest;

use super::state::{ApiError, AppState};

const DEFAULT_REVIEW_RUNS_PAGE_SIZE: usize = 50;

#[derive(Debug, Deserialize)]
pub(crate) struct ProjectDetailQuery {
    agent_id: Option<AgentId>,
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProjectReviewRunsQuery {
    offset: Option<usize>,
    limit: Option<usize>,
}

pub(crate) async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<Vec<mai_protocol::ProjectSummary>>, ApiError> {
    Ok(Json(state.runtime.list_projects().await))
}

pub(crate) async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateProjectRequest>,
) -> std::result::Result<Json<CreateProjectResponse>, ApiError> {
    let project = state.runtime.create_project(request).await?;
    Ok(Json(CreateProjectResponse { project }))
}

pub(crate) async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Query(query): Query<ProjectDetailQuery>,
) -> std::result::Result<Json<mai_protocol::ProjectDetail>, ApiError> {
    Ok(Json(
        state
            .runtime
            .get_project(id, query.agent_id, query.session_id)
            .await?,
    ))
}

pub(crate) async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Json(request): Json<UpdateProjectRequest>,
) -> std::result::Result<Json<UpdateProjectResponse>, ApiError> {
    let project = state.runtime.update_project(id, request).await?;
    Ok(Json(UpdateProjectResponse { project }))
}

pub(crate) async fn send_project_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Json(request): Json<SendMessageRequest>,
) -> std::result::Result<Json<SendMessageResponse>, ApiError> {
    let turn_id = state.runtime.send_project_message(id, request).await?;
    Ok(Json(SendMessageResponse { turn_id }))
}

pub(crate) async fn list_project_review_runs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
    Query(query): Query<ProjectReviewRunsQuery>,
) -> std::result::Result<Json<ProjectReviewRunsResponse>, ApiError> {
    Ok(Json(
        state
            .runtime
            .list_project_review_runs(
                id,
                query.offset.unwrap_or(0),
                query.limit.unwrap_or(DEFAULT_REVIEW_RUNS_PAGE_SIZE),
            )
            .await?,
    ))
}

pub(crate) async fn get_project_review_run(
    State(state): State<Arc<AppState>>,
    Path((id, run_id)): Path<(ProjectId, String)>,
) -> std::result::Result<Json<ProjectReviewRunDetail>, ApiError> {
    let run_id = run_id.parse().map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid review run id: {err}"),
    })?;
    Ok(Json(
        state.runtime.get_project_review_run(id, run_id).await?,
    ))
}

pub(crate) async fn request_project_pull_request_review(
    State(state): State<Arc<AppState>>,
    Path((id, pr)): Path<(ProjectId, u64)>,
) -> std::result::Result<Json<ProjectReviewQueueResponse>, ApiError> {
    let summary = state
        .runtime
        .enqueue_project_review(ProjectReviewQueueRequest {
            project_id: id,
            pr,
            head_sha: None,
            delivery_id: None,
            reason: "manual_rereview".to_string(),
        })
        .await?;
    Ok(Json(ProjectReviewQueueResponse {
        queued: summary.queued,
        deduped: summary.deduped,
        ignored: summary.ignored,
    }))
}

pub(crate) async fn list_project_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.list_project_skills(id).await?))
}

pub(crate) async fn detect_project_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<Json<SkillsListResponse>, ApiError> {
    Ok(Json(state.runtime.detect_project_skills(id).await?))
}

pub(crate) async fn cancel_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.cancel_project(id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<ProjectId>,
) -> std::result::Result<StatusCode, ApiError> {
    state.runtime.delete_project(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use chrono::Utc;
    use mai_docker::DockerClient;
    use mai_protocol::{AgentRole, AgentStatus, ProjectCloneStatus, ProjectStatus};
    use mai_runtime::{AgentRuntime, ModelClient, RuntimeConfig};
    use mai_store::ConfigStore;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[tokio::test]
    async fn request_project_pull_request_review_queues_manual_rereview() {
        let project_id = ProjectId::new_v4();
        let maintainer_id = AgentId::new_v4();
        let state = test_app_state(project_id, maintainer_id).await;

        let response =
            request_project_pull_request_review(State(Arc::clone(&state)), Path((project_id, 17)))
                .await
                .expect("queue review")
                .0;

        assert_eq!(
            ProjectReviewQueueResponse {
                queued: vec![17],
                deduped: vec![],
                ignored: vec![],
            },
            response
        );
    }

    async fn test_app_state(project_id: ProjectId, maintainer_id: AgentId) -> Arc<AppState> {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            ConfigStore::open_with_config_and_artifact_index_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
                dir.path().join("artifacts/index"),
            )
            .await
            .expect("store"),
        );
        save_test_project(&store, project_id, maintainer_id, true).await;
        let runtime = AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(&dir)),
            ModelClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
                projects_root: dir.path().join("projects"),
                cache_root: dir.path().join("cache"),
                artifact_files_root: dir.path().join("artifacts/files"),
                sidecar_image: "sidecar:latest".to_string(),
                github_api_base_url: None,
                git_binary: None,
                system_skills_root: None,
                system_agents_root: None,
            },
        )
        .await
        .expect("runtime");
        let relay = crate::services::relay_manager::RelayManager::new(Arc::clone(&store));
        Arc::new(AppState {
            runtime,
            store,
            relay,
        })
    }

    async fn save_test_project(
        store: &ConfigStore,
        project_id: ProjectId,
        maintainer_id: AgentId,
        auto_review_enabled: bool,
    ) {
        let now = Utc::now();
        store
            .save_agent(
                &mai_protocol::AgentSummary {
                    id: maintainer_id,
                    parent_id: None,
                    name: "Maintainer".to_string(),
                    status: AgentStatus::Idle,
                    task_id: None,
                    project_id: Some(project_id),
                    role: Some(AgentRole::Planner),
                    model: "mock-model".to_string(),
                    provider_id: "mock".to_string(),
                    provider_name: "Mock".to_string(),
                    reasoning_effort: None,
                    docker_image: "unused".to_string(),
                    container_id: None,
                    current_turn: None,
                    created_at: now,
                    updated_at: now,
                    last_error: None,
                    token_usage: Default::default(),
                },
                None,
            )
            .await
            .expect("save maintainer");
        store
            .save_project(&mai_protocol::ProjectSummary {
                id: project_id,
                name: "owner/repo".to_string(),
                status: ProjectStatus::Creating,
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                repository_full_name: "owner/repo".to_string(),
                git_account_id: Some("account-1".to_string()),
                repository_id: 42,
                installation_id: 0,
                installation_account: "owner".to_string(),
                branch: "main".to_string(),
                docker_image: "unused".to_string(),
                clone_status: ProjectCloneStatus::Pending,
                maintainer_agent_id: maintainer_id,
                created_at: now,
                updated_at: now,
                last_error: None,
                auto_review_enabled,
                reviewer_extra_prompt: None,
                review_status: Default::default(),
                current_reviewer_agent_id: None,
                last_review_started_at: None,
                last_review_finished_at: None,
                next_review_at: None,
                last_review_outcome: None,
                review_last_error: None,
            })
            .await
            .expect("save project");
    }

    fn fake_docker_path(dir: &TempDir) -> String {
        let path = dir.path().join("fake-docker.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
case "$1" in
  ps)
    exit 0
    ;;
  version)
    echo "test-version"
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
"#,
        )
        .expect("write fake docker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path)
                .expect("fake docker metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod fake docker");
        }
        path.to_string_lossy().into_owned()
    }
}
