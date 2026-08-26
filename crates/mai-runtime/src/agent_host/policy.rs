use std::collections::BTreeSet;

use pl_core::{
    AgentAccessPolicy, AgentExecutionPolicy, AgentRoleId, AgentSnapshot, AgentTargetSelector,
    ToolEffect, ToolEffectSet, TurnFinalizationPolicy,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct MaiPolicyContext {
    pub(crate) can_manage_agents: bool,
}

/// 将 mai 的角色与父子关系编译成 PL 数据化执行策略。
pub(crate) fn compile_execution_policy(
    snapshot: &AgentSnapshot,
    configured_roles: impl IntoIterator<Item = AgentRoleId>,
    context: MaiPolicyContext,
) -> AgentExecutionPolicy {
    let spawn_roles = if context.can_manage_agents {
        configured_roles.into_iter().collect()
    } else {
        BTreeSet::new()
    };
    let collaboration = AgentAccessPolicy {
        spawn_roles,
        list_targets: AgentTargetSelector::Tree,
        message_targets: AgentTargetSelector::Tree,
        close_targets: if context.can_manage_agents {
            AgentTargetSelector::Tree
        } else {
            AgentTargetSelector::None
        },
    };
    AgentExecutionPolicy {
        allowed_effects: ToolEffectSet::from_effects(allowed_effects(
            snapshot.identity.role.as_str(),
        )),
        collaboration,
        finalization: TurnFinalizationPolicy::Direct,
    }
}

pub(crate) async fn can_manage_agents(
    state: &crate::state::RuntimeState,
    agent: &crate::state::AgentRecord,
) -> bool {
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
    is_project_maintainer || summary.parent_id.is_none()
}

fn allowed_effects(role: &str) -> Vec<ToolEffect> {
    match role {
        "planner" => vec![
            ToolEffect::Read,
            ToolEffect::AgentControl,
            ToolEffect::BranchControl,
        ],
        "explorer" => vec![ToolEffect::Read, ToolEffect::AgentControl],
        "reviewer" => vec![
            ToolEffect::Read,
            ToolEffect::WorkspaceWrite,
            ToolEffect::Process,
            ToolEffect::AgentControl,
        ],
        "executor" => vec![
            ToolEffect::Read,
            ToolEffect::WorkspaceWrite,
            ToolEffect::Process,
            ToolEffect::AgentControl,
            ToolEffect::BranchControl,
        ],
        _ => vec![
            ToolEffect::Read,
            ToolEffect::WorkspaceWrite,
            ToolEffect::Process,
            ToolEffect::AgentControl,
            ToolEffect::BranchControl,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pl_core::{AgentIdentity, AgentState, ThreadId};

    #[test]
    fn child_policy_has_no_spawn_or_close() {
        let snapshot = snapshot(Some("parent"), "executor");
        let policy = compile_execution_policy(
            &snapshot,
            [AgentRoleId::new("executor").unwrap()],
            MaiPolicyContext {
                can_manage_agents: false,
            },
        );

        assert!(policy.collaboration.spawn_roles.is_empty());
        assert!(matches!(
            policy.collaboration.close_targets,
            AgentTargetSelector::None
        ));
    }

    #[test]
    fn product_maintainer_can_manage_agents_independently_of_tree_position() {
        let snapshot = snapshot(Some("parent"), "executor");
        let executor = AgentRoleId::new("executor").unwrap();
        let policy = compile_execution_policy(
            &snapshot,
            [executor.clone()],
            MaiPolicyContext {
                can_manage_agents: true,
            },
        );

        assert!(policy.collaboration.spawn_roles.contains(&executor));
        assert!(matches!(
            policy.collaboration.close_targets,
            AgentTargetSelector::Tree
        ));
    }

    #[test]
    fn reviewer_can_read_write_and_run_without_branch_control() {
        let policy = compile_execution_policy(
            &snapshot(Some("maintainer"), "reviewer"),
            std::iter::empty(),
            MaiPolicyContext {
                can_manage_agents: false,
            },
        );

        assert!(policy.allowed_effects.contains(ToolEffect::Read));
        assert!(policy.allowed_effects.contains(ToolEffect::WorkspaceWrite));
        assert!(policy.allowed_effects.contains(ToolEffect::Process));
        assert!(policy.allowed_effects.contains(ToolEffect::AgentControl));
        assert!(!policy.allowed_effects.contains(ToolEffect::BranchControl));
    }

    fn snapshot(parent: Option<&str>, role: &str) -> AgentSnapshot {
        AgentSnapshot {
            identity: AgentIdentity {
                id: ThreadId::new("agent").unwrap(),
                parent_id: parent.map(|id| ThreadId::new(id).unwrap()),
                role: AgentRoleId::new(role).unwrap(),
                depth: parent.is_some() as u32,
            },
            state: AgentState::idle(),
            pending_inputs: 0,
            progress: None,
            last_turn: None,
            revision: 1,
            event_sequence: 1,
            updated_at: 0,
        }
    }
}
