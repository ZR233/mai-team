use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentLastTurn, AgentRuntimeActivity, AgentRuntimeLifecycle, AgentRuntimeState,
    AgentTurnOutcomeKind, TokenUsage,
};
use pl_core::{
    AgentActivityState, AgentLifecycleState, AgentSnapshot, AgentTurnOutcome, TurnOutcomeKind,
};
use uuid::Uuid;

/// 将 PL 字符串 ID 稳定映射为 mai wire UUID；原值为 UUID 时保持不变。
pub(crate) fn protocol_uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap_or_else(|_| Uuid::new_v5(&Uuid::NAMESPACE_OID, value.as_bytes()))
}

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
            AgentActivityState::Running => AgentRuntimeActivity::Running,
            AgentActivityState::WaitingTool => AgentRuntimeActivity::WaitingTool,
            AgentActivityState::WaitingInteraction => AgentRuntimeActivity::WaitingInteraction,
        },
        active_turn: snapshot
            .active_turn_id
            .as_ref()
            .map(|turn| protocol_uuid(turn.as_str())),
        active_session: snapshot
            .active_session_id
            .as_ref()
            .map(|session| protocol_uuid(session.as_str())),
        pending_inputs: snapshot.pending_inputs,
        last_turn: snapshot.last_turn.as_ref().map(last_turn),
        revision: snapshot.revision,
    }
}

fn last_turn(outcome: &AgentTurnOutcome) -> AgentLastTurn {
    AgentLastTurn {
        turn_id: protocol_uuid(outcome.turn_id.as_str()),
        session_id: protocol_uuid(outcome.session_id.as_str()),
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
        AgentActivityState, AgentId, AgentIdentity, AgentLifecycleState, AgentRoleId,
        AgentSnapshot, AgentTurnOutcome, SessionId, TurnId, TurnOutcomeKind,
    };

    use super::runtime_state;

    #[test]
    fn maps_complete_runtime_snapshot_without_pl_types_leaking() {
        let snapshot = AgentSnapshot {
            identity: AgentIdentity {
                id: AgentId::new("agent").unwrap(),
                parent_id: None,
                role: AgentRoleId::new("executor").unwrap(),
                depth: 0,
            },
            lifecycle: AgentLifecycleState::Active,
            activity: AgentActivityState::Queued,
            active_turn_id: None,
            active_session_id: None,
            pending_inputs: 2,
            last_turn: Some(AgentTurnOutcome {
                turn_id: TurnId::new("turn").unwrap(),
                session_id: SessionId::new("session").unwrap(),
                kind: TurnOutcomeKind::BudgetLimited,
                reason: Some("token budget".to_string()),
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
