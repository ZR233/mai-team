use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::github::{VerifiedGithubRepository, github_clone_url};
use crate::state::{ProjectRecord, RuntimeState};
use crate::{Result, RuntimeError};
use mai_protocol::{
    AgentDetail, AgentId, AgentModelPreference, AgentRole, AgentSummary, CreateProjectRequest,
    GitAccountSummary, GithubInstallationsResponse, ProjectCloneStatus, ProjectDetail, ProjectId,
    ProjectReviewStatus, ProjectStatus, ProjectSummary, SendMessageRequest, ServiceEventKind,
    SessionId, TurnId, UpdateProjectRequest, now,
};
use uuid::Uuid;

/// Supplies the agent details and review run summaries needed to assemble
/// project read models without exposing the full runtime to project service
/// queries.
pub(crate) trait ProjectReadOps: Send + Sync {
    fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl Future<Output = Result<AgentDetail>> + Send;
    fn recent_review_runs(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<Vec<mai_protocol::ProjectReviewRunSummary>>> + Send;
}

/// Supplies side effects required by project update/delete lifecycle operations.
pub(crate) trait ProjectLifecycleOps: Send + Sync {
    fn save_project(&self, project: &ProjectSummary) -> impl Future<Output = Result<()>> + Send;
    fn delete_project_from_store(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;
    fn publish_project_event(&self, event: ServiceEventKind) -> impl Future<Output = ()> + Send;
    fn start_project_review_loop_if_ready(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;
    fn stop_project_review_loop(&self, project_id: ProjectId) -> impl Future<Output = ()> + Send;
    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;
    fn cancel_project_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;
    fn shutdown_project_mcp_manager(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = ()> + Send;
    fn delete_project_sidecar(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;
    fn delete_project_review_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<()>> + Send;
    fn remove_project_from_memory(&self, project_id: ProjectId) -> impl Future<Output = ()> + Send;
    fn remove_project_skill_lock(&self, project_id: ProjectId) -> impl Future<Output = ()> + Send;
    fn project_skill_cache_dir(&self, project_id: ProjectId) -> PathBuf;
    fn send_project_agent_message(
        &self,
        agent_id: AgentId,
        request: SendMessageRequest,
    ) -> impl Future<Output = Result<TurnId>> + Send;
}

pub(crate) struct ProjectMaintainerAgentRequest {
    pub(crate) project_id: ProjectId,
    pub(crate) name: String,
    pub(crate) model: AgentModelPreference,
    pub(crate) docker_image: Option<String>,
    pub(crate) system_prompt: String,
}

/// Supplies GitHub, agent, persistence, and async workspace setup side effects
/// required to create a project.
pub(crate) trait ProjectCreateOps: Send + Sync {
    fn list_github_installations(
        &self,
    ) -> impl Future<Output = Result<GithubInstallationsResponse>> + Send;
    fn upsert_github_app_relay_account(
        &self,
        installation_id: u64,
        account_login: &str,
    ) -> impl Future<Output = Result<String>> + Send;
    fn verified_repository(
        &self,
        account_id: &str,
        repository_full_name: &str,
    ) -> impl Future<Output = Result<VerifiedGithubRepository>> + Send;
    fn git_account_summary(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<GitAccountSummary>> + Send;
    fn planner_model(&self) -> impl Future<Output = Result<AgentModelPreference>> + Send;
    fn create_project_maintainer_agent(
        &self,
        request: ProjectMaintainerAgentRequest,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;
    fn save_project(&self, project: &ProjectSummary) -> impl Future<Output = Result<()>> + Send;
    fn insert_project(&self, project: ProjectSummary) -> impl Future<Output = ()> + Send;
    fn publish_project_event(&self, event: ServiceEventKind) -> impl Future<Output = ()> + Send;
    fn start_project_workspace(
        &self,
        project_id: ProjectId,
        maintainer_agent_id: AgentId,
    ) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn project(
    state: &RuntimeState,
    project_id: ProjectId,
) -> Result<Arc<ProjectRecord>> {
    state
        .projects
        .read()
        .await
        .get(&project_id)
        .cloned()
        .ok_or(RuntimeError::ProjectNotFound(project_id))
}

pub(crate) async fn list_projects(state: &RuntimeState) -> Vec<ProjectSummary> {
    let project_records = {
        let projects = state.projects.read().await;
        projects.values().cloned().collect::<Vec<_>>()
    };
    let mut summaries = Vec::with_capacity(project_records.len());
    for project in project_records {
        summaries.push(project.summary.read().await.clone());
    }
    summaries.sort_by_key(|summary| summary.created_at);
    summaries
}

pub(crate) async fn get_project(
    state: &RuntimeState,
    ops: &impl ProjectReadOps,
    project_id: ProjectId,
    selected_agent_id: Option<AgentId>,
    session_id: Option<SessionId>,
) -> Result<ProjectDetail> {
    let project = project(state, project_id).await?;
    let summary = project.summary.read().await.clone();
    let agents = project_agents(state, project_id).await;
    let requested_agent_id =
        selected_agent_id.filter(|id| agents.iter().any(|agent| agent.id == *id));
    let selected_session_id = if selected_agent_id.is_some() && requested_agent_id.is_none() {
        None
    } else {
        session_id
    };
    let selected_agent_id = requested_agent_id.unwrap_or(summary.maintainer_agent_id);
    let maintainer_session_id = (selected_agent_id == summary.maintainer_agent_id)
        .then_some(selected_session_id)
        .flatten();
    let maintainer_agent = ops
        .get_agent(summary.maintainer_agent_id, maintainer_session_id)
        .await?;
    let selected_agent = ops
        .get_agent(selected_agent_id, selected_session_id)
        .await?;
    let status = if summary.status == ProjectStatus::Ready {
        "ready"
    } else {
        "pending"
    };
    let review_runs = ops.recent_review_runs(project_id).await?;
    Ok(ProjectDetail {
        summary,
        maintainer_agent,
        agents,
        selected_agent_id,
        selected_agent,
        auth_status: status.to_string(),
        mcp_status: status.to_string(),
        review_runs,
    })
}

pub(crate) async fn create_project(
    ops: &impl ProjectCreateOps,
    request: CreateProjectRequest,
) -> Result<ProjectSummary> {
    let relay_installation_id = request.installation_id;
    let account_id = match normalize_optional_text(request.git_account_id.clone()) {
        Some(account_id) => account_id,
        None if relay_installation_id > 0 => {
            let installations = ops.list_github_installations().await?;
            let installation = installations
                .installations
                .into_iter()
                .find(|installation| installation.id == relay_installation_id)
                .ok_or_else(|| {
                    RuntimeError::InvalidInput("GitHub App installation not found".to_string())
                })?;
            ops.upsert_github_app_relay_account(relay_installation_id, &installation.account_login)
                .await?
        }
        None => {
            return Err(RuntimeError::InvalidInput(
                "git_account_id or installation_id is required".to_string(),
            ));
        }
    };
    let repository_ref = request
        .repository_full_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let owner = request.owner.trim();
            let repo = request.repo.trim();
            (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
        })
        .ok_or_else(|| {
            RuntimeError::InvalidInput("repository_full_name is required".to_string())
        })?;
    let repository = ops
        .verified_repository(&account_id, &repository_ref)
        .await?;
    let owner = repository.owner.clone();
    let repo = repository.name.clone();
    let repository_id = repository.id;
    let branch = normalize_optional_path_segment(request.branch.as_deref(), "branch")?
        .unwrap_or_else(|| repository.default_branch.clone());
    let name =
        normalize_optional_text(Some(request.name)).unwrap_or_else(|| format!("{owner}/{repo}"));
    let account = ops.git_account_summary(&account_id).await?;
    let installation_id = account.installation_id.unwrap_or(relay_installation_id);
    let installation_account = account
        .installation_account
        .clone()
        .or(account.login)
        .unwrap_or(account.label);
    let project_id = Uuid::new_v4();
    let planner_model = ops.planner_model().await?;
    let clone_url = github_clone_url(&owner, &repo);
    let system_prompt = project_maintainer_system_prompt(&owner, &repo, &clone_url, &branch);
    let maintainer = ops
        .create_project_maintainer_agent(ProjectMaintainerAgentRequest {
            project_id,
            name: format!("{name} Maintainer"),
            model: planner_model,
            docker_image: request.docker_image.clone(),
            system_prompt,
        })
        .await?;
    let created_at = now();
    let project = ProjectSummary {
        id: project_id,
        name,
        status: ProjectStatus::Creating,
        owner,
        repo,
        repository_full_name: repository.full_name,
        git_account_id: Some(account_id),
        repository_id,
        installation_id,
        installation_account,
        branch,
        docker_image: maintainer.docker_image.clone(),
        clone_status: ProjectCloneStatus::Pending,
        maintainer_agent_id: maintainer.id,
        created_at,
        updated_at: created_at,
        last_error: None,
        auto_review_enabled: request.auto_review_enabled,
        reviewer_extra_prompt: normalize_optional_text(request.reviewer_extra_prompt),
        review_status: if request.auto_review_enabled {
            ProjectReviewStatus::Idle
        } else {
            ProjectReviewStatus::Disabled
        },
        current_reviewer_agent_id: None,
        last_review_started_at: None,
        last_review_finished_at: None,
        next_review_at: None,
        last_review_outcome: None,
        review_last_error: None,
    };
    ops.save_project(&project).await?;
    ops.insert_project(project.clone()).await;
    ops.publish_project_event(ServiceEventKind::ProjectCreated {
        project: project.clone(),
    })
    .await;
    ops.start_project_workspace(project_id, maintainer.id).await;
    Ok(project)
}

pub(crate) async fn find_project_for_github_event(
    state: &RuntimeState,
    installation_id: Option<u64>,
    repository_id: Option<u64>,
    repository_full_name: Option<&str>,
) -> Option<ProjectId> {
    let repository_full_name = repository_full_name.map(str::to_ascii_lowercase);
    let projects = state.projects.read().await;
    for (project_id, project) in projects.iter() {
        let summary = project.summary.read().await;
        let installation_matches = installation_id
            .filter(|id| *id != 0)
            .is_none_or(|id| summary.installation_id == 0 || summary.installation_id == id);
        let repository_id_matches = repository_id
            .filter(|id| *id != 0)
            .is_none_or(|id| summary.repository_id == 0 || summary.repository_id == id);
        let full_name_matches = repository_full_name.as_ref().is_none_or(|full_name| {
            summary.repository_full_name.eq_ignore_ascii_case(full_name)
                || format!("{}/{}", summary.owner, summary.repo).eq_ignore_ascii_case(full_name)
        });
        if installation_matches && repository_id_matches && full_name_matches {
            return Some(*project_id);
        }
    }
    None
}

pub(crate) async fn project_agents(
    state: &RuntimeState,
    project_id: ProjectId,
) -> Vec<AgentSummary> {
    let agents = state.agents.read().await;
    let mut summaries = Vec::new();
    for agent in agents.values() {
        let summary = agent.summary.read().await.clone();
        if summary.project_id == Some(project_id) {
            summaries.push(summary);
        }
    }
    summaries.sort_by_key(|summary| summary.created_at);
    summaries
}

pub(crate) async fn project_auto_reviewer_agents(
    state: &RuntimeState,
    project_id: ProjectId,
) -> Vec<AgentSummary> {
    let maintainer_agent_id = match project(state, project_id).await {
        Ok(project) => project.summary.read().await.maintainer_agent_id,
        Err(RuntimeError::ProjectNotFound(_)) => return Vec::new(),
        Err(_) => return Vec::new(),
    };
    project_agents(state, project_id)
        .await
        .into_iter()
        .filter(|summary| {
            summary.role == Some(AgentRole::Reviewer)
                && summary.parent_id == Some(maintainer_agent_id)
                && !summary.status.is_terminal()
        })
        .collect()
}

pub(crate) async fn update_project(
    state: &RuntimeState,
    ops: &impl ProjectLifecycleOps,
    project_id: ProjectId,
    request: UpdateProjectRequest,
) -> Result<ProjectSummary> {
    let project = project(state, project_id).await?;
    let updated = {
        let mut summary = project.summary.write().await;
        if let Some(name) = request.name {
            let name = name.trim();
            if !name.is_empty() {
                summary.name = name.to_string();
            }
        }
        if let Some(docker_image) = request.docker_image {
            let docker_image = docker_image.trim();
            if !docker_image.is_empty() {
                summary.docker_image = docker_image.to_string();
            }
        }
        if let Some(enabled) = request.auto_review_enabled {
            summary.auto_review_enabled = enabled;
            if enabled && summary.review_status == ProjectReviewStatus::Disabled {
                summary.review_status = ProjectReviewStatus::Idle;
            }
            if !enabled {
                summary.review_status = ProjectReviewStatus::Disabled;
                summary.current_reviewer_agent_id = None;
                summary.next_review_at = None;
            }
        }
        if request.reviewer_extra_prompt.is_some() {
            summary.reviewer_extra_prompt = normalize_optional_text(request.reviewer_extra_prompt);
        }
        summary.updated_at = now();
        summary.clone()
    };
    ops.save_project(&updated).await?;
    ops.publish_project_event(ServiceEventKind::ProjectUpdated {
        project: updated.clone(),
    })
    .await;
    if updated.auto_review_enabled {
        ops.start_project_review_loop_if_ready(project_id).await?;
    } else {
        ops.stop_project_review_loop(project_id).await;
    }
    Ok(updated)
}

pub(crate) async fn delete_project(
    state: &RuntimeState,
    ops: &impl ProjectLifecycleOps,
    project_id: ProjectId,
) -> Result<()> {
    let project = project(state, project_id).await?;
    ops.stop_project_review_loop(project_id).await;
    let root_agents = project_agents(state, project_id)
        .await
        .into_iter()
        .filter(|agent| agent.parent_id.is_none())
        .map(|agent| agent.id)
        .collect::<Vec<_>>();
    {
        let mut summary = project.summary.write().await;
        summary.status = ProjectStatus::Deleting;
        summary.updated_at = now();
        ops.save_project(&summary).await?;
        ops.publish_project_event(ServiceEventKind::ProjectUpdated {
            project: summary.clone(),
        })
        .await;
    }
    for agent_id in root_agents {
        let _ = ops.delete_agent(agent_id).await;
    }
    ops.shutdown_project_mcp_manager(project_id).await;
    let _ = ops.delete_project_sidecar(project_id).await;
    let _ = ops.delete_project_review_workspace(project_id).await;
    ops.delete_project_from_store(project_id).await?;
    ops.remove_project_from_memory(project_id).await;
    ops.remove_project_skill_lock(project_id).await;
    ops.publish_project_event(ServiceEventKind::ProjectDeleted { project_id })
        .await;
    let _ = std::fs::remove_dir_all(ops.project_skill_cache_dir(project_id));
    Ok(())
}

pub(crate) async fn cancel_project(
    state: &RuntimeState,
    ops: &impl ProjectLifecycleOps,
    project_id: ProjectId,
) -> Result<()> {
    let project = project(state, project_id).await?;
    ops.stop_project_review_loop(project_id).await;
    for agent in project_agents(state, project_id).await {
        let _ = ops.cancel_project_agent(agent.id).await;
    }
    let updated = {
        let mut summary = project.summary.write().await;
        if matches!(summary.status, ProjectStatus::Creating) {
            summary.status = ProjectStatus::Failed;
            summary.last_error = Some("cancelled".to_string());
        }
        summary.updated_at = now();
        summary.clone()
    };
    ops.save_project(&updated).await?;
    ops.publish_project_event(ServiceEventKind::ProjectUpdated { project: updated })
        .await;
    ops.shutdown_project_mcp_manager(project_id).await;
    let _ = ops.delete_project_sidecar(project_id).await;
    Ok(())
}

pub(crate) async fn send_project_message(
    state: &RuntimeState,
    ops: &impl ProjectLifecycleOps,
    project_id: ProjectId,
    request: SendMessageRequest,
) -> Result<TurnId> {
    let project = project(state, project_id).await?;
    let maintainer_agent_id = project.summary.read().await.maintainer_agent_id;
    ops.send_project_agent_message(maintainer_agent_id, request)
        .await
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn project_maintainer_system_prompt(
    owner: &str,
    repo: &str,
    clone_url: &str,
    branch: &str,
) -> String {
    format!(
        r#"You are the Maintainer agent for the GitHub project `{owner}/{repo}`.

The repository clone URL is `{clone_url}`.
You run inside an isolated Docker container. The repository is cloned at `/workspace/repo`; use that path for local inspection and edits.
The selected branch is `{branch}`.

Security rules:
- Do not look for or persist GitHub credentials.
- Do not configure credential helpers.
- Do not write `~/.config/gh`, `~/.git-credentials`, long-lived `GH_TOKEN`, or long-lived `GITHUB_TOKEN`.
- Use MCP/GitHub API tools for GitHub reads and writes such as issues, branches, commits, and pull requests.
- Treat the deployment as no-webhook/no-public-inbound: refresh or poll state when you need current GitHub information.

Operational focus:
- Help the user review, plan, and maintain this repository.
- Prefer small, testable changes.
- Run relevant checks before reporting completion."#
    )
}

fn normalize_optional_path_segment(value: Option<&str>, field: &str) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.contains(char::is_whitespace)
        || value.starts_with('-')
        || value.starts_with('/')
        || value.contains("..")
        || value.contains('\\')
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{field} must be a safe Git ref name"
        )));
    }
    Ok(Some(value.to_string()))
}
