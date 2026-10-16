use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole, AgentSummary};

use crate::Result;
use crate::state::AgentRecord;

mod container;
mod create;
mod delete;
mod files;
mod model;
mod observability;
pub(crate) mod profiles;
mod purge;
mod recovery;
mod update;

pub(crate) use container::{
    AgentContainerOps, AgentContainerStartRequest, AgentContainerStatusChange,
    AgentMcpStatusChange, ContainerSource, ensure_agent_container_with_source,
};
pub(crate) use create::{
    AgentCreateOps, CreateAgentRecordContext, CreatedAgentRecord, create_agent_record,
};
pub(crate) use delete::{AgentDeleteOps, delete_agent, rollback_unregistered_agent};
pub(crate) use files::{AgentFileOps, download_file_tar, upload_file};
pub(crate) use model::normalize_reasoning_effort;
pub(crate) use observability::{
    AgentObservabilityOps, agent_logs, tool_output_artifact, tool_trace, tool_traces,
};
pub(crate) use purge::{AgentPurgeOps, descendant_delete_order, purge_agent_records};
pub(crate) use recovery::{
    AgentResourceRecoveryOps, AgentResourceRecoveryRequest, PreparedAgentWorkspace,
    agent_resource_recovery_retry_request, recover_agent_resources,
};
pub(crate) use update::{AgentUpdateOps, update_agent};

/// 关闭产品资源时所需的最小端口；framework 生命周期由 PL 独占。
///
/// `close` 只释放执行环境：容器、binding 与软件数据必须保留，让后续 `ensure` 能重新验证
/// 并复用同一环境。真正删除容器与工作区只发生在明确授权的 [`AgentDeleteOps`] 路径。
pub(crate) trait AgentCloseOps: Send + Sync {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send;

    /// 停止（不删除）该 Agent 的执行环境，并撤销其固定环境租约。
    ///
    /// `container_id` 是最后一次验证通过的环境收据：内存容器句柄优先，其次取持久化投影。
    /// 实现必须保留容器、`agent_reference_bindings` 与工作区数据，返回实际停止的容器 ID；
    /// 容器已经不存在时视为已完成，不得报错，也不得代以删除或重建。
    fn release_agent_environment(
        &self,
        agent_id: AgentId,
        container_id: Option<String>,
    ) -> impl std::future::Future<Output = Result<Vec<String>>> + Send;
}

pub(crate) async fn list_agents(agents: Vec<Arc<AgentRecord>>) -> Vec<AgentSummary> {
    let mut summaries = Vec::with_capacity(agents.len());
    for agent in agents {
        summaries.push(agent.summary.read().await.clone());
    }
    summaries.sort_by_key(|summary| summary.created_at);
    summaries
}

/// 关闭一个 Agent 的执行环境，同时保留全部产品收据与工作区数据。
///
/// 顺序固定为：关闭 MCP 运行时（含 sidecar）→ 只停止容器并撤销环境租约。容器、
/// `agent_reference_bindings`、`container_id` 投影与工作区都不会被删除，因此后续
/// `ensure` 可以按同一 binding 重新验证并复用同一环境，而不是退回基础镜像重建。
///
/// 本函数不触碰准入 gate，也不发布租约：关闭之后没有可用租约就等于没有可用的旧授权，
/// 仍然阻塞的 pending/failed 修订继续由应用状态机保持阻塞，不会被这里复活。
pub(crate) async fn close_agent(ops: &impl AgentCloseOps, agent_id: AgentId) -> Result<()> {
    let agent = ops.agent(agent_id).await?;
    if let Some(manager) = agent.mcp.write().await.take() {
        manager.shutdown().await;
    }
    // 容器句柄仍然指向真实存在的容器：这里不接管、不清空它，也不改写 `container_id`。
    let in_memory_container_id = agent
        .container
        .read()
        .await
        .as_ref()
        .map(|container| container.id.clone());
    let container_id = match in_memory_container_id {
        Some(container_id) => Some(container_id),
        None => agent.summary.read().await.container_id.clone(),
    };
    ops.release_agent_environment(agent_id, container_id)
        .await?;
    Ok(())
}

