use chrono::Utc;
use mai_protocol::{
    AgentMessage, AgentResourceState, AgentState, AgentSummary, MessageRole, TokenUsage,
};
use pretty_assertions::assert_eq;

use crate::records::{AgentRuntimeEventRecord, AgentRuntimeTraceRecord};
use crate::{
    AgentRuntimeCommitDocument, AgentRuntimeCommitOutcome, MaiStore, StoredAgentPendingInput,
    StoredAgentRuntime, StoredAgentRuntimeEvent, StoredAgentRuntimeSession,
    StoredAgentRuntimeState, StoredAgentRuntimeTrace, StoredAgentTurn, StoredTokenUsage,
};
use toasty::stmt::{List, Query};

#[tokio::test]
async fn runtime_commit_is_atomic_and_revision_checked() {
    let temp = tempfile::tempdir().unwrap();
    let store = MaiStore::open_with_config_path(
        temp.path().join("mai.sqlite3"),
        temp.path().join("config.toml"),
    )
    .await
    .unwrap();
    let first = document(None, 1, "queued");
    assert_eq!(
        store.commit_agent_runtime(first.clone()).await.unwrap(),
        AgentRuntimeCommitOutcome::Applied
    );

    let stale = document(None, 2, "running");
    assert_eq!(
        store.commit_agent_runtime(stale).await.unwrap(),
        AgentRuntimeCommitOutcome::RevisionConflict {
            actual_revision: Some(1)
        }
    );
    assert_eq!(
        store.load_agent_runtimes().await.unwrap(),
        vec![first.runtime]
    );
    assert_eq!(runtime_projection_counts(&store).await, (1, 1));
}

#[tokio::test]
async fn runtime_commit_round_trips_queue_session_and_canonical_history() {
    let temp = tempfile::tempdir().unwrap();
    let store = MaiStore::open_with_config_path(
        temp.path().join("mai.sqlite3"),
        temp.path().join("config.toml"),
    )
    .await
    .unwrap();
    store
        .commit_agent_runtime(document(None, 1, "queued"))
        .await
        .unwrap();
    let second = document(Some(1), 2, "running");
    store.commit_agent_runtime(second.clone()).await.unwrap();

    assert_eq!(
        store.load_agent_runtimes().await.unwrap(),
        vec![second.runtime]
    );
    assert_eq!(runtime_projection_counts(&store).await, (2, 2));
}

async fn runtime_projection_counts(store: &MaiStore) -> (usize, usize) {
    let mut db = store.db.clone();
    let events = Query::<List<AgentRuntimeEventRecord>>::all()
        .exec(&mut db)
        .await
        .unwrap();
    let traces = Query::<List<AgentRuntimeTraceRecord>>::all()
        .exec(&mut db)
        .await
        .unwrap();
    (events.len(), traces.len())
}

#[tokio::test]
async fn deleting_product_agent_removes_mapped_framework_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = MaiStore::open_with_config_path(
        temp.path().join("mai.sqlite3"),
        temp.path().join("config.toml"),
    )
    .await
    .unwrap();
    let product_agent_id = uuid::Uuid::new_v4();
    let now = Utc::now();
    store
        .save_agent_with_runtime_id(
            &AgentSummary {
                id: product_agent_id,
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                name: "agent".to_string(),
                state: AgentState {
                    resource: AgentResourceState::Ready,
                    ..AgentState::default()
                },
                container_id: None,
                docker_image: "ubuntu:latest".to_string(),
                provider_id: "openai".to_string(),
                provider_name: "OpenAI".to_string(),
                model: "gpt-5".to_string(),
                reasoning_effort: None,
                created_at: now,
                updated_at: now,
                token_usage: TokenUsage::default(),
            },
            None,
            "agent-1",
        )
        .await
        .unwrap();
    store
        .commit_agent_runtime(document(None, 1, "queued"))
        .await
        .unwrap();

    store.delete_agent(product_agent_id).await.unwrap();

    assert_eq!(store.load_agent_runtimes().await.unwrap(), Vec::new());
    assert!(
        store
            .load_runtime_snapshot(10)
            .await
            .unwrap()
            .agents
            .is_empty()
    );
}

fn document(
    expected_revision: Option<u64>,
    revision: u64,
    activity: &str,
) -> AgentRuntimeCommitDocument {
    AgentRuntimeCommitDocument {
        expected_revision,
        runtime: StoredAgentRuntime {
            state: StoredAgentRuntimeState {
                agent_id: "agent-1".to_string(),
                parent_id: None,
                role: "maintainer".to_string(),
                depth: 0,
                lifecycle: "active".to_string(),
                activity: activity.to_string(),
                active_turn_id: None,
                active_session_id: None,
                pending_inputs: 1,
                last_turn: None,
                revision,
                event_sequence: revision,
                updated_at: 1_700_000_000,
            },
            sessions: vec![StoredAgentRuntimeSession {
                session_id: "session-1".to_string(),
                title: Some("会话".to_string()),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                history_items: vec![serde_json::json!({
                    "role": "user",
                    "content": "hello"
                })],
                messages: vec![AgentMessage {
                    role: MessageRole::User,
                    content: "hello".to_string(),
                    created_at: Utc::now(),
                }],
                usage: StoredTokenUsage::default(),
                last_context_tokens: Some(42),
                trace_sequence: 7,
            }],
            pending_inputs: vec![StoredAgentPendingInput {
                turn_id: "turn-1".to_string(),
                session_id: "session-1".to_string(),
                message: "next".to_string(),
                metadata: serde_json::json!({ "skills": ["rust"] }),
                queued_at: 1_700_000_000,
            }],
        },
        turns: vec![StoredAgentTurn {
            turn_id: "turn-1".to_string(),
            session_id: "session-1".to_string(),
            status: activity.to_string(),
            error: None,
            usage: StoredTokenUsage::default(),
            started_at: None,
            finished_at: None,
        }],
        events: vec![StoredAgentRuntimeEvent {
            sequence: revision,
            created_at: 1_700_000_000,
            payload: serde_json::json!({ "kind": activity }),
        }],
        traces: vec![StoredAgentRuntimeTrace {
            sequence: revision,
            payload: serde_json::json!({ "trace": activity }),
        }],
    }
}
