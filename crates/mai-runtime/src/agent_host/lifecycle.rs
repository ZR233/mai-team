use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use mai_protocol::{AgentRole, CreateAgentRequest};
use pl_core::{AgentLifecycleAdapter, CloseLifecycleRequest, SpawnLifecycleRequest};

use crate::{AgentRuntime, Result, RuntimeError, agents};

#[derive(Clone)]
pub(crate) struct MaiAgentLifecycle {
    runtime: Weak<AgentRuntime>,
}

impl MaiAgentLifecycle {
    pub(crate) fn new(runtime: Weak<AgentRuntime>) -> Self {
        Self { runtime }
    }

    fn runtime(&self) -> Result<Arc<AgentRuntime>> {
        self.runtime.upgrade().ok_or_else(|| {
            RuntimeError::InvalidInput("mai agent lifecycle is shutting down".to_string())
        })
    }
}

pub(crate) struct MaiSpawnLease {
    product_agent_id: mai_protocol::AgentId,
}

pub(crate) struct MaiCloseLease {
    product_agent_id: mai_protocol::AgentId,
    commit_started: AtomicBool,
}

impl AgentLifecycleAdapter for MaiAgentLifecycle {
    type Error = RuntimeError;
    type SpawnLease = MaiSpawnLease;
    type CloseLease = MaiCloseLease;

    async fn prepare_spawn(&self, request: SpawnLifecycleRequest) -> Result<Self::SpawnLease> {
        let runtime = self.runtime()?;
        if let Ok((product_agent_id, _)) =
            super::turn_factory::product_agent(&runtime, &request.child.identity.id).await
        {
            return Ok(MaiSpawnLease { product_agent_id });
        }
        let (parent_id, parent) =
            super::turn_factory::product_agent(&runtime, &request.parent.identity.id).await?;
        let parent_summary = parent.summary.read().await.clone();
        let parent_container_id = runtime.container_id(parent_id).await?;
        let role = AgentRole::from_str(request.child.identity.role.as_str()).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "unsupported mai child role `{}`: {error}",
                request.child.identity.role
            ))
        })?;
        let name = request
            .metadata
            .get("taskName")
            .or_else(|| request.metadata.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let summary = runtime
            .create_agent_resource_with_container_source(
                CreateAgentRequest {
                    name,
                    provider_id: Some(parent_summary.provider_id.clone()),
                    model: Some(parent_summary.model.clone()),
                    reasoning_effort: parent_summary.reasoning_effort.clone(),
                    docker_image: Some(parent_summary.docker_image.clone()),
                    parent_id: Some(parent_id),
                    system_prompt: Some(agents::task_role_system_prompt(role).to_string()),
                },
                agents::ContainerSource::CloneFrom {
                    parent_container_id,
                    docker_image: parent_summary.docker_image,
                    workspace_volume: None,
                },
                parent_summary.task_id,
                parent_summary.project_id,
                Some(role),
            )
            .await?;
        let agent = runtime.agent(summary.id).await?;
        *agent.runtime_agent_id.write().await = request.child.identity.id.clone();
        runtime
            .deps
            .store
            .save_agent_with_runtime_id(
                &summary,
                agent.system_prompt.as_deref(),
                request.child.identity.id.as_str(),
            )
            .await?;
        Ok(MaiSpawnLease {
            product_agent_id: summary.id,
        })
    }

    async fn activate_spawn(&self, lease: &Self::SpawnLease) -> Result<()> {
        self.runtime()?.agent(lease.product_agent_id).await?;
        Ok(())
    }

    async fn rollback_spawn(&self, lease: Self::SpawnLease) -> Result<()> {
        let runtime = self.runtime()?;
        match runtime.close_agent(lease.product_agent_id).await {
            Ok(_) | Err(RuntimeError::AgentNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        if runtime.agent(lease.product_agent_id).await.is_ok() {
            runtime
                .cleanup_agent_workspace(lease.product_agent_id)
                .await?;
            let agent = runtime.agent(lease.product_agent_id).await?;
            runtime
                .set_agent_resource_state(&agent, mai_protocol::AgentResourceState::Deleted, None)
                .await?;
        }
        Ok(())
    }

    async fn prepare_close(&self, request: CloseLifecycleRequest) -> Result<Self::CloseLease> {
        let runtime = self.runtime()?;
        let (product_agent_id, _) =
            super::turn_factory::product_agent(&runtime, &request.agent.identity.id).await?;
        Ok(MaiCloseLease {
            product_agent_id,
            commit_started: AtomicBool::new(false),
        })
    }

    async fn commit_close(&self, lease: &Self::CloseLease) -> Result<()> {
        lease.commit_started.store(true, Ordering::Release);
        let runtime = self.runtime()?;
        runtime.close_agent(lease.product_agent_id).await?;
        runtime
            .cleanup_agent_workspace(lease.product_agent_id)
            .await?;
        let agent = runtime.agent(lease.product_agent_id).await?;
        runtime
            .set_agent_resource_state(&agent, mai_protocol::AgentResourceState::Deleted, None)
            .await?;
        Ok(())
    }

    async fn rollback_close(&self, lease: Self::CloseLease) -> Result<()> {
        if !lease.commit_started.load(Ordering::Acquire) {
            return Ok(());
        }
        let runtime = self.runtime()?;
        if let Ok(agent) = runtime.agent(lease.product_agent_id).await {
            runtime
                .set_agent_resource_state(
                    &agent,
                    mai_protocol::AgentResourceState::Failed,
                    Some("agent resource close failed; cleanup may be partial".to_string()),
                )
                .await?;
        }
        Err(RuntimeError::InvalidInput(
            "agent resource close cannot be rolled back after destructive cleanup".to_string(),
        ))
    }
}
