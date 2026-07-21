use mai_protocol::SessionId;
use pl_protocol::{
    InteractionRequest, PlanLifecycleEvent, SessionEventEnvelope, SessionEventKind, SessionMessage,
    SessionPart, SessionRuntimeSnapshot, SessionStreamFrame, SessionTimelineEvent, SessionTurn,
    SessionViewSnapshot, SkillActivation,
};

use super::protocol_uuid;

/// 将 PL 内部字符串 ID 投影为 Mai 对外稳定的 UUID wire ID。
pub(crate) fn project_session_stream_frame(frame: &mut SessionStreamFrame, session_id: SessionId) {
    match frame {
        SessionStreamFrame::Snapshot { snapshot } => {
            project_session_view_snapshot(snapshot, session_id);
        }
        SessionStreamFrame::Event { event } => {
            project_session_event_envelope(event, session_id);
        }
        SessionStreamFrame::ResyncRequired { reason: _ } => {}
    }
}

pub(crate) fn project_session_view_snapshot(
    snapshot: &mut SessionViewSnapshot,
    session_id: SessionId,
) {
    let session_id = session_id.to_string();
    snapshot.session_id.clone_from(&session_id);
    if let Some(turn) = &mut snapshot.turn {
        project_turn(turn, &session_id);
    }
    for message in &mut snapshot.messages {
        project_message(message, &session_id);
    }
    for part in &mut snapshot.parts {
        project_part(part, &session_id);
    }
    for interaction in &mut snapshot.interactions {
        project_interaction(interaction, &session_id);
    }
    for agent in &mut snapshot.agents {
        agent.session_id.clone_from(&session_id);
    }
    for event in &mut snapshot.timeline_events {
        project_timeline_event(event, &session_id);
    }
    if let Some(runtime) = &mut snapshot.runtime {
        project_runtime(runtime, &session_id);
    }
    for activation in &mut snapshot.activated_skills {
        project_skill_activation(activation);
    }
    for event in &mut snapshot.plan_events {
        project_plan_event(event);
    }
}

pub(crate) fn project_session_event_envelope(
    event: &mut SessionEventEnvelope,
    session_id: SessionId,
) {
    let session_id = session_id.to_string();
    event.session_id.clone_from(&session_id);
    event.turn_id = event
        .turn_id
        .take()
        .map(|turn_id| project_turn_id(&turn_id));
    match &mut event.kind {
        SessionEventKind::TurnChanged { turn } => project_turn(turn, &session_id),
        SessionEventKind::MessageChanged { message } => project_message(message, &session_id),
        SessionEventKind::MessageRemoved { message_id: _ } => {}
        SessionEventKind::PartChanged { part } => project_part(part, &session_id),
        SessionEventKind::PartRemoved {
            message_id: _,
            part_id: _,
        } => {}
        SessionEventKind::PartDelta { delta: _ } => {}
        SessionEventKind::InteractionChanged { event } => {
            project_interaction(&mut event.interaction, &session_id);
        }
        SessionEventKind::AgentChanged { agent } => {
            agent.session_id.clone_from(&session_id);
        }
        SessionEventKind::TimelineEventAppended { event } => {
            project_timeline_event(event, &session_id);
        }
        SessionEventKind::RuntimeChanged { runtime } => project_runtime(runtime, &session_id),
        SessionEventKind::SkillActivated { activation } => {
            project_skill_activation(activation);
        }
        SessionEventKind::PlanChanged { event } => project_plan_event(event),
        SessionEventKind::ContextCompacted { compaction: _ } => {}
        SessionEventKind::ErrorOccurred {
            message: _,
            severity: _,
        } => {}
    }
}

fn project_turn(turn: &mut SessionTurn, session_id: &str) {
    turn.session_id = session_id.to_string();
    turn.turn_id = project_turn_id(&turn.turn_id);
}

