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
fn framework_thread_transitions_do_not_invalidate_product_agent_queries() {
    let observer = include_str!("agent_host/events.rs");

    assert!(observer.contains("persist_state(runtime, *snapshot).await?"));
    assert!(
        !observer.contains("MaiProductEventKind::AgentUpdated"),
        "PL Thread transitions must remain on the canonical Thread stream"
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
        "thread",
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
    let provisioning = include_str!("runtime_provisioning.rs");
    let create_maintainer = project_traits
        .split("async fn create_project_maintainer_agent")
        .nth(1)
        .expect("project maintainer implementation")
        .split("async fn save_project")
        .next()
        .expect("project maintainer implementation body");

    assert!(create_maintainer.contains("self.register_prepared_agent(resource).await"));
    assert!(provisioning.contains("self.register_framework_agent(resource.id()).await"));
}

#[test]
fn agent_creation_owner_covers_provisioning_and_framework_registration() {
    let provisioning = include_str!("runtime_provisioning.rs");
    let creation_owner = include_str!("runtime_agent_creation.rs");
    let create_record = include_str!("agents/create.rs");
    let create_environment = include_str!("runtime_environment.rs");

    assert!(provisioning.contains("let provisioning: Result<AgentSummary> = async"));
    assert!(provisioning.contains("resource.rollback().await"));
    assert!(provisioning.contains("resource.include_canonical_runtime();"));
    assert!(creation_owner.contains("impl Drop for AgentCreationLease"));
    assert!(creation_owner.contains("rollback_unregistered_agent"));
    assert!(creation_owner.contains("agents::delete_agent"));
    assert!(!create_record.contains("MaiProductEventKind::AgentCreated"));
    assert!(!create_environment.contains("MaiProductEventKind::AgentCreated"));
    assert!(
        provisioning.find("resource.commit()").unwrap()
            < provisioning
                .find("MaiProductEventKind::AgentCreated")
                .unwrap()
    );
}

#[test]
fn framework_spawn_rollback_only_deletes_resources_created_by_its_lease() {
    let lifecycle = include_str!("agent_host/lifecycle.rs");

    assert!(lifecycle.contains("ownership: SpawnProductOwnership"));
    assert!(lifecycle.contains("SpawnProductOwnership::Borrowed => return Ok(())"));
    assert!(lifecycle.contains("SpawnProductOwnership::CreatedHere => {}"));
    assert!(lifecycle.contains("impl Drop for MaiSpawnLease"));
    assert!(lifecycle.contains("agents::delete_agent(runtime.as_ref(), agent_id)"));
}

#[test]
fn startup_reconcile_rebuilds_missing_agent_resources_through_one_lifecycle_owner() {
    let provisioning = include_str!("runtime_provisioning.rs");
    let recovery = include_str!("agents/recovery.rs");

    assert!(provisioning.contains("recover_project_agent_resources"));
    assert!(
        !provisioning
            .contains("PROJECT_AGENT_WORKSPACE_VOLUME_MISSING_AFTER_STARTUP_RECONCILE.to_string()")
    );
    assert!(!provisioning.contains("recovered_agent_count"));
    assert!(recovery.contains("Drop for AgentResourceRecoveryLease"));
}

#[test]
fn container_tools_do_not_request_host_path_approval() {
    let turn_factory = include_str!("agent_host/turn_factory.rs");

    assert!(turn_factory.contains(".with_permission_mode(pl_core::PermissionMode::FullAccess)"));
}

#[test]
fn product_instructions_are_overlays_on_the_pl_base_prompt() {
    let turn_factory = include_str!("agent_host/turn_factory.rs");

    assert!(turn_factory.contains("InstructionProfile::new()"));
    assert!(turn_factory.contains(".with_instruction_profile(instruction_profile)"));
    assert!(turn_factory.contains(".with_developer_block(\"mai runtime\""));
    assert!(turn_factory.contains(".with_user_context_block(\"mai config user\""));
    assert!(!turn_factory.contains("InstructionSnapshot::profile_base_override"));
    assert!(!turn_factory.contains("with_instruction_snapshot"));
}

#[test]
fn product_agent_id_is_submitted_as_the_canonical_thread_identity() {
    let agent_api = include_str!("runtime_agent_api.rs");

    assert!(agent_api.contains("let thread_id = agent_host::canonical_id(agent_id)?"));
    assert!(agent_api.contains("AgentSubmitRequest::start(thread_id, message)"));
    assert!(!agent_api.contains("runtime_agent_id"));
}

#[test]
fn framework_retirement_is_followed_by_product_record_purge() {
    let delete = include_str!("agents/delete.rs");
    let runtime_ports = include_str!("runtime_agent_traits.rs");

    assert!(runtime_ports.contains(".retire(agent_host::canonical_id(agent_id)?)"));
    assert!(!runtime_ports.contains(".close(agent_host::canonical_id(agent_id)?)"));
    assert!(runtime_ports.contains("AgentRuntimeError::NotFound"));
    assert!(delete.contains("CanonicalAgentClose::Closed => purge_agent_tree"));
    assert!(delete.contains("CanonicalAgentClose::Missing => rollback_unregistered_agent"));
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
