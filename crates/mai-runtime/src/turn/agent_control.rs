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
/// 双写、通信边界与 project maintainer 策略。
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

impl fmt::Debug for MaiAgentControlBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaiAgentControlBackend")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl pl_core::AgentControlBackend for MaiAgentControlBackend {
    async fn spawn_agent(
        &self,
        request: AgentControlSpawnRequest,
    ) -> pl_core::Result<AgentControlSpawnOutput> {
        self.ensure_tool_visible(pl_core::TOOL_SPAWN_AGENT).await?;
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
        .await
        .map_err(tool_error(pl_core::TOOL_SPAWN_AGENT))?;
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
    ) -> pl_core::Result<AgentControlSendInputOutput> {
        let target = self
            .accessible_target(pl_core::TOOL_SEND_INPUT, &request.target)
            .await?;
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
        .await
        .map_err(tool_error(pl_core::TOOL_SEND_INPUT))?;
        let queued = output
            .get("queued")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let turn_id = output
            .get("turn_id")
            .or_else(|| output.get("turnId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let status = self
            .runtime
            .agent(target)
            .await
            .map_err(tool_error(pl_core::TOOL_SEND_INPUT))?
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
    ) -> pl_core::Result<AgentControlWaitOutput> {
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
            .await
            .map_err(tool_error(pl_core::TOOL_WAIT_AGENT))?;
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
    ) -> pl_core::Result<AgentControlListOutput> {
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
    ) -> pl_core::Result<AgentControlMessageOutput> {
        self.ensure_tool_visible(pl_core::TOOL_CLOSE_AGENT).await?;
        let target = parse_agent_id(pl_core::TOOL_CLOSE_AGENT, &request.target)?;
        if target == self.agent_id {
            return Err(pl_core::PureError::ToolExecutionFailed {
                tool: pl_core::TOOL_CLOSE_AGENT.to_string(),
                error: "cannot close the current agent".to_string(),
            });
        }
        self.runtime
            .close_agent(target)
            .await
            .map_err(tool_error(pl_core::TOOL_CLOSE_AGENT))?;
        Ok(AgentControlMessageOutput {
            target: target.to_string(),
            status: PlAgentStatus::Shutdown,
        })
    }

    async fn resume_agent(
        &self,
        request: AgentControlTargetRequest,
    ) -> pl_core::Result<AgentControlMessageOutput> {
        let target = self
            .accessible_target(pl_core::TOOL_RESUME_AGENT, &request.target)
            .await?;
        let resumed = self
            .runtime
            .resume_agent(target)
            .await
            .map_err(tool_error(pl_core::TOOL_RESUME_AGENT))?;
        Ok(AgentControlMessageOutput {
            target: resumed.id.to_string(),
            status: pl_agent_status(&resumed.status),
        })
    }
}

impl MaiAgentControlBackend {
    async fn ensure_tool_visible(&self, tool: &'static str) -> pl_core::Result<()> {
        let visible =
            super::tool_visibility::visible_tool_names(&self.runtime.state, &self.agent, &[]).await;
        if visible.contains(tool) {
            return Ok(());
        }
        Err(pl_core::PureError::ToolExecutionFailed {
            tool: tool.to_string(),
            error: format!("Tool '{tool}' is not available for this agent"),
        })
    }

    async fn accessible_target(
        &self,
        tool: &'static str,
        target: &str,
    ) -> pl_core::Result<AgentId> {
        let target = parse_agent_id(tool, target)?;
        if super::tool_visibility::agent_can_access_target(&self.runtime.state, &self.agent, target)
            .await
        {
            return Ok(target);
        }
        Err(pl_core::PureError::ToolExecutionFailed {
            tool: tool.to_string(),
            error: "target agent is outside this agent's communication policy".to_string(),
        })
    }

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

fn parse_agent_id(tool: &'static str, value: &str) -> pl_core::Result<AgentId> {
    Uuid::parse_str(value).map_err(|error| pl_core::PureError::ToolExecutionFailed {
        tool: tool.to_string(),
        error: format!("invalid agent id `{value}`: {error}"),
    })
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

fn tool_error(
    tool: &'static str,
) -> impl FnOnce(RuntimeError) -> pl_core::PureError + Send + 'static {
    move |error| pl_core::PureError::ToolExecutionFailed {
        tool: tool.to_string(),
        error: error.to_string(),
    }
}
