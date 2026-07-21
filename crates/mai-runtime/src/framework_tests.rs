use pretty_assertions::assert_eq;

use super::project_workspace_creation_was_interrupted;
use mai_protocol::{ProjectCloneStatus, ProjectStatus};

#[test]
fn startup_resume_recovers_interrupted_project_clone_states() {
    assert!(project_workspace_creation_was_interrupted(
        &ProjectStatus::Creating,
        &ProjectCloneStatus::Pending,
    ));
    assert!(project_workspace_creation_was_interrupted(
        &ProjectStatus::Creating,
        &ProjectCloneStatus::Cloning,
    ));
    assert!(!project_workspace_creation_was_interrupted(
        &ProjectStatus::Ready,
        &ProjectCloneStatus::Ready,
    ));
    assert!(!project_workspace_creation_was_interrupted(
        &ProjectStatus::Creating,
        &ProjectCloneStatus::Failed,
    ));
}

#[test]
fn product_agent_record_does_not_duplicate_framework_execution_state() {
    let state = include_str!("state.rs");
    let agent_record = state
        .split("pub(crate) struct AgentRecord")
        .nth(1)
        .expect("AgentRecord")
        .split('}')
        .next()
        .expect("AgentRecord body");

    for forbidden in [
        "sessions",
        "turn_lock",
        "cancel_requested",
        "active_turn",
        "pending_inputs",
    ] {
        assert!(
            !agent_record.contains(forbidden),
            "产品 AgentRecord 不应持有 PL 执行状态 `{forbidden}`"
        );
    }
}

#[test]
fn product_facade_uses_pl_agent_runtime_as_the_only_executor() {
    let runtime = format!(
        "{}\n{}",
        include_str!("lib.rs"),
        include_str!("runtime_bootstrap.rs")
    );

    assert!(runtime.contains("AgentRuntime<agent_host::MaiAgentHost>"));
    assert!(runtime.contains("fn framework_handle(&self) -> Result<pl_core::AgentRuntimeHandle>"));
    for removed in [
        "TurnControlSlot",
        "TurnGuard",
        "TurnTaskHandle",
        "HostedAgentRunner",
        "AgentControlBackend",
        "resume_agent",
    ] {
        assert!(
            !runtime.contains(removed),
            "mai-runtime 不应恢复旧执行路径 `{removed}`"
        );
    }
}

#[test]
fn framework_session_transitions_do_not_invalidate_product_agent_queries() {
    let observer = include_str!("agent_host/events.rs");

    assert!(observer.contains("persist_state(runtime, snapshot).await?"));
    assert!(
        !observer.contains("MaiProductEventKind::AgentUpdated"),
        "PL turn/session transitions must remain on the canonical session stream"
    );
}

#[test]
fn framework_module_boundary_is_stable() {
    let host = include_str!("agent_host/mod.rs");
    let expected_modules = [
        "events",
        "lifecycle",
        "policy",
        "protocol",
        "repository",
        "sessions",
        "trace_projection",
        "turn_factory",
    ];
    let actual_modules = expected_modules
        .iter()
        .copied()
        .filter(|name| host.contains(&format!("mod {name};")))
        .collect::<Vec<_>>();

    assert_eq!(actual_modules, expected_modules);
}

#[test]
fn project_maintainer_is_registered_with_framework_runtime() {
    let project_traits = include_str!("runtime_project_traits.rs");
    let create_maintainer = project_traits
        .split("async fn create_project_maintainer_agent")
        .nth(1)
        .expect("project maintainer implementation")
        .split("async fn save_project")
        .next()
        .expect("project maintainer implementation body");

    assert!(create_maintainer.contains("self.register_framework_agent(summary.id).await?;"));
}

#[test]
fn container_tools_do_not_request_host_path_approval() {
    let turn_factory = include_str!("agent_host/turn_factory.rs");

    assert!(turn_factory.contains(".with_permission_mode(pl_core::PermissionMode::FullAccess)"));
}

#[test]
fn product_session_projection_is_not_submitted_as_framework_identity() {
    let agent_api = include_str!("runtime_agent_api.rs");

    assert!(agent_api.contains("AgentSubmitRequest::start(session_id.framework, message)"));
    assert!(!agent_api.contains("SessionId::new(session_id.to_string())"));
}

#[test]
fn framework_close_is_followed_by_product_record_purge() {
    let agent_api = include_str!("runtime_agent_api.rs");
    let delete_agent = agent_api
        .split("pub async fn delete_agent")
        .nth(1)
        .expect("delete_agent implementation")
        .split("pub(super) async fn close_agent")
        .next()
        .expect("delete_agent implementation body");

    assert!(delete_agent.contains(".close(runtime_agent_id)"));
    assert!(delete_agent.contains("agents::purge_agent_tree(self, agent_id).await"));
}

#[test]
fn project_review_cycle_has_a_project_scoped_singleton_lock() {
    let runtime_workspace = include_str!("runtime_workspace.rs");
    let review_once = runtime_workspace
        .split("pub(super) async fn run_project_review_once")
        .nth(1)
        .expect("run_project_review_once implementation")
        .split("pub(super) async fn ensure_project_repository_ready")
        .next()
        .expect("run_project_review_once implementation body");

    assert!(review_once.contains("project.review_cycle_lock.lock().await"));
}
