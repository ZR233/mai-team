use mai_protocol::{AgentStatus, TurnStatus};
use pl_core::{TurnAbortReason, TurnResultStatus};

use crate::{RuntimeError, turn::completion::TurnResult};

/// `pl-core` turn 结果在 mai 产品边界上的终态投影。
pub(crate) struct CoreTurnOutcome {
    turn_status: TurnStatus,
    agent_status: AgentStatus,
    final_text: Option<String>,
    error: Option<String>,
    return_error: Option<RuntimeError>,
}

impl CoreTurnOutcome {
    pub(crate) fn from_result(result: &pl_core::TurnResult) -> Self {
        let final_text = (!result.content.trim().is_empty()).then(|| result.content.clone());
        let (turn_status, agent_status, return_error) = match result.status {
            TurnResultStatus::Completed => (TurnStatus::Completed, AgentStatus::Completed, None),
            TurnResultStatus::Aborted
                if result.abort_reason == Some(TurnAbortReason::Interrupted) =>
            {
                (
                    TurnStatus::Cancelled,
                    AgentStatus::Cancelled,
                    Some(RuntimeError::TurnCancelled),
                )
            }
            TurnResultStatus::Aborted | TurnResultStatus::Errored => {
                let message = result
                    .error
                    .clone()
                    .unwrap_or_else(|| "pl-core turn failed".to_string());
                (
                    TurnStatus::Failed,
                    AgentStatus::Failed,
                    Some(RuntimeError::InvalidInput(message)),
                )
            }
        };
        Self {
            turn_status,
            agent_status,
            final_text,
            error: result.error.clone(),
            return_error,
        }
    }

    pub(crate) fn final_text(&self) -> Option<&str> {
        self.final_text.as_deref()
    }

    pub(crate) fn into_completion(self, turn_id: mai_protocol::TurnId) -> TurnResult {
        TurnResult {
            turn_id,
            status: self.turn_status,
            agent_status: self.agent_status,
            final_text: self.final_text,
            error: self.error,
        }
    }

    pub(crate) fn take_return_error(&mut self) -> Option<RuntimeError> {
        self.return_error.take()
    }
}

/// turn 编排阶段提前失败时的完成事件投影。
pub(crate) struct TurnFailure {
    pub(crate) turn_status: TurnStatus,
    pub(crate) agent_status: AgentStatus,
    pub(crate) error_message: Option<String>,
    pub(crate) should_publish_error: bool,
}

impl TurnFailure {
    pub(crate) fn from_error(error: RuntimeError) -> Self {
        match error {
            RuntimeError::TurnCancelled => Self {
                turn_status: TurnStatus::Cancelled,
                agent_status: AgentStatus::Cancelled,
                error_message: None,
                should_publish_error: false,
            },
            error => Self {
                turn_status: TurnStatus::Failed,
                agent_status: AgentStatus::Failed,
                error_message: Some(error.to_string()),
                should_publish_error: true,
            },
        }
    }
}
