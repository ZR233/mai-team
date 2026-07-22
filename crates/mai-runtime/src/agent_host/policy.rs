use std::collections::BTreeSet;

use pl_core::{
    AgentAccessPolicy, AgentExecutionPolicy, AgentRoleId, AgentSnapshot, AgentTargetSelector,
    ToolEffect, ToolEffectSet, ToolVisibilitySet, TurnFinalizationPolicy,
};

const COLLABORATION_TOOLS: [&str; 5] = [
    "spawn_agent",
    "send_input",
    "wait_agent",
    "list_agents",
    "close_agent",
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct MaiPolicyContext {
    pub(crate) can_manage_agents: bool,
}

/// 将 mai 的角色与父子关系编译成 PL 数据化执行策略。
pub(crate) fn compile_execution_policy(
    snapshot: &AgentSnapshot,
    configured_roles: impl IntoIterator<Item = AgentRoleId>,
    base_visibility: ToolVisibilitySet,
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
        wait_targets: AgentTargetSelector::Tree,
        close_targets: if context.can_manage_agents {
            AgentTargetSelector::Tree
        } else {
            AgentTargetSelector::None
        },
    };
    let mut visible_tools = base_visibility;
    visible_tools.extend_tool_names(
        COLLABORATION_TOOLS
            .into_iter()
            .filter(|name| collaboration_tool_visible(name, &collaboration)),
    );
    AgentExecutionPolicy {
        visible_tools,
        allowed_effects: ToolEffectSet::from_effects(allowed_effects(
            snapshot.identity.role.as_str(),
        )),
        collaboration,
        finalization: TurnFinalizationPolicy::Direct,
    }
}

fn collaboration_tool_visible(name: &str, policy: &AgentAccessPolicy) -> bool {
    match name {
        "spawn_agent" => !policy.spawn_roles.is_empty(),
        "close_agent" => !matches!(policy.close_targets, AgentTargetSelector::None),
        "send_input" | "wait_agent" | "list_agents" => true,
        _ => false,
    }
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
    use pl_core::{AgentActivityState, AgentId, AgentIdentity, AgentLifecycleState};

    #[test]
    fn child_policy_has_no_spawn_or_close() {
        let snapshot = snapshot(Some("parent"), "executor");
        let policy = compile_execution_policy(
            &snapshot,
            [AgentRoleId::new("executor").unwrap()],
            ToolVisibilitySet::from_tool_names(["read_file"]),
            MaiPolicyContext {
                can_manage_agents: false,
            },
        );

        assert!(policy.collaboration.spawn_roles.is_empty());
        assert!(matches!(
            policy.collaboration.close_targets,
            AgentTargetSelector::None
        ));
        assert!(policy.visible_tools.contains("send_input"));
    }

    #[test]
    fn product_maintainer_can_manage_agents_independently_of_tree_position() {
        let snapshot = snapshot(Some("parent"), "executor");
        let executor = AgentRoleId::new("executor").unwrap();
        let policy = compile_execution_policy(
            &snapshot,
            [executor.clone()],
            ToolVisibilitySet::from_tool_names(["read_file"]),
            MaiPolicyContext {
                can_manage_agents: true,
            },
        );

        assert!(policy.collaboration.spawn_roles.contains(&executor));
        assert!(matches!(
            policy.collaboration.close_targets,
            AgentTargetSelector::Tree
        ));
        assert!(policy.visible_tools.contains("spawn_agent"));
        assert!(policy.visible_tools.contains("close_agent"));
    }

    #[test]
    fn reviewer_can_read_write_and_run_without_branch_control() {
        let policy = compile_execution_policy(
            &snapshot(Some("maintainer"), "reviewer"),
            std::iter::empty(),
            ToolVisibilitySet::from_tool_names(["read_file", "write_file", "exec"]),
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
                id: AgentId::new("agent").unwrap(),
                parent_id: parent.map(|id| AgentId::new(id).unwrap()),
                role: AgentRoleId::new(role).unwrap(),
                depth: parent.is_some() as u32,
            },
            lifecycle: AgentLifecycleState::Active,
            activity: AgentActivityState::Idle,
            active_turn_id: None,
            active_session_id: None,
            pending_inputs: 0,
            last_turn: None,
            revision: 1,
            event_sequence: 1,
            updated_at: 0,
        }
    }
}
