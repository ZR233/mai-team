use std::future::Future;
use std::sync::Arc;

use mai_protocol::{AgentId, AgentSummary, UpdateAgentRequest, now};
use mai_store::ProviderSelection;

use super::normalize_reasoning_effort;
use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

/// Provides the narrow runtime capabilities needed to update agent model config.
pub(crate) trait AgentUpdateOps: Send + Sync {
    fn agent(&self, agent_id: AgentId) -> impl Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn resolve_provider(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> impl Future<Output = Result<ProviderSelection>> + Send;

    fn persist_agent(&self, agent: Arc<AgentRecord>) -> impl Future<Output = Result<()>> + Send;

    fn publish_agent_updated(&self, agent: AgentSummary) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn update_agent(
    ops: &impl AgentUpdateOps,
    agent_id: AgentId,
    request: UpdateAgentRequest,
) -> Result<AgentSummary> {
    let agent = ops.agent(agent_id).await?;
    {
        let summary = agent.summary.read().await;
        if !summary.status.can_start_turn() || summary.current_turn.is_some() {
            return Err(RuntimeError::AgentBusy(agent_id));
        }
    }
    let current = agent.summary.read().await.clone();
    let provider_id = request
        .provider_id
        .as_deref()
        .or(Some(&current.provider_id));
    let model = request.model.as_deref().or(Some(&current.model));
    let provider_selection = ops.resolve_provider(provider_id, model).await?;
    let requested_reasoning_effort = if request.reasoning_effort.is_some()
        || provider_selection.model.id != current.model
        || provider_selection.provider.id != current.provider_id
    {
        request.reasoning_effort
    } else {
        current.reasoning_effort
    };
    let reasoning_effort = normalize_reasoning_effort(
        &provider_selection.model,
        requested_reasoning_effort.as_deref(),
        true,
    )?;
    let updated = {
        let mut summary = agent.summary.write().await;
        summary.provider_id = provider_selection.provider.id.clone();
        summary.provider_name = provider_selection.provider.name.clone();
        summary.model = provider_selection.model.id.clone();
        summary.reasoning_effort = reasoning_effort;
        summary.updated_at = now();
        summary.clone()
    };
    ops.persist_agent(Arc::clone(&agent)).await?;
    ops.publish_agent_updated(updated.clone()).await;
    Ok(updated)
}
