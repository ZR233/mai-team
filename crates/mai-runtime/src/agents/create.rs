use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentRole, AgentSummary, CreateAgentRequest, ProjectId, TaskId, TokenUsage, now,
};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::normalize_reasoning_effort;
use crate::state::AgentRecord;
use crate::{ProviderSelection, Result};

/// Context supplied when creating an agent record.
pub(crate) struct CreateAgentRecordContext {
    pub(crate) task_id: Option<TaskId>,
    pub(crate) project_id: Option<ProjectId>,
    pub(crate) role: Option<AgentRole>,
}

/// Provides the narrow persistence/state/event capabilities needed to create
/// an agent record and its initial session.
pub(crate) trait AgentCreateOps: Send + Sync {
    fn default_docker_image(&self) -> String;

    fn resolve_provider(
        &self,
        role: AgentRole,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> impl Future<Output = Result<ProviderSelection>> + Send;

    fn save_agent(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn insert_agent(&self, agent: Arc<AgentRecord>) -> impl Future<Output = ()> + Send;

    fn publish_agent_created(&self, agent: AgentSummary) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn create_agent_record(
    ops: &impl AgentCreateOps,
    request: CreateAgentRequest,
    context: CreateAgentRecordContext,
) -> Result<Arc<AgentRecord>> {
    let id = Uuid::new_v4();
    let created_at = now();
    let name = request
        .name
        .unwrap_or_else(|| format!("agent-{}", super::short_id(id)));
    let provider_selection = ops
        .resolve_provider(
            context.role.unwrap_or_default(),
            request.provider_id.as_deref(),
            request.model.as_deref(),
        )
        .await?;
    let reasoning_effort = normalize_reasoning_effort(
        &provider_selection.model,
        request.reasoning_effort.as_deref(),
        true,
    )?;
    let default_docker_image = ops.default_docker_image();
    let docker_image = resolve_docker_image(&default_docker_image, request.docker_image.as_deref());
    let system_prompt = request.system_prompt;
    let summary = AgentSummary {
        id,
        parent_id: request.parent_id,
        task_id: context.task_id,
        project_id: context.project_id,
        role: context.role,
        name,
        state: mai_protocol::AgentState::default(),
        container_id: None,
        docker_image,
        provider_id: provider_selection.provider.id.clone(),
        provider_name: provider_selection.provider.name.clone(),
        model: provider_selection.model.id.clone(),
        reasoning_effort,
        created_at,
        updated_at: created_at,
        token_usage: TokenUsage::default(),
    };
    ops.save_agent(&summary, system_prompt.as_deref()).await?;
    let agent = Arc::new(AgentRecord {
        runtime_agent_id: RwLock::new(pl_core::AgentId::new(id.to_string())?),
        summary: RwLock::new(summary.clone()),
        container: RwLock::new(None),
        mcp: RwLock::new(None),
        review_context: RwLock::new(None),
        system_prompt,
    });

    ops.insert_agent(Arc::clone(&agent)).await;
    ops.publish_agent_created(summary).await;
    Ok(agent)
}

fn resolve_docker_image(default_image: &str, requested: Option<&str>) -> String {
    requested
        .map(str::trim)
        .filter(|image| !image.is_empty())
        .unwrap_or(default_image)
        .to_string()
}
