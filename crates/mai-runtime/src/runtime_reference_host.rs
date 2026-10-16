//! 领域状态机到 canonical Thread、MCP 和产品 Agent 投影的适配。

use super::*;
use mai_references::{ReferenceEnvironmentHost, ReferenceError};
use mai_store::PersistedReferenceBinding;

impl ReferenceEnvironmentHost for AgentRuntime {
    async fn quiesce(
        &self,
        agent_id: AgentId,
        policy: ReferenceApplyPolicy,
    ) -> mai_references::Result<()> {
        // 保留 canonical Thread 和已受理输入；只中断真正执行中的 Turn。
        // 等待准入的输入尚无 Turn，不能用 interrupt(None) 将它取消。
        if policy == ReferenceApplyPolicy::StopAndApply {
            self.cancel_reference_turn(agent_id)
                .await
                .map_err(reference_host_error)?;
        }
        let admission = self.references.admissions.agent(agent_id);
        while admission.gate.admitted_turns() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let agent = self.agent(agent_id).await.map_err(reference_host_error)?;
        if let Some(mcp) = agent.mcp.write().await.take() {
            mcp.shutdown().await;
        }
        Ok(())
    }

    async fn rebind(
        &self,
        binding: PersistedReferenceBinding,
        container: ContainerHandle,
    ) -> mai_references::Result<()> {
        let agent = self
            .agent(binding.agent_id)
            .await
            .map_err(reference_host_error)?;
        *agent.container.write().await = Some(container);
        agent.summary.write().await.container_id = Some(binding.container_id.clone());
        let settings = agents::AgentContainerOps::agent_mcp_runtime_config(self, &agent)
            .await
            .map_err(reference_host_error)?;
        let mcp = agents::AgentContainerOps::start_agent_mcp_runtime(
            self,
            binding.agent_id,
            binding.container_id,
            settings,
        )
        .await
        .map_err(reference_host_error)?;
        let failures = mcp.required_failures().await;
        if !failures.is_empty() {
            mcp.shutdown().await;
            return Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentApplyFailed,
                "候选环境的必需 MCP 服务未就绪",
            ));
        }
        *agent.mcp.write().await = Some(Arc::new(mcp));
        Ok(())
    }

    async fn activate(&self, binding: PersistedReferenceBinding) -> mai_references::Result<()> {
        let agent = self
            .agent(binding.agent_id)
            .await
            .map_err(reference_host_error)?;
        {
            let mut summary = agent.summary.write().await;
            summary.container_id = Some(binding.container_id);
            summary.updated_at = now();
            summary.resource.state = AgentResourceState::Ready;
            summary.resource.error = None;
        }
        self.persist_agent(&agent)
            .await
            .map_err(reference_host_error)?;
        let runtime = self.self_ref.get().and_then(Weak::upgrade).ok_or_else(|| {
            ReferenceError::new(ReferenceErrorCode::EnvironmentApplyFailed, "Runtime 已关闭")
        })?;
        // 非驻留 Thread 以后按新租约惰性装配。此处不得递归装配祖先：
        // 每个祖先可能正在持有自己的环境换代生命周期锁。
        if let Some(resident) = self.resident_thread(binding.agent_id) {
            let tools =
                crate::thread_host::assemble_thread_tools(crate::thread_host::ThreadToolRequest {
                    runtime,
                    agent,
                    thread_id: binding.agent_id.to_string(),
                })
                .await
                .map_err(reference_host_error)?;
            let snapshot = resident.handle.snapshot();
            resident
                .handle
                .reconfigure(pl_core::thread::IdleReconfiguration {
                    expected_sequence: snapshot.commit_sequence,
                    application: pl_core::thread::extensions::ApplicationUpdate {
                        mutations: Vec::new(),
                        facts: Vec::new(),
                    },
                    context: None,
                    model_update: None,
                    replace_tools: true,
                    remove_tools: Vec::new(),
                    tools: tools.registrations,
                })
                .await
                .map_err(RuntimeError::from)
                .map_err(reference_host_error)?;
        }
        Ok(())
    }
}

fn reference_host_error(error: RuntimeError) -> ReferenceError {
    match error {
        RuntimeError::Reference(error) => error,
        error => ReferenceError::new(
            ReferenceErrorCode::EnvironmentApplyFailed,
            error.to_string(),
        )
        .with_cause(error),
    }
}

impl AgentRuntime {
    /// 已持久化停止意图后立即取消当前 Turn，取消不等待 Docker 换代并发额度。
    pub(super) async fn cancel_reference_turn(&self, agent_id: AgentId) -> Result<()> {
        if let Some(resident) = self.resident_thread(agent_id) {
            let running = resident
                .handle
                .snapshot()
                .turns
                .iter()
                .find(|turn| turn.state == pl_core::thread::TurnState::Running)
                .map(|turn| turn.turn_id.clone());
            if let Some(turn_id) = running {
                resident.handle.cancel_running_turn(turn_id).await?;
            }
        }
        Ok(())
    }
}
