use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use mai_protocol::{
    AgentId, AgentRole, AgentSessionSummary, AgentStatus, AgentSummary, CreateAgentRequest,
    ProjectId, TaskId, TokenUsage, now,
};
use mai_store::ProviderSelection;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::{initial_session_record, normalize_reasoning_effort};
use crate::Result;
use crate::state::AgentRecord;

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
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> impl Future<Output = Result<ProviderSelection>> + Send;

    fn save_agent(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn save_agent_session(
        &self,
        agent_id: AgentId,
        session: &AgentSessionSummary,
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
        .resolve_provider(request.provider_id.as_deref(), request.model.as_deref())
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
        status: AgentStatus::Created,
        container_id: None,
        docker_image,
        provider_id: provider_selection.provider.id.clone(),
        provider_name: provider_selection.provider.name.clone(),
        model: provider_selection.model.id.clone(),
        reasoning_effort,
        created_at,
        updated_at: created_at,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    ops.save_agent(&summary, system_prompt.as_deref()).await?;
    let session = initial_session_record(context.task_id.is_some());
    ops.save_agent_session(id, &session.summary).await?;

    let agent = Arc::new(AgentRecord {
        summary: RwLock::new(summary.clone()),
        sessions: Mutex::new(vec![session]),
        container: RwLock::new(None),
        mcp: RwLock::new(None),
        system_prompt,
        turn_lock: Mutex::new(()),
        cancel_requested: AtomicBool::new(false),
        active_turn: std::sync::Mutex::new(None),
        pending_inputs: Mutex::new(std::collections::VecDeque::new()),
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
