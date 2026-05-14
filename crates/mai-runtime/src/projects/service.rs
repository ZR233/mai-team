use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentDetail, AgentId, AgentRole, AgentSummary, ProjectDetail, ProjectId, ProjectStatus,
    ProjectSummary, SessionId,
};

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
