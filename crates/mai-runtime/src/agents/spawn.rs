use std::future::Future;
use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole, AgentStatus, AgentSummary, SessionId, TaskId, TurnId};

use super::ContainerSource;
use crate::state::{AgentRecord, CollabInput};
use crate::{Result, RuntimeError};

pub(crate) struct SpawnChildAgentRequest {
    pub(crate) name: Option<String>,
    pub(crate) role: AgentRole,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) use_role_model: bool,
    pub(crate) fork_context: bool,
    pub(crate) collab_input: CollabInput,
}

pub(crate) struct SpawnChildAgentResult {
    pub(crate) agent: AgentSummary,
    pub(crate) turn_id: Option<TurnId>,
}

/// Supplies the runtime operations required to spawn child agents without
/// exposing the full facade to agent lifecycle code.
pub(crate) trait AgentSpawnOps: Send + Sync {
    fn agent(&self, agent_id: AgentId) -> impl Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn ensure_agent_container(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
    ) -> impl Future<Output = Result<String>> + Send;

    fn role_model(
        &self,
        role: AgentRole,
    ) -> impl Future<Output = Result<mai_protocol::AgentModelPreference>> + Send;

    fn create_agent_with_container_source(
        &self,
        request: mai_protocol::CreateAgentRequest,
        source: ContainerSource,
        task_id: Option<TaskId>,
        project_id: Option<mai_protocol::ProjectId>,
        role: Option<AgentRole>,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn fork_agent_context(
        &self,
        parent_id: AgentId,
        child_id: AgentId,
    ) -> impl Future<Output = Result<()>> + Send;

    fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> impl Future<Output = Result<SessionId>> + Send;

    fn prepare_turn(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<(Arc<AgentRecord>, TurnId)>> + Send;

    fn spawn_turn(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    );
}

pub(crate) async fn spawn_task_role_agent(
    ops: &impl AgentSpawnOps,
    parent_agent_id: AgentId,
    role: AgentRole,
    name: Option<String>,
) -> Result<AgentSummary> {
    let parent = ops.agent(parent_agent_id).await?;
    let parent_summary = parent.summary.read().await.clone();
    let task_id = parent_summary.task_id.ok_or_else(|| {
        RuntimeError::InvalidInput("parent agent is not attached to a task".to_string())
    })?;
    let parent_container_id = ops
        .ensure_agent_container(&parent, parent_summary.status.clone())
        .await?;
    let model = ops.role_model(role).await?;
    ops.create_agent_with_container_source(
        mai_protocol::CreateAgentRequest {
            name,
            provider_id: Some(model.provider_id),
            model: Some(model.model),
            reasoning_effort: model.reasoning_effort,
            docker_image: Some(parent_summary.docker_image.clone()),
            parent_id: Some(parent_agent_id),
            system_prompt: Some(super::task_role_system_prompt(role).to_string()),
        },
        ContainerSource::CloneFrom {
            parent_container_id,
            docker_image: parent_summary.docker_image,
            workspace_volume: None,
            repo_mount: None,
        },
        Some(task_id),
        parent_summary.project_id,
        Some(role),
    )
    .await
}

pub(crate) async fn spawn_child_agent(
    ops: &impl AgentSpawnOps,
    parent_agent_id: AgentId,
    request: SpawnChildAgentRequest,
) -> Result<SpawnChildAgentResult> {
    let parent = ops.agent(parent_agent_id).await?;
    let parent_status = parent.summary.read().await.status.clone();
    let parent_summary = parent.summary.read().await.clone();
    let parent_container_id = ops.ensure_agent_container(&parent, parent_status).await?;
    let parent_docker_image = parent_summary.docker_image.clone();
    let (provider_id, model, reasoning_effort) = if request.use_role_model {
        let child_model = ops.role_model(request.role).await?;
        (
            child_model.provider_id,
            child_model.model,
            child_model.reasoning_effort,
        )
    } else {
        (
            parent_summary.provider_id.clone(),
            request
                .model
                .unwrap_or_else(|| parent_summary.model.clone()),
            request
                .reasoning_effort
                .or_else(|| parent_summary.reasoning_effort.clone()),
        )
    };
    let created = ops
        .create_agent_with_container_source(
            mai_protocol::CreateAgentRequest {
                name: request.name,
                provider_id: Some(provider_id),
                model: Some(model),
                reasoning_effort,
                docker_image: Some(parent_docker_image.clone()),
                parent_id: Some(parent_agent_id),
                system_prompt: Some(super::task_role_system_prompt(request.role).to_string()),
            },
            ContainerSource::CloneFrom {
                parent_container_id,
                docker_image: parent_docker_image,
                workspace_volume: None,
                repo_mount: None,
            },
            parent_summary.task_id,
            parent_summary.project_id,
            Some(request.role),
        )
        .await?;
    if request.fork_context {
        ops.fork_agent_context(parent_agent_id, created.id).await?;
    }
    let turn_id = if let Some(message) = request.collab_input.message {
        let session_id = AgentSpawnOps::resolve_session_id(ops, created.id, None).await?;
        let (agent, turn_id) = ops.prepare_turn(created.id).await?;
        ops.spawn_turn(
            &agent,
            created.id,
            session_id,
            turn_id,
            message,
            request.collab_input.skill_mentions,
        );
        Some(turn_id)
    } else {
        None
    };
    Ok(SpawnChildAgentResult {
        agent: created,
        turn_id,
    })
}
