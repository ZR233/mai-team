use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentLastTurn, AgentRuntimeActivity, AgentRuntimeLifecycle, AgentRuntimeState,
    AgentTurnOutcomeKind, TokenUsage,
};
use pl_core::{
    ActiveKind, AgentActivityState, AgentLifecycleState, AgentSnapshot, AgentTurnOutcome,
    TurnOutcomeKind,
};
/// PL snapshot 到 mai-protocol wire DTO 的唯一映射入口。
pub(crate) fn runtime_state(snapshot: &AgentSnapshot) -> AgentRuntimeState {
    AgentRuntimeState {
        lifecycle: match snapshot.lifecycle {
            AgentLifecycleState::Active => AgentRuntimeLifecycle::Active,
            AgentLifecycleState::Closing => AgentRuntimeLifecycle::Closing,
            AgentLifecycleState::Closed => AgentRuntimeLifecycle::Closed,
            AgentLifecycleState::Faulted => AgentRuntimeLifecycle::Faulted,
        },
        activity: match snapshot.activity {
            AgentActivityState::Idle => AgentRuntimeActivity::Idle,
            AgentActivityState::Queued => AgentRuntimeActivity::Queued,
            AgentActivityState::Active(ActiveKind::Running) => AgentRuntimeActivity::Running,
            AgentActivityState::Active(ActiveKind::WaitingTool) => {
                AgentRuntimeActivity::WaitingTool
            }
            AgentActivityState::Active(ActiveKind::WaitingInteraction) => {
                AgentRuntimeActivity::WaitingInteraction
            }
            AgentActivityState::Cancelling => AgentRuntimeActivity::Cancelling,
        },
        active_turn: snapshot.active_turn_id.as_ref().map(ToString::to_string),
        pending_inputs: snapshot.pending_inputs,
        last_turn: snapshot.last_turn.as_ref().map(last_turn),
        revision: snapshot.revision,
    }
}

fn last_turn(outcome: &AgentTurnOutcome) -> AgentLastTurn {
    AgentLastTurn {
        turn_id: outcome.turn_id.to_string(),
        thread_id: outcome.thread_id.to_string(),
        outcome: match outcome.kind {
            TurnOutcomeKind::Completed => AgentTurnOutcomeKind::Completed,
            TurnOutcomeKind::Cancelled => AgentTurnOutcomeKind::Cancelled,
            TurnOutcomeKind::Failed => AgentTurnOutcomeKind::Failed,
            TurnOutcomeKind::BudgetLimited => AgentTurnOutcomeKind::BudgetLimited,
        },
        reason: outcome.reason.clone(),
        usage: TokenUsage {
            input_tokens: outcome.usage.prompt_tokens,
            cached_input_tokens: outcome.usage.cached_prompt_tokens,
            output_tokens: outcome.usage.completion_tokens,
            reasoning_output_tokens: outcome.usage.reasoning_tokens,
            total_tokens: outcome.usage.total_tokens,
        },
        finished_at: DateTime::from_timestamp(outcome.finished_at, 0).unwrap_or_else(Utc::now),
    }
}

#[cfg(test)]
mod tests {
    use mai_protocol::{AgentRuntimeActivity, AgentRuntimeLifecycle, AgentTurnOutcomeKind};
    use pl_core::{
        AgentActivityState, AgentIdentity, AgentLifecycleState, AgentRoleId, AgentSnapshot,
        AgentTurnOutcome, ThreadId, TurnId, TurnOutcomeKind,
    };

    use super::runtime_state;

    #[test]
    fn maps_complete_runtime_snapshot_without_pl_types_leaking() {
        let snapshot = AgentSnapshot {
            identity: AgentIdentity {
                id: ThreadId::new("agent").unwrap(),
                parent_id: None,
                role: AgentRoleId::new("executor").unwrap(),
                depth: 0,
            },
            lifecycle: AgentLifecycleState::Active,
            activity: AgentActivityState::Queued,
            active_turn_id: None,
            pending_inputs: 2,
            progress: None,
            last_turn: Some(AgentTurnOutcome {
                turn_id: TurnId::new("turn").unwrap(),
                thread_id: ThreadId::new("thread").unwrap(),
                kind: TurnOutcomeKind::BudgetLimited,
                reason: Some("token budget".to_string()),
                failure: None,
                budget_limit: None,
                rollover_compacted: false,
                rollover_compaction_error: None,
                usage: pl_model::TokenUsage::default(),
                finished_at: 1,
            }),
            revision: 7,
            event_sequence: 8,
            updated_at: 1,
        };

        let mapped = runtime_state(&snapshot);

        assert_eq!(mapped.lifecycle, AgentRuntimeLifecycle::Active);
        assert_eq!(mapped.activity, AgentRuntimeActivity::Queued);
        assert_eq!(mapped.pending_inputs, 2);
        assert_eq!(
            mapped.last_turn.unwrap().outcome,
            AgentTurnOutcomeKind::BudgetLimited
        );
        assert_eq!(mapped.revision, 7);
    }
}
