use crate::mcp::McpTool;
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
) -> pl_core::ToolVisibilitySet {
    let capability = agent_capability(state, agent).await;
    let summary = agent.summary.read().await.clone();
    let has_project_workspace = summary.project_id.is_some();
    let shared_visibility = pl_core::HostedSharedToolVisibility::default()
        .with_git(has_project_workspace)
        .with_spawn_agent(capability.can_spawn_agents)
        .with_close_agent(capability.can_close_agents);
    let mut product_tools = vec![
        crate::turn::product_tool_schemas::TOOL_SAVE_ARTIFACT.to_string(),
        crate::turn::product_tool_schemas::TOOL_GITHUB_API_REQUEST.to_string(),
    ];
    if task_plan_tool_visible(&summary) {
        product_tools.push(crate::turn::product_tool_schemas::TOOL_SAVE_TASK_PLAN.to_string());
    }
    if task_review_result_tool_visible(&summary) {
        product_tools
            .push(crate::turn::product_tool_schemas::TOOL_SUBMIT_REVIEW_RESULT.to_string());
    }
    if project_review_queue_tool_visible(&summary) {
        product_tools
            .push(crate::turn::product_tool_schemas::TOOL_QUEUE_PROJECT_REVIEW_PRS.to_string());
    }
    pl_core::ToolVisibilitySet::hosted_container_with_tool_names(
        shared_visibility,
        product_tools,
        mcp_tools.iter().map(|tool| tool.model_name.clone()),
    )
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

fn task_plan_tool_visible(summary: &mai_protocol::AgentSummary) -> bool {
    summary.task_id.is_some() && matches!(summary.role, Some(AgentRole::Planner))
}

fn task_review_result_tool_visible(summary: &mai_protocol::AgentSummary) -> bool {
    summary.task_id.is_some() && matches!(summary.role, Some(AgentRole::Reviewer))
}

fn project_review_queue_tool_visible(summary: &mai_protocol::AgentSummary) -> bool {
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
