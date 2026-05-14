use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use mai_docker::DockerClient;
use mai_protocol::{
    AgentDetail, AgentId, AgentRole, AgentSummary, ProjectDetail, ProjectId, ProjectReviewStatus,
    ProjectStatus, ProjectSummary, ServiceEventKind, SessionId, UpdateProjectRequest, now, preview,
};

use super::mcp::PROJECT_WORKSPACE_PATH;
use crate::state::{ProjectRecord, RuntimeState};
use crate::{Result, RuntimeError};

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

pub(crate) async fn prepare_copied_workspace(
    docker: &DockerClient,
    container_id: &str,
) -> Result<()> {
    let command = format!(
        "set -eu\n\
         owner=$(id -u):$(id -g)\n\
         chown -R \"$owner\" {workspace} 2>/dev/null || git config --global --add safe.directory {workspace}",
        workspace = shell_quote(PROJECT_WORKSPACE_PATH),
    );
    let output = docker
        .exec_shell(container_id, &command, Some("/"), Some(60))
        .await?;
    if output.status != 0 {
        let combined = format!("{}\n{}", output.stderr, output.stdout);
        let message = preview(combined.trim(), 500);
        return Err(RuntimeError::InvalidInput(format!(
            "repository workspace ownership setup failed: {message}"
        )));
    }
    Ok(())
}

pub(crate) async fn clone_repository_in_sidecar(
    docker: &DockerClient,
    container_id: &str,
    repo_url: &str,
    branch: &str,
    token: &str,
) -> Result<()> {
    let branch_arg = if branch.is_empty() {
        String::new()
    } else {
        format!(" --branch {}", shell_quote(branch))
    };
    let command = format!(
        "set -eu\n\
         tmp=$(mktemp -d)\n\
         askpass=\"$tmp/askpass.sh\"\n\
         cleanup() {{ rm -rf \"$tmp\"; }}\n\
         trap cleanup EXIT HUP INT TERM\n\
         cat >\"$askpass\" <<'EOF'\n\
#!/bin/sh\n\
case \"$1\" in\n\
  *Username*) printf '%s\\n' x-access-token ;;\n\
  *Password*) printf '%s\\n' \"$MAI_GITHUB_INSTALLATION_TOKEN\" ;;\n\
  *) printf '\\n' ;;\n\
esac\n\
EOF\n\
         chmod 700 \"$askpass\"\n\
         rm -rf {workspace}\n\
         GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=\"$askpass\" git -c credential.helper= clone{branch_arg} -- {repo_url} {workspace}",
        workspace = shell_quote(PROJECT_WORKSPACE_PATH),
        repo_url = shell_quote(repo_url),
    );
    let output = docker
        .exec_shell_env(
            container_id,
            &command,
            Some("/"),
            Some(600),
            &[(
                "MAI_GITHUB_INSTALLATION_TOKEN".to_string(),
                token.to_string(),
            )],
        )
        .await?;
    if output.status != 0 {
        let combined = format!("{}\n{}", output.stderr, output.stdout);
        let message = preview(redact_secret(combined.trim(), token).trim(), 500);
        return Err(RuntimeError::InvalidInput(format!(
            "repository clone failed in project sidecar: {message}"
        )));
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
