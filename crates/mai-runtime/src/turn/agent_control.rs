use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use mai_protocol::{AgentId, AgentRole, AgentStatus as MaiAgentStatus, AgentSummary};
use pl_core::{
    AgentControlAgentRecord, AgentControlListOutput, AgentControlListRequest,
    AgentControlMessageOutput, AgentControlSendInputOutput, AgentControlSendInputRequest,
    AgentControlSpawnOutput, AgentControlSpawnRequest, AgentControlTargetRequest,
    AgentControlWaitOutput, AgentControlWaitRequest,
};
use pl_protocol::AgentStatus as PlAgentStatus;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::state::{AgentRecord, CollabInput};
use crate::{AgentRuntime, RuntimeError, agents};

const DEFAULT_WAIT_AGENT_OBSERVATION_SECS: u64 = 30;

/// 将 mai-team 的协作 agent 生命周期接入 pl-core agent-control 工具。
///
/// pl-core 负责模型可见 schema、输入解析、输出序列化、trace 与工具生命周期；
/// 本 adapter 只保留 mai-team 的产品语义，包括容器 clone、上下文 fork、store/UI
/// 双写与 project agent 生命周期动作。
#[derive(Clone)]
pub(crate) struct MaiAgentControlBackend {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
    cancellation_token: CancellationToken,
}

impl MaiAgentControlBackend {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
            cancellation_token,
        }
    }
}

/// mai-team 注入 pl-core agent-control 工具的产品权限策略。
///
/// 工具可见性和目标通信边界在 pl-core 调用 backend 之前统一检查；backend
/// 只执行已经授权的生命周期动作。
#[derive(Clone)]
pub(crate) struct MaiAgentControlPolicy {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
}

impl MaiAgentControlPolicy {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
        }
    }
}

impl fmt::Debug for MaiAgentControlPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaiAgentControlPolicy")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for MaiAgentControlBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaiAgentControlBackend")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl pl_core::AgentControlBackend for MaiAgentControlBackend {
    type Error = RuntimeError;

    async fn spawn_agent(
        &self,
        request: AgentControlSpawnRequest,
    ) -> std::result::Result<AgentControlSpawnOutput, Self::Error> {
        let role = request
            .agent_type
            .as_deref()
            .and_then(agents::agent_type_role)
            .unwrap_or_default();
        let role_profile_requested = role_profile_requested(request.agent_type.as_deref());
        let result = agents::spawn_child_agent(
            &self.runtime,
            self.agent_id,
            agents::SpawnChildAgentRequest {
                name: Some(request.task_name.clone()),
                role,
                model: request.model,
                reasoning_effort: request.reasoning_effort,
                use_role_model: role_profile_requested,
                forked_history: request.forked_messages,
                collab_input: CollabInput {
                    message: non_empty_message(request.message),
                    skill_mentions: request.skill_mentions,
                },
            },
        )
        .await?;
        Ok(AgentControlSpawnOutput {
            agent_id: result.agent.id.to_string(),
            task_name: result.agent.name,
            path: result.agent.id.to_string(),
            status: pl_agent_status(&result.agent.status),
            turn_id: result.turn_id.map(|turn_id| turn_id.to_string()),
        })
    }

    async fn send_input(
        &self,
        request: AgentControlSendInputRequest,
    ) -> std::result::Result<AgentControlSendInputOutput, Self::Error> {
        let target = parse_agent_id(&request.target)?;
        let interrupt = request.interrupt;
        let trigger_turn = request.trigger_turn || interrupt;
        let output = agents::send_input_to_agent(
            self.runtime.as_ref(),
            &self.runtime,
            agents::SendInputRequest {
                target,
                session_id: None,
                message: request.message,
                skill_mentions: request.skill_mentions,
                trigger_turn,
                interrupt,
                cancel_grace: crate::TURN_CANCEL_GRACE,
            },
        )
        .await?;
        let queued = output
            .get("queued")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let turn_id = output
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let status = self
            .runtime
            .agent(target)
            .await?
            .summary
            .read()
            .await
            .status
            .clone();
        Ok(AgentControlSendInputOutput {
            target: target.to_string(),
            status: pl_agent_status(&status),
            interrupt,
            queued,
            turn_id,
        })
    }

    async fn wait_agent(
        &self,
        request: AgentControlWaitRequest,
    ) -> std::result::Result<AgentControlWaitOutput, Self::Error> {
        let targets = self.child_agent_ids().await;
        if targets.is_empty() {
            return Ok(AgentControlWaitOutput {
                message: "no managed sub-agents to wait for".to_string(),
                timed_out: false,
            });
        }
        let output = self
            .runtime
            .wait_agents_output_with_cancel(
                targets,
                wait_timeout(request.timeout_ms),
                &self.cancellation_token,
            )
            .await?;
        let timed_out = output
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(AgentControlWaitOutput {
            message: output.to_string(),
            timed_out,
        })
    }

    async fn list_agents(
        &self,
        request: AgentControlListRequest,
    ) -> std::result::Result<AgentControlListOutput, Self::Error> {
        let path_prefix = request.path_prefix.unwrap_or_default();
        let current_parent = self.agent.summary.read().await.parent_id;
        let agents = self
            .runtime
            .list_agents()
            .await
            .into_iter()
            .filter(|summary| self.summary_visible(summary, current_parent))
            .map(compact_agent_record)
            .filter(|record| path_prefix.is_empty() || record.path.starts_with(&path_prefix))
            .collect();
        Ok(AgentControlListOutput { agents })
    }

