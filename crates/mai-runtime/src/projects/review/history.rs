//! 用 PL 的完整耐久 effect 区分已执行 Turn 与只受理、从未启动的输入。

use super::runs::{ReviewRunSnapshot, runtime_usage_snapshot};
use crate::{Result, thread_projection::project_turn_history_from_effects};
use mai_protocol::{AgentId, ProjectReviewHistoryStatus};
use pl_core::{
    persistence::{SqliteSessionStore, ThreadEffectQuery},
    thread::{
        ThreadEffectBatch,
        input::{InputChange, InputDelivery, InputState},
    },
};

pub(crate) async fn snapshot(
    store: &SqliteSessionStore,
    agent: AgentId,
    input: Option<&str>,
) -> Result<ReviewRunSnapshot> {
    let id = agent.to_string();
    let mut effects = Vec::new();
    let mut before_sequence = None;
    loop {
        let page = store
            .query_thread_effects(
                &id,
                ThreadEffectQuery {
                    before_sequence,
                    limit: 200,
                },
            )
            .await?;
        let reached_input = input.is_some_and(|id| page.effects.iter().any(|effect| effect.inputs.iter().any(|change| matches!(change, InputChange::Accepted(record) if record.input.id == id))));
        effects.extend(page.effects);
        // 输入受理之前不可能执行它；覆盖到受理 effect 已经包含该输入的全部事实。
        if reached_input {
            break;
        }
        match page.next_before_sequence {
            Some(next) => before_sequence = Some(next),
            None => break,
        }
    }
    let history = project_turn_history_from_effects(&id, &effects)
        .map_err(|error| crate::RuntimeError::InvalidInput(error.to_string()))?
        .into_iter()
        .find(|history| input.is_none_or(|id| history.turn.input_id.as_deref() == Some(id)));
    let history_status = if input.is_some_and(|id| accepted_without_turn(&effects, id)) {
        ProjectReviewHistoryStatus::NotStarted
    } else {
        ProjectReviewHistoryStatus::Available
    };
    let usage = store
        .read_thread_checkpoint(&id)
        .await?
        .map(|checkpoint| {
            runtime_usage_snapshot(
                &checkpoint.state.usage_summary,
                mai_protocol::now().timestamp(),
            )
        })
        .unwrap_or_default();
    Ok(ReviewRunSnapshot {
        usage,
        history,
        history_status,
    })
}

fn accepted_without_turn(effects: &[ThreadEffectBatch], input: &str) -> bool {
    let mut accepted = false;
    for effect in effects {
        if effect
            .turn
            .as_ref()
            .is_some_and(|turn| turn.input_id.as_deref() == Some(input))
        {
            return false;
        }
        for change in effect.inputs.iter() {
            match change {
                InputChange::Accepted(record) if record.input.id == input => {
                    if record.delivery != InputDelivery::NextTurn
                        || matches!(record.state, InputState::Consumed { .. })
                    {
                        return false;
                    }
                    accepted = true;
                }
                InputChange::Transition {
                    id,
                    state: InputState::Consumed { .. },
                    ..
                } if id == input => return false,
                InputChange::Accepted(_) | InputChange::Transition { .. } => {}
            }
        }
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use pl_core::{
        context::OpaquePayload,
        persistence::SqliteSessionOptions,
        thread::{
            TurnRecord, TurnState,
            input::{InputRecord, ThreadInput},
        },
    };
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    #[tokio::test]
    async fn native_history_distinguishes_queued_input_from_missing_or_started_turn() {
        let temp = tempfile::tempdir().unwrap();
        let id = AgentId::new_v4();
        let store = SqliteSessionStore::open(SqliteSessionOptions {
            path: temp.path().join("history.sqlite"),
        })
        .await
        .unwrap();
        let input = InputRecord {
            accepted_sequence: 1,
            delivery: InputDelivery::NextTurn,
            input: ThreadInput {
                id: "queued".into(),
                payload: OpaquePayload::text("request"),
                context: vec![],
            },
            ordinal: 1,
            revision: 1,
            state: InputState::Pending,
        };
        let effect = ThreadEffectBatch {
            thread_id: id.to_string(),
            sequence: 1,
            inputs: Arc::from([InputChange::Accepted(input)]),
            ..Default::default()
        };
        store
            .register_resource(
                &id.to_string(),
                "thread-commit.00000000000000000001",
                effect.encode().unwrap(),
            )
            .unwrap();
        store.flush().await.unwrap();
        assert_eq!(
            snapshot(&store, id, Some("queued"))
                .await
                .unwrap()
                .history_status,
            ProjectReviewHistoryStatus::NotStarted
        );
        assert_eq!(
            snapshot(&store, id, Some("missing"))
                .await
                .unwrap()
                .history_status,
            ProjectReviewHistoryStatus::Available
        );
        let effect = ThreadEffectBatch {
            thread_id: id.to_string(),
            sequence: 2,
            turn: Some(TurnRecord {
                turn_id: "input:queued:1".into(),
                input_id: Some("queued".into()),
                state: TurnState::Running,
                model_steps: 0,
                elapsed_ms: None,
            }),
            ..Default::default()
        };
        store
            .register_resource(
                &id.to_string(),
                "thread-commit.00000000000000000002",
                effect.encode().unwrap(),
            )
            .unwrap();
        store.flush().await.unwrap();
        let started = snapshot(&store, id, Some("queued")).await.unwrap();
        assert_eq!(
            (started.history_status, started.history),
            (ProjectReviewHistoryStatus::Available, None)
        );
        store.shutdown().await.unwrap();
    }
}