fn project_message(message: &mut SessionMessage, session_id: &str) {
    message.session_id = session_id.to_string();
    message.turn_id = project_turn_id(&message.turn_id);
}

fn project_part(part: &mut SessionPart, session_id: &str) {
    part.session_id = session_id.to_string();
    part.turn_id = project_turn_id(&part.turn_id);
}

fn project_interaction(interaction: &mut InteractionRequest, session_id: &str) {
    interaction.scope.session_id = session_id.to_string();
    interaction.scope.turn_id = project_turn_id(&interaction.scope.turn_id);
}

fn project_timeline_event(event: &mut SessionTimelineEvent, session_id: &str) {
    event.session_id = session_id.to_string();
}

fn project_runtime(runtime: &mut SessionRuntimeSnapshot, session_id: &str) {
    runtime.session_id = session_id.to_string();
}

fn project_skill_activation(activation: &mut SkillActivation) {
    activation.turn_id = project_turn_id(&activation.turn_id);
}

fn project_plan_event(event: &mut PlanLifecycleEvent) {
    event.turn_id = event
        .turn_id
        .take()
        .map(|turn_id| project_turn_id(&turn_id));
}

fn project_turn_id(turn_id: &str) -> String {
    protocol_uuid(turn_id).to_string()
}

#[cfg(test)]
mod tests {
    use mai_protocol::SessionId;
    use pl_protocol::{
        SessionEventEnvelope, SessionEventKind, SessionEventPosition, SessionStreamFrame,
        SessionTurn, SessionTurnStatus, SessionViewSnapshot,
    };
    use pretty_assertions::assert_eq;

    use super::{project_session_event_envelope, project_session_stream_frame, project_turn_id};

    #[test]
    fn snapshot_uses_the_requested_mai_session_and_projected_turn_id() {
        let session_id = SessionId::new_v4();
        let mut snapshot = SessionViewSnapshot::empty("session_internal");
        snapshot.turn = Some(SessionTurn {
            turn_id: "turn_internal".to_string(),
            session_id: "session_internal".to_string(),
            status: SessionTurnStatus::RunningTool,
            reason: None,
            updated_at: 1,
        });
        let mut frame = SessionStreamFrame::Snapshot {
            snapshot: Box::new(snapshot),
        };

        project_session_stream_frame(&mut frame, session_id);

        let SessionStreamFrame::Snapshot { snapshot } = frame else {
            panic!("expected snapshot");
        };
        assert_eq!(snapshot.session_id, session_id.to_string());
        assert_eq!(
            snapshot.turn,
            Some(SessionTurn {
                turn_id: project_turn_id("turn_internal"),
                session_id: session_id.to_string(),
                status: SessionTurnStatus::RunningTool,
                reason: None,
                updated_at: 1,
            })
        );
    }

    #[test]
    fn event_envelope_and_payload_share_the_mai_wire_ids() {
        let session_id = SessionId::new_v4();
        let projected_turn_id = project_turn_id("turn_internal");
        let mut event = SessionEventEnvelope {
            event_id: "event-1".to_string(),
            session_id: "session_internal".to_string(),
            source_agent_id: None,
            turn_id: Some("turn_internal".to_string()),
            emitted_at: 1,
            position: SessionEventPosition::Durable { sequence: 1 },
            kind: SessionEventKind::TurnChanged {
                turn: SessionTurn {
                    turn_id: "turn_internal".to_string(),
                    session_id: "session_internal".to_string(),
                    status: SessionTurnStatus::Streaming,
                    reason: None,
                    updated_at: 1,
                },
            },
        };

        project_session_event_envelope(&mut event, session_id);

        assert_eq!(event.session_id, session_id.to_string());
        assert_eq!(event.turn_id, Some(projected_turn_id.clone()));
        let SessionEventKind::TurnChanged { turn } = event.kind else {
            panic!("expected turn event");
        };
        assert_eq!(turn.session_id, session_id.to_string());
        assert_eq!(turn.turn_id, projected_turn_id);
    }
}