    async fn close_agent(
        &self,
        request: AgentControlTargetRequest,
    ) -> std::result::Result<AgentControlMessageOutput, Self::Error> {
        let target = parse_agent_id(&request.target)?;
        self.runtime.close_agent(target).await?;
        Ok(AgentControlMessageOutput {
            target: target.to_string(),
            status: PlAgentStatus::Shutdown,
        })
    }

    async fn resume_agent(
        &self,
        request: AgentControlTargetRequest,
    ) -> std::result::Result<AgentControlMessageOutput, Self::Error> {
        let target = parse_agent_id(&request.target)?;
        let resumed = self.runtime.resume_agent(target).await?;
        Ok(AgentControlMessageOutput {
            target: resumed.id.to_string(),
            status: pl_agent_status(&resumed.status),
        })
    }
}

impl pl_core::AgentControlPolicy for MaiAgentControlPolicy {
    type Error = RuntimeError;

    async fn check_tool(
        &self,
        kind: pl_core::AgentControlToolKind,
    ) -> std::result::Result<(), Self::Error> {
        let visible =
            super::tool_visibility::visible_tool_names(&self.runtime.state, &self.agent, &[]).await;
        if visible.contains(kind.name()) {
            return Ok(());
        }
        Err(RuntimeError::InvalidInput(format!(
            "Tool '{}' is not available for this agent",
            kind.name()
        )))
    }

    async fn check_target(
        &self,
        kind: pl_core::AgentControlToolKind,
        target: &str,
    ) -> std::result::Result<(), Self::Error> {
        let target = parse_agent_id(target)?;
        match kind {
            pl_core::AgentControlToolKind::SendInput
            | pl_core::AgentControlToolKind::WaitAgent
            | pl_core::AgentControlToolKind::ResumeAgent => {
                if super::tool_visibility::agent_can_access_target(
                    &self.runtime.state,
                    &self.agent,
                    target,
                )
                .await
                {
                    return Ok(());
                }
                Err(RuntimeError::InvalidInput(
                    "target agent is outside this agent's communication policy".to_string(),
                ))
            }
            pl_core::AgentControlToolKind::CloseAgent => {
                if target == self.agent_id {
                    return Err(RuntimeError::InvalidInput(
                        "cannot close the current agent".to_string(),
                    ));
                }
                Ok(())
            }
            pl_core::AgentControlToolKind::SpawnAgent
            | pl_core::AgentControlToolKind::ListAgents => Ok(()),
        }
    }
}

impl MaiAgentControlBackend {
    async fn child_agent_ids(&self) -> Vec<AgentId> {
        self.runtime
            .list_agents()
            .await
            .into_iter()
            .filter(|summary| summary.parent_id == Some(self.agent_id))
            .filter(|summary| summary.status != MaiAgentStatus::Deleted)
            .map(|summary| summary.id)
            .collect()
    }

    fn summary_visible(&self, summary: &AgentSummary, current_parent: Option<AgentId>) -> bool {
        if summary.id == self.agent_id || summary.parent_id == Some(self.agent_id) {
            return true;
        }
        current_parent == Some(summary.id)
    }
}

fn role_profile_requested(agent_type: Option<&str>) -> bool {
    agent_type.is_some_and(|value| {
        matches!(
            value.trim().to_lowercase().as_str(),
            "planner" | "explorer" | "executor" | "reviewer"
        )
    })
}

fn non_empty_message(message: String) -> Option<String> {
    let trimmed = message.trim();
    (!trimmed.is_empty()).then(|| message)
}

fn wait_timeout(timeout_ms: Option<i64>) -> Duration {
    let Some(timeout_ms) = timeout_ms.and_then(|value| u64::try_from(value).ok()) else {
        return Duration::from_secs(DEFAULT_WAIT_AGENT_OBSERVATION_SECS);
    };
    Duration::from_millis(timeout_ms.max(100))
}

fn parse_agent_id(value: &str) -> crate::Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|error| RuntimeError::InvalidInput(format!("invalid agent id `{value}`: {error}")))
}

fn compact_agent_record(summary: AgentSummary) -> AgentControlAgentRecord {
    AgentControlAgentRecord {
        path: summary.id.to_string(),
        status: pl_agent_status(&summary.status),
        role: summary.role.unwrap_or(AgentRole::Executor).to_string(),
        task: summary.name,
        summary: Some(format!("{} / {}", summary.provider_name, summary.model)),
        error: summary.last_error,
    }
}

fn pl_agent_status(status: &MaiAgentStatus) -> PlAgentStatus {
    match status {
        MaiAgentStatus::Created | MaiAgentStatus::StartingContainer | MaiAgentStatus::Idle => {
            PlAgentStatus::Queued
        }
        MaiAgentStatus::RunningTurn => PlAgentStatus::Running,
        MaiAgentStatus::WaitingTool => PlAgentStatus::Waiting,
        MaiAgentStatus::Completed => PlAgentStatus::Completed,
        MaiAgentStatus::Failed => PlAgentStatus::Errored,
        MaiAgentStatus::Cancelled => PlAgentStatus::Interrupted,
        MaiAgentStatus::DeletingContainer | MaiAgentStatus::Deleted => PlAgentStatus::Shutdown,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_control_backend_delegates_tool_error_shape_to_pl_core() {
        let source = include_str!("agent_control.rs");

        assert!(
            !source.contains(&format!("{}{}", "ToolExecution", "Failed")),
            "agent-control adapter 不应在 mai-team 手动构造工具错误协议"
        );
        assert!(
            !source.contains(&format!("{}{}", "Pure", "Error")),
            "agent-control adapter 不应依赖 pl_protocol/pl_core 错误协议类型"
        );
    }
}
