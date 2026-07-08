use std::collections::HashSet;

use mai_mcp::McpTool;
use mai_protocol::{AgentId, AgentRole};

use crate::state::{AgentRecord, RuntimeState};

#[derive(Debug, Clone)]
struct AgentCapability {
    can_spawn_agents: bool,
    can_close_agents: bool,
    communication: AgentCommunicationPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentCommunicationPolicy {
    All,
    ParentAndMaintainer,
}

/// 计算当前 agent 对模型可见的工具名集合。
///
/// pl-core 提供共享工具 schema 与执行内核；mai-team 仅在这里叠加产品策略，
/// 包括 project agent 的 git 工具、review queue 工具和协作 agent 通信边界。
pub(crate) async fn visible_tool_names(
    state: &RuntimeState,
    agent: &AgentRecord,
    mcp_tools: &[McpTool],
) -> HashSet<String> {
    let capability = agent_capability(state, agent).await;
    let mut names = pl_core::shared_tool_names(shared_tool_name_options())
        .into_iter()
        .filter(|name| default_shared_tool_visible(name))
        .collect::<HashSet<_>>();
    names.extend([
        pl_core::TOOL_LIST_MCP_RESOURCES.to_string(),
        pl_core::TOOL_LIST_MCP_RESOURCE_TEMPLATES.to_string(),
        pl_core::TOOL_READ_MCP_RESOURCE.to_string(),
        mai_tools::TOOL_SAVE_TASK_PLAN.to_string(),
        mai_tools::TOOL_SUBMIT_REVIEW_RESULT.to_string(),
        mai_tools::TOOL_SAVE_ARTIFACT.to_string(),
        mai_tools::TOOL_GITHUB_API_REQUEST.to_string(),
    ]);
    if agent.summary.read().await.project_id.is_some() {
        names.extend(canonical_git_tool_names().map(str::to_string));
    }
    if project_review_queue_tool_visible(agent).await {
        names.insert(mai_tools::TOOL_QUEUE_PROJECT_REVIEW_PRS.to_string());
    }
    if capability.can_spawn_agents {
        names.insert(pl_core::TOOL_SPAWN_AGENT.to_string());
    }
    if capability.can_close_agents {
        names.insert(pl_core::TOOL_CLOSE_AGENT.to_string());
    }
    names.extend(mcp_tools.iter().map(|tool| tool.model_name.clone()));
    names
}

fn default_shared_tool_visible(name: &str) -> bool {
    !matches!(name, pl_core::TOOL_SPAWN_AGENT | pl_core::TOOL_CLOSE_AGENT)
        && !canonical_git_tool_names().any(|tool| tool == name)
}

fn canonical_git_tool_names() -> impl Iterator<Item = &'static str> {
    [
        pl_core::TOOL_GIT_STATUS,
        pl_core::TOOL_GIT_DIFF,
        pl_core::TOOL_GIT_BRANCH,
        pl_core::TOOL_GIT_FETCH,
        pl_core::TOOL_GIT_COMMIT,
        pl_core::TOOL_GIT_PUSH,
        pl_core::TOOL_GIT_WORKSPACE_INFO,
        pl_core::TOOL_GIT_SYNC_DEFAULT_BRANCH,
    ]
    .into_iter()
}

fn shared_tool_name_options() -> pl_core::SharedToolSchemaOptions {
    pl_core::SharedToolSchemaOptions::from_capabilities(
        &pl_core::ToolCapabilityConfig::hosted_container_workspace(),
    )
    .with_plan_exit(false)
}

async fn agent_capability(state: &RuntimeState, agent: &AgentRecord) -> AgentCapability {
    let summary = agent.summary.read().await.clone();
    let is_project_maintainer = if let Some(project_id) = summary.project_id {
        let project = state.projects.read().await.get(&project_id).cloned();
        if let Some(project) = project {
            project.summary.read().await.maintainer_agent_id == summary.id
        } else {
            false
        }
    } else {
        summary.parent_id.is_none()
    };
    if is_project_maintainer || summary.parent_id.is_none() {
        AgentCapability {
            can_spawn_agents: true,
            can_close_agents: true,
            communication: AgentCommunicationPolicy::All,
        }
    } else {
        AgentCapability {
            can_spawn_agents: false,
            can_close_agents: false,
            communication: AgentCommunicationPolicy::ParentAndMaintainer,
        }
    }
}

async fn project_review_queue_tool_visible(agent: &AgentRecord) -> bool {
    let summary = agent.summary.read().await;
    summary.project_id.is_some()
        && matches!(
            summary.role,
            Some(AgentRole::Explorer | AgentRole::Reviewer)
        )
}

pub(crate) async fn agent_can_access_target(
    state: &RuntimeState,
    agent: &AgentRecord,
    target: AgentId,
) -> bool {
    let capability = agent_capability(state, agent).await;
    if capability.communication == AgentCommunicationPolicy::All {
        return true;
    }
    let summary = agent.summary.read().await.clone();
    if summary.parent_id == Some(target) {
        return true;
    }
    let Some(project_id) = summary.project_id else {
        return false;
    };
    let project = state.projects.read().await.get(&project_id).cloned();
    if let Some(project) = project {
        project.summary.read().await.maintainer_agent_id == target
    } else {
        false
    }
}
