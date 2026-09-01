use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use mai_protocol::{AgentRole, CreateAgentRequest};
use pl_core::{AgentLifecycleAdapter, CloseLifecycleRequest, SpawnLifecycleRequest};
use pl_protocol::{AgentWorkspaceAssignmentSnapshot, AgentWorkspaceMode};

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
    runtime: Weak<AgentRuntime>,
    product_agent_id: mai_protocol::AgentId,
    assignment: AgentWorkspaceAssignmentSnapshot,
    ownership: SpawnProductOwnership,
    armed: AtomicBool,
}

enum SpawnProductOwnership {
    Borrowed,
    CreatedHere,
}

pub(crate) struct MaiCloseLease {
    product_agent_id: mai_protocol::AgentId,
    commit_started: AtomicBool,
}

impl MaiSpawnLease {
    fn borrowed(
        runtime: &Arc<AgentRuntime>,
        product_agent_id: mai_protocol::AgentId,
        assignment: AgentWorkspaceAssignmentSnapshot,
    ) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
            product_agent_id,
            assignment,
            ownership: SpawnProductOwnership::Borrowed,
            armed: AtomicBool::new(false),
        }
    }

    fn created_here(
        runtime: &Arc<AgentRuntime>,
        product_agent_id: mai_protocol::AgentId,
        assignment: AgentWorkspaceAssignmentSnapshot,
    ) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
            product_agent_id,
            assignment,
            ownership: SpawnProductOwnership::CreatedHere,
            armed: AtomicBool::new(true),
        }
    }

    async fn rollback(&self) -> Result<()> {
        if !self.armed.load(Ordering::Acquire) {
            return Ok(());
        }
        let runtime = self.runtime.upgrade().ok_or_else(|| {
            RuntimeError::InvalidInput("spawn lease lost its runtime".to_string())
        })?;
        rollback_created_spawn(runtime, self.product_agent_id).await?;
        self.armed.store(false, Ordering::Release);
        Ok(())
    }

    fn commit(&self) {
        self.armed.store(false, Ordering::Release);
    }
}

impl Drop for MaiSpawnLease {
    fn drop(&mut self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let Some(runtime) = self.runtime.upgrade() else {
            tracing::warn!(
                agent_id = %self.product_agent_id,
                "spawn lease cleanup was abandoned after runtime shutdown"
            );
            return;
        };
        let agent_id = self.product_agent_id;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                agent_id = %agent_id,
                "spawn lease cleanup was abandoned without a Tokio runtime"
            );
            return;
        };
        handle.spawn(async move {
            if let Err(error) = rollback_created_spawn(runtime, agent_id).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    "dropped spawn lease failed to release resources: {error}"
                );
            }
        });
    }
}

