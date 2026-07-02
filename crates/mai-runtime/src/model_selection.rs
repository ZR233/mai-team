use std::sync::Arc;

use mai_protocol::{AgentId, ServiceEventKind, SessionId, TurnId, now};
use mai_store::ProviderSelection;
use serde_json::json;

use crate::agents;
use crate::deps::RuntimeDeps;
use crate::events::RuntimeEvents;
use crate::state::AgentRecord;
use crate::turn::persistence::{AgentLogRecord, record_agent_log};
use crate::{Result, RuntimeError};

pub(crate) struct AgentModelSelection {
    pub(crate) provider_id: String,
    pub(crate) model_name: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) provider_selection: ProviderSelection,
}

pub(crate) async fn resolve_agent_model_selection(
    deps: &RuntimeDeps,
    events: &RuntimeEvents,
    agent: &Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
) -> Result<AgentModelSelection> {
    let summary = agent.summary.read().await.clone();
    let provider_id = summary.provider_id.clone();
    let model_name = summary.model.clone();
    match deps
        .store
        .resolve_provider(Some(&provider_id), Some(&model_name))
        .await
    {
        Ok(provider_selection) => {
            return Ok(AgentModelSelection {
                provider_id,
                model_name,
                reasoning_effort: summary.reasoning_effort,
                provider_selection,
            });
        }
        Err(err) if is_stale_provider_selection_store_error(&err) => {
            let stale_error = err.to_string();
            let provider_selection = deps.store.resolve_provider(None, None).await?;
            let reasoning_effort =
                agents::normalize_reasoning_effort(&provider_selection.model, None, true)?;
            tracing::warn!(
                agent_id = %agent_id,
                stale_provider_id = %provider_id,
                stale_model = %model_name,
                provider_id = %provider_selection.provider.id,
                model = %provider_selection.model.id,
                error = %stale_error,
                "agent model selection is stale; falling back to the default provider"
            );
            let updated = {
                let mut summary = agent.summary.write().await;
                summary.provider_id = provider_selection.provider.id.clone();
                summary.provider_name = provider_selection.provider.name.clone();
                summary.model = provider_selection.model.id.clone();
                summary.reasoning_effort = reasoning_effort.clone();
                summary.updated_at = now();
                summary.clone()
            };
            deps.store
                .save_agent(&updated, agent.system_prompt.as_deref())
                .await?;
            events
                .publish(ServiceEventKind::AgentUpdated {
                    agent: updated.clone(),
                })
                .await;
            record_agent_log(
                deps.store.as_ref(),
                AgentLogRecord {
                    agent_id,
                    session_id,
                    turn_id,
                    level: "warn",
                    category: "runtime",
                    message: "agent model selection fell back to default provider",
                    details: json!({
                        "stale_provider_id": provider_id,
                        "stale_model": model_name,
                        "provider_id": updated.provider_id,
                        "model": updated.model,
                        "error": stale_error,
                    }),
                },
            )
            .await;
            return Ok(AgentModelSelection {
                provider_id: updated.provider_id,
                model_name: updated.model,
                reasoning_effort: updated.reasoning_effort,
                provider_selection,
            });
        }
        Err(err) => return Err(err.into()),
    }
}

pub(crate) fn is_stale_agent_model_selection_error(err: &RuntimeError) -> bool {
    let RuntimeError::Store(err) = err else {
        return false;
    };
    is_stale_provider_selection_store_error(err)
}

fn is_stale_provider_selection_store_error(err: &mai_store::StoreError) -> bool {
    let mai_store::StoreError::InvalidConfig(message) = err else {
        return false;
    };
    (message.starts_with("provider `") && message.ends_with("` not found"))
        || (message.starts_with("model `")
            && message.contains("` is not configured for provider `"))
}
