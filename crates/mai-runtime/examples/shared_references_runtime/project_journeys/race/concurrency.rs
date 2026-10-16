//! T29 第二段并发编排：删除子树、保存/应用引用，同时观察被 hold 的 Turn 终态。

use std::time::{Duration, Instant};

use mai_protocol::{AgentId, ProjectId, ProjectReferenceBinding};
use serde::Serialize;
use serde_json::json;
use tokio::time::sleep;

use crate::error::Result;
use crate::host::RuntimeHost;
use crate::probe;

use super::references::{self, SavedApply};
use super::support;

/// 已确认创建、供失败清理与终态核验使用的产品身份。
#[derive(Debug, Default, Clone, Serialize)]
pub(super) struct CreatedAgents {
    pub child: Option<AgentId>,
    pub grandchild: Option<AgentId>,
}

/// child Turn 的实际观察终态；`disappeared_before_terminal` 不被当作成功。
#[derive(Debug, Clone, Serialize)]
pub(super) struct TurnRaceWatch {
    pub last_state: String,
    pub terminal: bool,
    pub completed: bool,
    pub cancelled: bool,
    pub disappeared_before_terminal: bool,
    pub observation_error: Option<String>,
}

/// 三个并发入口的原始结果与可序列化事实。
#[derive(Debug, Clone, Serialize)]
pub(super) struct RaceOutcome {
    pub deleted: serde_json::Value,
    pub reference: serde_json::Value,
    pub watch: TurnRaceWatch,
    #[serde(skip)]
    deleted_private: Result<()>,
    #[serde(skip)]
    reference_private: Result<SavedApply>,
}

impl RaceOutcome {
    pub(super) fn deleted_result(&self) -> Result<()> {
        self.deleted_private.clone()
    }

    pub(super) fn reference_result(&self) -> Result<SavedApply> {
        self.reference_private.clone()
    }
}

/// 同时启动三个未来；每个真实入口都包在独立硬超时内。
pub(super) async fn delete_apply_and_watch(
    host: &RuntimeHost,
    project: ProjectId,
    expected_revision: u64,
    bindings: Vec<ProjectReferenceBinding>,
    child: AgentId,
    input_id: &str,
    limit: Duration,
) -> RaceOutcome {
    let delete =
        async { support::bounded(host, "delete_agent", host.runtime.delete_agent(child)).await };
    let reference =
        async { references::save_and_apply(host, project, expected_revision, bindings).await };
    let watch = watch_child_turn(host, child, input_id, limit);
    let (deleted, reference, watch) = tokio::join!(delete, reference, watch);
    RaceOutcome {
        deleted: support::result_fact(&deleted),
        reference: match &reference {
            Ok(saved) => json!(saved),
            Err(_) => support::result_fact(&reference),
        },
        watch,
        deleted_private: deleted,
        reference_private: reference,
    }
}

/// 有界观察 child Turn：只接受 Completed/Cancelled；删除先移除记录时显式留缺口。
async fn watch_child_turn(
    host: &RuntimeHost,
    child: AgentId,
    input_id: &str,
    limit: Duration,
) -> TurnRaceWatch {
    let deadline = Instant::now() + limit;
    let mut last_error = None;
    loop {
        match support::bounded(
            host,
            "t29_child_turn_snapshot",
            probe::snapshot(host, child, input_id),
        )
        .await
        {
            Ok(snapshot) => {
                if snapshot.terminal() {
                    return TurnRaceWatch {
                        last_state: snapshot.state.clone(),
                        terminal: true,
                        completed: snapshot.completed(),
                        cancelled: snapshot.cancelled(),
                        disappeared_before_terminal: false,
                        observation_error: last_error,
                    };
                }
                if Instant::now() >= deadline {
                    return timed_out(snapshot.state, last_error);
                }
            }
            Err(cause) => {
                last_error = Some(cause.message().to_string());
                let exists = support::bounded_value(
                    host,
                    "t29_watch_agent_list",
                    host.runtime.list_agents(),
                )
                .await;
                let summaries = match exists {
                    Ok(summaries) => summaries,
                    Err(cause) => {
                        return timed_out(
                            "agent_query_failed".into(),
                            Some(cause.message().to_string()),
                        );
                    }
                };
                let exists = summaries.iter().any(|summary| summary.id == child);
                let missing = !exists
                    && matches!(
                        tokio::time::timeout(
                            support::operation_limit(host),
                            host.runtime.get_agent(child),
                        ).await,
                        Ok(Err(mai_runtime::RuntimeError::AgentNotFound(id))) if id == child
                    );
                if !exists && missing {
                    return TurnRaceWatch {
                        last_state: "agent_not_found_after_delete".to_string(),
                        terminal: false,
                        completed: false,
                        cancelled: false,
                        disappeared_before_terminal: true,
                        observation_error: last_error,
                    };
                }
                if Instant::now() >= deadline {
                    return timed_out("observation_error".to_string(), last_error);
                }
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn timed_out(state: String, observation_error: Option<String>) -> TurnRaceWatch {
    TurnRaceWatch {
        last_state: state,
        terminal: false,
        completed: false,
        cancelled: false,
        disappeared_before_terminal: false,
        observation_error,
    }
}
