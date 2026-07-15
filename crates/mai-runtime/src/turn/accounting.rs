use mai_protocol::{AgentId, ServiceEventKind, SessionId, TokenUsage, now};
use mai_store::ConfigStore;

use crate::events::RuntimeEvents;
use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

pub(crate) async fn record_model_usage(
    store: &ConfigStore,
    events: &RuntimeEvents,
    agent: &AgentRecord,
    agent_id: AgentId,
    session_id: SessionId,
    usage: &TokenUsage,
) -> Result<()> {
    let summary = {
        let mut summary = agent.summary.write().await;
        summary.token_usage.add(usage);
        summary.updated_at = now();
        summary.clone()
    };
    let session_summary = {
        let mut sessions = agent.sessions.lock().await;
        let session = sessions
            .iter_mut()
            .find(|session| session.summary.id == session_id)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })?;
        session.summary.token_usage.add(usage);
        session.summary.updated_at = summary.updated_at;
        session.summary.clone()
    };
    store
        .save_agent(&summary, agent.system_prompt.as_deref())
        .await?;
    store.save_agent_session(agent_id, &session_summary).await?;
    events
        .publish(ServiceEventKind::AgentUpdated { agent: summary })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use mai_protocol::{
        AgentSessionSummary, AgentStatus, AgentSummary, ServiceEventKind, SessionId, TokenUsage,
        now,
    };
    use mai_store::ConfigStore;
    use pl_core::AgentInputQueue;
    use tokio::sync::{Mutex, RwLock};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::events::RuntimeEvents;
    use crate::state::{AgentRecord, AgentSessionRecord, TurnControl, TurnControlSlot};
    use crate::turn::control::TurnTaskHandle;

    #[tokio::test]
    async fn record_model_usage_updates_agent_and_selected_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            ConfigStore::open_with_config_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
            )
            .await
            .expect("open store"),
        );
        let agent_id = Uuid::new_v4();
        let first_session_id: SessionId = Uuid::new_v4();
        let second_session_id: SessionId = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let created_at = now();
        let summary = AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            project_id: None,
            role: None,
            name: "agent-test".to_string(),
            status: AgentStatus::RunningTurn,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.2".to_string(),
            reasoning_effort: None,
            created_at,
            updated_at: created_at,
            current_turn: Some(turn_id),
            last_error: None,
            token_usage: TokenUsage {
                input_tokens: 1,
                cached_input_tokens: 0,
                output_tokens: 2,
                reasoning_output_tokens: 0,
                total_tokens: 3,
            },
        };
        let first_session = session(first_session_id, created_at, TokenUsage::default());
        let second_session = session(
            second_session_id,
            created_at,
            TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 4,
                output_tokens: 5,
                reasoning_output_tokens: 1,
                total_tokens: 15,
            },
        );
        let agent = Arc::new(AgentRecord {
            summary: RwLock::new(summary.clone()),
            sessions: Mutex::new(vec![
                AgentSessionRecord {
                    summary: first_session.clone(),
                    messages: Vec::new(),
                    last_context_tokens: None,
                    last_turn_response: None,
                },
                AgentSessionRecord {
                    summary: second_session.clone(),
                    messages: Vec::new(),
                    last_context_tokens: None,
                    last_turn_response: None,
                },
            ]),
            container: RwLock::new(None),
            mcp: RwLock::new(None),
            system_prompt: None,
            turn_lock: Mutex::new(()),
            cancel_requested: AtomicBool::new(false),
            active_turn: TurnControlSlot::with_active(TurnControl::new(
                turn_id,
                second_session_id,
                TurnTaskHandle::from_external_token(CancellationToken::new()),
            )),
            pending_inputs: Mutex::new(AgentInputQueue::new()),
        });
        store.save_agent(&summary, None).await.expect("save agent");
        store
            .save_agent_session(agent_id, &first_session)
            .await
            .expect("save first session");
        store
            .save_agent_session(agent_id, &second_session)
            .await
            .expect("save second session");
        let events = RuntimeEvents::new(Arc::clone(&store), 0, Vec::new());
        let usage = TokenUsage {
            input_tokens: 20,
            cached_input_tokens: 8,
            output_tokens: 7,
            reasoning_output_tokens: 3,
            total_tokens: 27,
        };

        super::record_model_usage(
            store.as_ref(),
            &events,
            &agent,
            agent_id,
            second_session_id,
            &usage,
        )
        .await
        .expect("record usage");

        let summary = agent.summary.read().await.clone();
        assert_eq!(
            summary.token_usage,
            TokenUsage {
                input_tokens: 21,
                cached_input_tokens: 8,
                output_tokens: 9,
                reasoning_output_tokens: 3,
                total_tokens: 30,
            }
        );
        let sessions = agent.sessions.lock().await;
        assert_eq!(sessions[0].summary.token_usage, TokenUsage::default());
        assert_eq!(
            sessions[1].summary.token_usage,
            TokenUsage {
                input_tokens: 30,
                cached_input_tokens: 12,
                output_tokens: 12,
                reasoning_output_tokens: 4,
                total_tokens: 42,
            }
        );
        drop(sessions);

        let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(snapshot.agents[0].summary.token_usage, summary.token_usage);
        let persisted_sessions = snapshot.agents[0]
            .sessions
            .iter()
            .map(|session| (session.summary.id, session.summary.token_usage.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            persisted_sessions.get(&first_session_id),
            Some(&TokenUsage::default())
        );
        assert_eq!(
            persisted_sessions.get(&second_session_id),
            Some(&TokenUsage {
                input_tokens: 30,
                cached_input_tokens: 12,
                output_tokens: 12,
                reasoning_output_tokens: 4,
                total_tokens: 42,
            })
        );
        assert!(
            snapshot
                .recent_events
                .iter()
                .any(|event| matches!(event.kind, ServiceEventKind::AgentUpdated { .. }))
        );
    }

    fn session(
        id: SessionId,
        timestamp: chrono::DateTime<chrono::Utc>,
        token_usage: TokenUsage,
    ) -> AgentSessionSummary {
        AgentSessionSummary {
            id,
            title: format!("Chat {}", id.to_string().chars().next().unwrap_or('1')),
            created_at: timestamp,
            updated_at: timestamp,
            message_count: 0,
            token_usage,
        }
    }
}