async fn rollback_created_spawn(
    runtime: Arc<AgentRuntime>,
    agent_id: mai_protocol::AgentId,
) -> Result<()> {
    match agents::delete_agent(runtime.as_ref(), agent_id).await {
        Ok(()) | Err(RuntimeError::AgentNotFound(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

impl AgentLifecycleAdapter for MaiAgentLifecycle {
    type Error = RuntimeError;
    type SpawnLease = MaiSpawnLease;
    type CloseLease = MaiCloseLease;

    async fn prepare_spawn(&self, request: SpawnLifecycleRequest) -> Result<Self::SpawnLease> {
        let runtime = self.runtime()?;
        let profile = request.agent_profile.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput("child Agent spawn has no frozen Profile".to_string())
        })?;
        if profile.profile_id != request.child.identity.role.as_str() {
            return Err(RuntimeError::InvalidInput(format!(
                "frozen Profile `{}` does not match child role `{}`",
                profile.profile_id, request.child.identity.role
            )));
        }
        let (parent_id, parent) =
            super::turn_factory::product_agent(&runtime, &request.parent.identity.id).await?;
        let parent_summary = parent.summary.read().await.clone();
        let workspace_root = if parent_summary.project_id.is_some() {
            crate::projects::workspace::AGENT_WORKSPACE_REPO_PATH
        } else {
            "/workspace"
        };
        let assignment = workspace_assignment(profile, workspace_root, &request)?;
        if let Ok((product_agent_id, _)) =
            super::turn_factory::product_agent(&runtime, &request.child.identity.id).await
        {
            return Ok(MaiSpawnLease::borrowed(
                &runtime,
                product_agent_id,
                assignment,
            ));
        }
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
        let product_agent_id = mai_protocol::AgentId::parse_str(request.child.identity.id.as_str())
            .map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "spawned Thread id `{}` is not a product UUID: {error}",
                    request.child.identity.id
                ))
            })?;
        let resource = runtime
            .create_agent_resource_with_container_source(
                product_agent_id,
                CreateAgentRequest {
                    name,
                    provider_id: Some(profile.provider_id.clone()),
                    model: Some(profile.model.clone()),
                    reasoning_effort: profile.effort.clone(),
                    docker_image: Some(parent_summary.docker_image.clone()),
                    parent_id: Some(parent_id),
                    system_prompt: Some(profile.system_instructions.clone()),
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
        resource.commit();
        Ok(MaiSpawnLease::created_here(
            &runtime,
            product_agent_id,
            assignment,
        ))
    }

    fn workspace_assignment(
        &self,
        lease: &Self::SpawnLease,
    ) -> Result<Option<AgentWorkspaceAssignmentSnapshot>> {
        Ok(Some(lease.assignment.clone()))
    }

    fn initial_context(
        &self,
        lease: &Self::SpawnLease,
    ) -> Result<Vec<pl_core::PinnedContextSection>> {
        let warning = match lease.assignment.mode {
            AgentWorkspaceMode::Unrestricted => {
                "This Profile adds no workspace restriction beyond the product container boundary."
            }
            AgentWorkspaceMode::Directory => {
                "writablePaths is enforced by Mai's built-in file backend; shell, Git, and MCP remain cooperative capabilities and must honor the same receipt."
            }
            AgentWorkspaceMode::Worktree => {
                return Err(RuntimeError::InvalidInput(
                    "mai does not advertise worktree Agent Profiles".to_string(),
                ));
            }
        };
        let receipt = serde_json::to_string_pretty(&lease.assignment)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        Ok(vec![pl_core::context_section(
            "agent.workspace",
            1,
            "Frozen Agent Workspace",
            format!("{warning}\n\nCanonical workspace receipt:\n{receipt}"),
        )?])
    }

    async fn activate_spawn(&self, lease: &Self::SpawnLease) -> Result<()> {
        self.runtime()?.agent(lease.product_agent_id).await?;
        lease.commit();
        Ok(())
    }

    async fn rollback_spawn(
        &self,
        lease: Self::SpawnLease,
        _reason: pl_core::SpawnRollbackReason,
    ) -> Result<()> {
        match lease.ownership {
            SpawnProductOwnership::Borrowed => return Ok(()),
            SpawnProductOwnership::CreatedHere => {}
        }
        lease.rollback().await
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
        runtime.tool_sets.remove(lease.product_agent_id).await;
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

fn workspace_assignment(
    profile: &pl_protocol::AgentProfileSnapshot,
    project_root: &str,
    request: &SpawnLifecycleRequest,
) -> Result<AgentWorkspaceAssignmentSnapshot> {
    let metadata_mode: AgentWorkspaceMode =
        serde_json::from_value(request.metadata.get("workspaceMode").cloned().ok_or_else(
            || RuntimeError::InvalidInput("spawn metadata has no workspace mode".to_string()),
        )?)
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    if metadata_mode != profile.workspace_mode {
        return Err(RuntimeError::InvalidInput(format!(
            "spawn workspace mode `{}` does not match frozen Profile mode `{}`",
            metadata_mode.label(),
            profile.workspace_mode.label()
        )));
    }
    let writable_paths: Option<Vec<String>> = serde_json::from_value(
        request
            .metadata
            .get("writablePaths")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    super::agent_workspace_assignment(profile, project_root, writable_paths)
}
