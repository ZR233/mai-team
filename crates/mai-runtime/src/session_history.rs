//! mai 的会话存储句柄目录；Thread effect 的编解码和查询由 pl-core 负责。

use std::{collections::HashMap, path::PathBuf};

use mai_protocol::{
    AgentId, ReviewInferenceBilling, ReviewInferenceBillingPage, ReviewInferenceStatus, TurnId,
};
use pl_core::persistence::{
    SessionStoreError, SqliteSessionOptions, SqliteSessionStore, ThreadEffectPage,
    ThreadEffectQuery,
};
use pl_core::thread::AttemptOutcome;
use tokio::sync::Mutex;

/// 将产品 Agent 身份映射到一个长期持有的 pl-core 会话存储句柄。
///
/// 一个句柄同时服务 Thread 的 ColdStore 写入和历史查询，避免为同一数据库重复获取独占租约。
#[derive(Debug)]
pub(crate) struct SessionHistory {
    root: PathBuf,
    sessions: Mutex<HashMap<AgentId, SqliteSessionStore>>,
}

impl SessionHistory {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 打开或复用某个 Agent 的 pl-core 会话存储。AgentId 是 UUID，不进入用户可控路径。
    pub(crate) async fn open(
        &self,
        agent_id: AgentId,
    ) -> Result<SqliteSessionStore, SessionStoreError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&agent_id) {
            return Ok(session.clone());
        }
        let session = SqliteSessionStore::open(SqliteSessionOptions {
            path: self.root.join(agent_id.to_string()).join("history.sqlite"),
        })
        .await?;
        sessions.insert(agent_id, session.clone());
        Ok(session)
    }

    /// 按序号倒序读取已持久化的 Thread effects。
    pub(crate) async fn effects(
        &self,
        agent_id: AgentId,
        query: ThreadEffectQuery,
    ) -> Result<ThreadEffectPage, SessionStoreError> {
        self.open(agent_id)
            .await?
            .query_thread_effects(&agent_id.to_string(), query)
            .await
    }

    /// 查询一个 review Turn 的模型计费事实；分页和完整性校验仍由 PL effect 查询负责。
    pub(crate) async fn billing(
        &self,
        agent_id: AgentId,
        turn_id: &TurnId,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<ReviewInferenceBillingPage, SessionStoreError> {
        let page = self
            .effects(
                agent_id,
                ThreadEffectQuery {
                    before_sequence,
                    limit,
                },
            )
            .await?;
        let mut records = Vec::new();
        for effect in page.effects {
            let Some(attempt) = effect.attempt else {
                continue;
            };
            if !attempt_belongs_to_input(&attempt.turn_id, turn_id) {
                continue;
            }
            let status = match &attempt.outcome {
                AttemptOutcome::Committed(_) => ReviewInferenceStatus::Committed,
                AttemptOutcome::Rejected { .. } => ReviewInferenceStatus::Rejected,
                AttemptOutcome::Cancelled { .. } => ReviewInferenceStatus::Cancelled,
                AttemptOutcome::Failed(_) => ReviewInferenceStatus::Failed,
                AttemptOutcome::Running | AttemptOutcome::Interrupted => continue,
            };
            let Some(billing) =
                pl_model::runtime::model_attempt_billing(&attempt, effect.committed_at)
                    .map_err(|error| SessionStoreError::Invalid(error.to_string()))?
            else {
                continue;
            };
            records.push(ReviewInferenceBilling {
                effect_sequence: effect.sequence,
                committed_at: effect.committed_at,
                turn_id: attempt.turn_id,
                attempt_id: attempt.attempt_id,
                status,
                billing,
            });
        }
        Ok(ReviewInferenceBillingPage {
            records,
            next_before_sequence: page.next_before_sequence,
        })
    }

    /// 产品 Agent 已删除且保留期到期后，通过 PL 的维护接口删除该会话。
    pub(crate) async fn delete(&self, agent_id: AgentId) -> Result<bool, SessionStoreError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.remove(&agent_id)
            && let Err(error) = session.shutdown().await
        {
            sessions.insert(agent_id, session);
            return Err(SessionStoreError::MaintenanceShutdown(error));
        }
        SqliteSessionStore::delete_session(
            SqliteSessionOptions {
                path: self.root.join(agent_id.to_string()).join("history.sqlite"),
            },
            &agent_id.to_string(),
        )
        .await
    }

    /// 停止所有已打开的持久化 writer；失败的句柄仍留在目录中以供重试。
    pub(crate) async fn shutdown(&self) -> Vec<(AgentId, std::sync::Arc<SessionStoreError>)> {
        let sessions = self.sessions.lock().await;
        let mut failures = Vec::new();
        for (agent_id, session) in sessions.iter() {
            if let Err(error) = session.shutdown().await {
                failures.push((*agent_id, error));
            }
        }
        failures
    }
}

fn attempt_belongs_to_input(attempt_turn_id: &str, input_id: &TurnId) -> bool {
    attempt_turn_id.starts_with(&format!("input:{input_id}:"))
}

#[cfg(test)]
mod tests {
    use super::attempt_belongs_to_input;

    #[test]
    fn matches_only_the_canonical_input_turn_prefix() {
        assert!(attempt_belongs_to_input(
            "input:review-input:2",
            &"review-input".to_owned(),
        ));
        assert!(!attempt_belongs_to_input(
            "input:review-input-other:2",
            &"review-input".to_owned(),
        ));
        assert!(!attempt_belongs_to_input(
            "message:7",
            &"review-input".to_owned(),
        ));
    }
}
