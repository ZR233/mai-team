use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole, AgentSummary, now};

use crate::Result;
use crate::state::AgentRecord;

mod container;
mod create;
mod files;
mod model;
mod observability;
pub(crate) mod profiles;
mod purge;
mod resources;
mod update;

pub(crate) use container::{
    AgentContainerOps, AgentContainerStartRequest, AgentContainerStatusChange,
    AgentMcpStatusChange, ContainerSource, ensure_agent_container,
    ensure_agent_container_with_source,
};
pub(crate) use create::{AgentCreateOps, CreateAgentRecordContext, create_agent_record};
pub(crate) use files::{AgentFileOps, download_file_tar, upload_file};
pub(crate) use model::normalize_reasoning_effort;
pub(crate) use observability::{
    AgentObservabilityOps, agent_logs, tool_output_artifact, tool_trace, tool_traces,
};
pub(crate) use purge::{AgentPurgeOps, purge_agent_tree};
pub(crate) use resources::{AgentResourceBroker, AgentResourceBrokerOps, agent_resource_broker};
pub(crate) use update::{AgentUpdateOps, update_agent};

/// 关闭产品资源时所需的最小端口；framework 生命周期由 PL 独占。
pub(crate) trait AgentCloseOps: Send + Sync {
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn persist_agent(
        &self,
        agent: &AgentRecord,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    fn delete_agent_containers(
        &self,
        agent_id: AgentId,
        preferred_container_id: Option<String>,
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

pub(crate) async fn close_agent(ops: &impl AgentCloseOps, agent_id: AgentId) -> Result<()> {
    let agent = ops.agent(agent_id).await?;
    if let Some(manager) = agent.mcp.write().await.take() {
        manager.shutdown().await;
    }
    let in_memory_container_id = agent
        .container
        .write()
        .await
        .take()
        .map(|container| container.id);
    let persisted_container_id = agent.summary.read().await.container_id.clone();
    let preferred_container_id = in_memory_container_id.or(persisted_container_id);
    ops.delete_agent_containers(agent_id, preferred_container_id)
        .await?;
    {
        let mut summary = agent.summary.write().await;
        summary.container_id = None;
        summary.updated_at = now();
    }
    ops.persist_agent(&agent).await?;
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