pub(crate) const PLANNER_SYSTEM_PROMPT: &str = r#"You are the Planner for a Mai task. Your job is to create a decision-complete implementation plan through a structured 3-phase process. A decision-complete plan can be handed to the Executor agent and implemented without any additional design decisions.

## 3-Phase Planning Process

### Phase 1 — Explore (discover facts, eliminate unknowns)
- Use `spawn_agent` with role `explorer` to investigate code, docs, and relevant context.
- Run read-only commands to understand the codebase structure, existing patterns, and constraints.
- Do NOT ask the user questions that can be answered by exploring the code.
- Only ask clarifying questions about the prompt if there are obvious ambiguities.

### Phase 2 — Intent Chat (clarify what they want)
- Use `request_user_input` to ask structured questions about: goal + success criteria, scope, constraints, and key preferences/tradeoffs.
- Each question must materially change the plan, confirm an assumption, or choose between meaningful tradeoffs.
- Offer 2-4 clear options with a recommended default.
- Bias toward asking over guessing when high-impact ambiguity remains.

### Phase 3 — Implementation Spec (produce the plan)
- Create a complete implementation specification covering: approach, interfaces/data flow, edge cases, testing strategy, and assumptions.
- The plan must be decision-complete — the Executor should not need to make any design decisions.

## Rules

- **No code modification**: Only explore and plan. Never edit files or make changes.
- **Use `save_task_plan`** to save or update the plan with a clear title and complete Markdown content.
- **Use `update_todo_list`** to show your planning progress to the user.
- **Use `request_user_input`** for structured questions during planning.
- When the user requests revision of the plan, address their feedback fully and save an updated plan.

## Plan Format

The plan should include:
- A clear title
- A brief summary
- Key changes grouped by subsystem or behavior
- Important API/interface changes
- Test cases and scenarios
- Explicit assumptions and defaults chosen

Keep the plan concise and actionable. Prefer behavior-level descriptions over file-by-file inventories. Mention specific files only when needed to disambiguate a non-obvious change."#;

pub(crate) fn task_role_system_prompt(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Planner => PLANNER_SYSTEM_PROMPT,
        AgentRole::Explorer => {
            "You are an Explorer subagent for a task. Investigate code, docs, and relevant context using read-only exploration unless explicitly told otherwise. Return concise findings with concrete files, commands, or sources that help the planner decide."
        }
        AgentRole::Executor => {
            "You are the Executor for an approved task plan. Implement the requested changes in your container, keep scope tight, run verification, and report changed files plus test results. If reviewer feedback arrives, fix the issues and rerun relevant checks.\n\nWhen you have produced deliverable files (reports, generated code, data exports, documents, etc.), use the `save_artifact` tool to register each file so the user can download it. Always call `save_artifact` for any final output the user would want to keep."
        }
        AgentRole::Reviewer => {
            "You are the Reviewer for a task workflow. Review executor changes for bugs, regressions, missing tests, and unclear behavior. You must call submit_review_result with passed, findings, and summary before finishing. Set passed=true only when there are no blocking issues."
        }
    }
}

