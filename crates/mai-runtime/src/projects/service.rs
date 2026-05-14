use std::future::Future;
use std::sync::Arc;

use mai_docker::DockerClient;
use mai_protocol::{
    AgentDetail, AgentId, AgentRole, AgentSummary, ProjectDetail, ProjectId, ProjectStatus,
    ProjectSummary, SessionId, preview,
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
