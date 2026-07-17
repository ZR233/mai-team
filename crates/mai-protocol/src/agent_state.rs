use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{SessionId, TokenUsage, TurnId};

/// 产品外部资源（容器、workspace、MCP）的生命周期；与模型 turn 执行正交。
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Default,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AgentResourceState {
    #[default]
    Provisioning,
    Ready,
    Deleting,
    Failed,
    Deleted,
}

/// PL runtime agent 生命周期的产品 wire 投影。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentRuntimeLifecycle {
    #[default]
    Active,
    Closing,
    Closed,
    Faulted,
}

/// PL runtime 活动状态的产品 wire 投影。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentRuntimeActivity {
    #[default]
    Idle,
    Queued,
    Running,
    WaitingTool,
    WaitingInteraction,
}

/// 最近一次 turn 的稳定结果类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnOutcomeKind {
    Completed,
    Cancelled,
    Failed,
    BudgetLimited,
}

/// 最近一次 turn 的完整 wire snapshot。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLastTurn {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub outcome: AgentTurnOutcomeKind,
    pub reason: Option<String>,
    pub usage: TokenUsage,
    pub finished_at: DateTime<Utc>,
}

/// 不依赖 pl-core 的 agent runtime wire DTO。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AgentRuntimeState {
    pub lifecycle: AgentRuntimeLifecycle,
    pub activity: AgentRuntimeActivity,
    pub active_turn: Option<TurnId>,
    pub active_session: Option<SessionId>,
    pub pending_inputs: usize,
    pub last_turn: Option<AgentLastTurn>,
    pub revision: u64,
}

/// 产品资源和框架执行状态的正交组合。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AgentState {
    pub resource: AgentResourceState,
    #[serde(default)]
    pub resource_error: Option<String>,
    pub runtime: AgentRuntimeState,
}

impl AgentState {
    /// 只有资源已就绪且 PL actor 完全空闲时才允许修改模型等执行配置。
    pub fn can_reconfigure(&self) -> bool {
        self.resource == AgentResourceState::Ready
            && self.runtime.lifecycle == AgentRuntimeLifecycle::Active
            && self.runtime.activity == AgentRuntimeActivity::Idle
            && self.runtime.pending_inputs == 0
    }

    /// 返回当前由 PL runtime 管理的活动 turn。
    pub fn active_turn(&self) -> Option<TurnId> {
        self.runtime.active_turn
    }
}