pub(crate) fn short_id(id: AgentId) -> String {
    id.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mai_docker::ContainerHandle;
    use mai_protocol::{AgentResourceSnapshot, AgentSummary, RuntimeUsageSnapshot};
    use pretty_assertions::assert_eq;
    use tokio::sync::{Mutex, RwLock};
    use uuid::Uuid;

    use super::*;
    use crate::RuntimeError;

    /// 只记录"释放执行环境"的请求；旧实现会在这里调删除并清空收据。
    struct FakeCloseOps {
        agent: Arc<AgentRecord>,
        releases: Mutex<Vec<Option<String>>>,
    }

    impl AgentCloseOps for FakeCloseOps {
        async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
            if self.agent.summary.read().await.id == agent_id {
                Ok(Arc::clone(&self.agent))
            } else {
                Err(RuntimeError::AgentNotFound(agent_id))
            }
        }

        async fn release_agent_environment(
            &self,
            _agent_id: AgentId,
            container_id: Option<String>,
        ) -> Result<Vec<String>> {
            self.releases.lock().await.push(container_id.clone());
            Ok(container_id.into_iter().collect())
        }
    }

    /// 缺陷敏感：关闭必须只停止执行环境。旧实现会删除容器并清空 `container_id`，
    /// 于是 binding 悬空、后续 `ensure` 永远撞上 `EnvironmentRecoveryUnavailable`。
    #[tokio::test]
    async fn close_agent_stops_environment_and_keeps_the_container_receipt() {
        let agent = agent_record("container-1");
        let ops = FakeCloseOps {
            agent: Arc::clone(&agent),
            releases: Mutex::new(Vec::new()),
        };
        let agent_id = agent.summary.read().await.id;

        close_agent(&ops, agent_id).await.expect("close agent");

        assert_eq!(
            vec![Some("container-1".to_string())],
            *ops.releases.lock().await
        );
        let summary = agent.summary.read().await.clone();
        assert_eq!(summary.container_id, Some("container-1".to_string()));
        assert!(
            agent.container.read().await.is_some(),
            "停止不等于删除：内存里的容器句柄仍指向同一个环境"
        );
    }

    /// 没有内存句柄时按持久化投影释放，同样不能清空收据。
    #[tokio::test]
    async fn close_agent_uses_persisted_projection_without_clearing_it() {
        let agent = agent_record("container-2");
        *agent.container.write().await = None;
        let ops = FakeCloseOps {
            agent: Arc::clone(&agent),
            releases: Mutex::new(Vec::new()),
        };
        let agent_id = agent.summary.read().await.id;

        close_agent(&ops, agent_id).await.expect("close agent");

        assert_eq!(
            vec![Some("container-2".to_string())],
            *ops.releases.lock().await
        );
        assert_eq!(
            agent.summary.read().await.container_id,
            Some("container-2".to_string())
        );
    }

    #[tokio::test]
    async fn close_agent_reports_a_missing_agent() {
        let agent = agent_record("container-3");
        let ops = FakeCloseOps {
            agent: Arc::clone(&agent),
            releases: Mutex::new(Vec::new()),
        };

        let error = close_agent(&ops, Uuid::new_v4())
            .await
            .expect_err("unknown agent must fail");

        assert!(matches!(error, RuntimeError::AgentNotFound(_)));
        assert!(ops.releases.lock().await.is_empty());
    }

    fn agent_record(container_id: &str) -> Arc<AgentRecord> {
        let timestamp = mai_protocol::now();
        Arc::new(AgentRecord {
            summary: RwLock::new(AgentSummary {
                id: Uuid::new_v4(),
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                profile_id: None,
                workspace: None,
                review_run_id: None,
                name: "agent".to_string(),
                resource: AgentResourceSnapshot::default(),
                runtime: None,
                last_turn: None,
                container_id: Some(container_id.to_string()),
                docker_image: "image".to_string(),
                provider_id: "provider".to_string(),
                provider_name: "Provider".to_string(),
                model: "model".to_string(),
                reasoning_effort: None,
                created_at: timestamp,
                updated_at: timestamp,
                usage: RuntimeUsageSnapshot::default(),
            }),
            registration_pending: std::sync::atomic::AtomicBool::new(false),
            container: RwLock::new(Some(ContainerHandle {
                id: container_id.to_string(),
                name: format!("mai-team-{container_id}"),
                image: "image".to_string(),
            })),
            mcp: RwLock::new(None),
            active_mcp_servers: RwLock::new(Vec::new()),
            review_context: RwLock::new(None),
            skill_catalog: RwLock::new(None),
            system_prompt: None,
        })
    }
}
