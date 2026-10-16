//! T29 的物理事实与固定项目不变量核验。

use mai_protocol::{AgentId, AgentResourceState, ProjectReferenceConfiguration};
use serde_json::json;

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe::ProbeOutcome;

use super::concurrency::{CreatedAgents, TurnRaceWatch};
use super::invariants::FixedProjects;
use super::support;

/// 读取一个持久绑定，带 T29 单步超时。
async fn agent_binding(
    host: &RuntimeHost,
    agent_id: AgentId,
) -> Result<Option<mai_store::PersistedReferenceBinding>> {
    support::bounded(
        host,
        "t29_reference_binding",
        host.store.reference_binding(agent_id),
    )
    .await
}

/// 读取受管容器索引，带 T29 单步超时。
async fn managed_containers(host: &RuntimeHost) -> Result<Vec<mai_docker::ManagedContainer>> {
    support::bounded(
        host,
        "t29_list_managed_containers",
        host.docker.list_managed_containers(),
    )
    .await
}

/// 当前主集成树没有公开 recover_agent；不能用 get_agent 冒充恢复。
pub(super) fn record_recover_api_boundary(evidence: &mut Evidence) {
    evidence.boundary(
        "t29.recover_agent_public_api_missing",
        "主集成树 AgentRuntime 只有内部 recover_project_agent_resources，没有公开 recover_agent(parent/child)；本文件不构造平行恢复路径",
    );
    evidence.limitation(
        "T29 的 recover 竞争面因公共 API 缺失未执行；本场景只覆盖真实 spawn/apply/delete/watch 并发",
    );
}

/// 断言真实 spawn_agent 工具成功。
pub(super) fn assert_spawn_success(
    evidence: &mut Evidence,
    id: &str,
    outcome: &ProbeOutcome,
    pass: &str,
) -> Result<()> {
    let ok = outcome
        .call(0)
        .is_some_and(|call| call.observed && call.success == Some(true));
    evidence.check(
        id,
        ok,
        pass.to_string(),
        format!(
            "spawn_agent 没有真实成功：state={} calls={:?}",
            outcome.turn_state,
            outcome
                .calls
                .iter()
                .map(|call| (call.name.as_str(), call.observed, call.success))
                .collect::<Vec<_>>()
        ),
    );
    if ok {
        Ok(())
    } else {
        Err(error(id, "真实 spawn_agent 失败，后续竞争不能继续"))
    }
}

/// 记录 child/grandchild 的可信归属、Profile 与最新挂载。
pub(super) async fn record_spawned_agent(
    host: &RuntimeHost,
    handles: &super::super::support::ProjectHandles,
    agent_id: AgentId,
    parent: AgentId,
    alias: &str,
    label: &str,
    evidence: &mut Evidence,
) -> Result<()> {
    let target = crate::fixtures::container_path(alias);
    let summary = support::bounded(host, "t29_agent_summary", host.agent(agent_id)).await?;
    evidence.fact(
        format!("t29.{label}_summary"),
        json!({
            "agent_id": summary.id,
            "parent_id": summary.parent_id,
            "project_id": summary.project_id,
            "profile_id": summary.profile_id,
            "resource_state": summary.resource.state,
            "expected_alias": alias,
        }),
    );
    let valid = summary.parent_id == Some(parent)
        && summary.project_id == Some(handles.project)
        && summary.profile_id.as_deref() == Some(super::support::EXECUTOR_PROFILE);
    evidence.check(
        &format!("t29.{label}_trusted_project_and_profile"),
        valid,
        format!("{label} 从可信 project_id 继承项目并冻结 executor Profile"),
        format!(
            "{label} 归属或 Profile 不符：parent={:?} project={:?} profile={:?}",
            summary.parent_id, summary.project_id, summary.profile_id
        ),
    );
    if !valid {
        return Err(error(
            "t29_spawned_agent_identity_invalid",
            format!("{label} 的项目归属或 Profile 不符合产品事实"),
        ));
    }
    let targets = support::bounded(
        host,
        "t29_reference_targets",
        host.reference_targets(agent_id),
    )
    .await?;
    let target_present = targets.iter().any(|item| item == &target);
    evidence.check(
        &format!("t29.{label}_latest_reference_target"),
        target_present,
        format!("{label} 的实际挂载包含最新目标 {target}"),
        format!("{label} 缺少最新目标 {target}：{targets:?}"),
    );
    if target_present {
        Ok(())
    } else {
        Err(error(
            "t29_spawned_agent_target_missing",
            format!("{label} 的实际执行环境没有最新引用目标"),
        ))
    }
}

/// 断言初始消息中的真实 read_file 成功。
pub(super) fn assert_initial_read(
    evidence: &mut Evidence,
    id: &str,
    outcome: &ProbeOutcome,
    target: &str,
) -> Result<()> {
    let ok = outcome
        .call(0)
        .is_some_and(|call| call.observed && call.success == Some(true));
    evidence.check(
        id,
        ok,
        format!("初始 Turn 通过真实 read_file 读取 {target}"),
        format!(
            "初始真实读取失败：state={} calls={:?}",
            outcome.turn_state,
            outcome
                .calls
                .iter()
                .map(|call| (call.observed, call.success))
                .collect::<Vec<_>>()
        ),
    );
    if ok {
        Ok(())
    } else {
        Err(error(id, "初始真实 read_file 失败"))
    }
}

/// 核验某个 Agent 在物理 Docker 中只有一个执行容器且绑定匹配目标修订。
pub(super) async fn assert_single_owner(
    host: &RuntimeHost,
    handles: &super::super::support::ProjectHandles,
    agent_id: AgentId,
    revision: u64,
    target: &str,
    prefix: &str,
    evidence: &mut Evidence,
) -> Result<()> {
    let containers = managed_containers(host).await?;
    let owned: Vec<_> = containers
        .iter()
        .filter(|container| {
            !container.sidecar
                && container.agent_id.as_deref() == Some(agent_id.to_string().as_str())
        })
        .collect();
    let binding = agent_binding(host, agent_id).await?;
    let summary = support::bounded(host, "t29_agent_summary", host.agent(agent_id)).await?;
    let unique = owned.len() == 1
        && owned[0].project_id.as_deref() == Some(handles.project.to_string().as_str())
        && owned[0].generation.is_some()
        && binding
            .as_ref()
            .is_some_and(|binding| binding.container_id == owned[0].id)
        && summary.container_id.as_deref() == Some(owned[0].id.as_str());
    evidence.check(
        &format!("{prefix}.single_physical_owner"),
        unique,
        format!("Agent {agent_id} 只有一个受管执行容器且与产品绑定一致"),
        format!(
            "Agent {agent_id} 物理所有者不唯一或不一致：containers={owned:?} binding={binding:?} summary_container={:?}",
            summary.container_id
        ),
    );
    let binding_revision = binding
        .as_ref()
        .map(|binding| binding.applied_revision)
        .is_some_and(|applied| applied == revision);
    evidence.check(
        &format!("{prefix}.binding_matches_latest_target"),
        binding_revision,
        format!("Agent {agent_id} 的绑定指向目标修订 {revision}"),
        format!("Agent {agent_id} 绑定修订不匹配：{binding:?}"),
    );
    if !unique || owned.is_empty() {
        return Err(error(
            "t29_physical_owner_invalid",
            format!("Agent {agent_id} 的物理所有者数量或身份不满足 T29"),
        ));
    }
    let facts = support::bounded(
        host,
        "t29_container_facts",
        host.container_facts(&owned[0].id),
    )
    .await?;
    let targets = crate::host::reference_targets_of(&facts);
    evidence.check(
        &format!("{prefix}.container_matches_latest_target"),
        targets.iter().any(|item| item == target),
        format!("Agent {agent_id} 实际挂载包含 {target}"),
        format!("Agent {agent_id} 实际挂载缺少 {target}：{targets:?}"),
    );
    if !(unique && binding_revision && targets.iter().any(|item| item == target)) {
        return Err(error(
            "t29_physical_owner_invalid",
            format!("Agent {agent_id} 的物理所有者或目标修订不满足 T29"),
        ));
    }
    Ok(())
}

/// 接受 Completed、Cancelled，或删除已成功且产品明确返回 AgentNotFound。
/// 整体场景还必须核验 Agent、收据与物理容器都已清理，不能把任意观察错误当作成功。
pub(super) fn assert_deleted_turn(
    evidence: &mut Evidence,
    watch: &TurnRaceWatch,
    id: &str,
) -> Result<()> {
    let ok = (watch.terminal && (watch.completed || watch.cancelled))
        || (watch.disappeared_before_terminal
            && watch.last_state == "agent_not_found_after_delete");
    evidence.check(
        id,
        ok,
        format!(
            "child Turn 在删除竞争中进入明确终态 {}（completed={} cancelled={}）",
            watch.last_state, watch.completed, watch.cancelled
        ),
        format!(
            "child Turn 终态不可信：terminal={} state={} disappeared={} error={:?}",
            watch.terminal,
            watch.last_state,
            watch.disappeared_before_terminal,
            watch.observation_error
        ),
    );
    if ok {
        Ok(())
    } else {
        Err(error(id, "删除竞争没有留下可信的 Turn 终态"))
    }
}

/// 终态核验：原 alias 已应用、Maintainer Ready 且唯一、被删子树无残留、固定项目不变。
pub(super) async fn final_state(
    host: &RuntimeHost,
    handles: &super::super::support::ProjectHandles,
    original: &ProjectReferenceConfiguration,
    expected_revision: u64,
    created: &CreatedAgents,
    before: &FixedProjects,
    evidence: &mut Evidence,
) -> Result<()> {
    let status = support::bounded(
        host,
        "t29_final_reference_status",
        host.runtime.project_reference_status(
            handles.project,
            mai_protocol::ReferenceStatusQuery::default(),
        ),
    )
    .await?;
    let maintainer = support::bounded(
        host,
        "t29_final_agent_summary",
        host.agent(handles.maintainer),
    )
    .await?;
    let binding = agent_binding(host, handles.maintainer).await?;
    let Some(binding) = binding else {
        return Err(error(
            "t29_final_binding_missing",
            "还原后 Maintainer 没有物理绑定",
        ));
    };
    let Some(container_id) = support::bounded(
        host,
        "t29_final_container_id",
        host.container_id(handles.maintainer),
    )
    .await?
    else {
        return Err(error(
            "t29_final_container_missing",
            "还原后 Maintainer 没有容器",
        ));
    };
    let facts = support::bounded(
        host,
        "t29_final_container_facts",
        host.container_facts(&container_id),
    )
    .await?;
    let targets = crate::host::reference_targets_of(&facts);
    let private_target = crate::fixtures::container_path(crate::fixtures::ALIAS_PRIVATE);
    let linux_target = crate::fixtures::container_path(crate::fixtures::ALIAS_LINUX);
    let maintainer_ready = maintainer.resource.state == AgentResourceState::Ready;
    let restored = status.configuration.revision == expected_revision
        && status.configuration.bindings == original.bindings
        && status.state == mai_protocol::ProjectReferenceApplicationState::Applied
        && binding.applied_revision == expected_revision
        && binding.container_id == container_id
        && facts.generation == Some(binding.generation)
        && targets.contains(&private_target)
        && targets.contains(&linux_target);
    evidence.check(
        "t29.restored_binding_and_physical_mounts",
        restored && maintainer_ready,
        format!(
            "Maintainer Ready={maintainer_ready}，原 alias/linux 挂载恢复，binding 与 Docker generation 一致"
        ),
        format!(
            "终态还原失败：maintainer_state={:?} status={} binding={binding:?} targets={targets:?}",
            maintainer.resource.state, status.state
        ),
    );

    let agents =
        support::bounded_value(host, "t29_final_agent_list", host.runtime.list_agents()).await?;
    let containers = managed_containers(host).await?;
    let deleted_ids = [created.child, created.grandchild];
    let records_absent = deleted_ids
        .iter()
        .flatten()
        .all(|id| !agents.iter().any(|summary| summary.id == *id));
    let deleted_containers_absent = !containers.iter().any(|container| {
        deleted_ids
            .iter()
            .flatten()
            .any(|id| container.agent_id.as_deref() == Some(id.to_string().as_str()))
    });
    let execution_containers: Vec<_> = containers
        .iter()
        .filter(|container| {
            !container.sidecar
                && container.project_id.as_deref() == Some(handles.project.to_string().as_str())
        })
        .collect();
    let maintainer_only = execution_containers.len() == 1
        && execution_containers[0].agent_id.as_deref()
            == Some(handles.maintainer.to_string().as_str());
    evidence.fact(
        "t29.final_project_containers",
        json!({
            "execution_containers": execution_containers,
            "deleted_records_absent": records_absent,
            "deleted_containers_absent": deleted_containers_absent,
            "maintainer_only": maintainer_only,
        }),
    );
    evidence.check(
        "t29.deleted_subtree_has_no_records_or_containers",
        records_absent && deleted_containers_absent,
        "被删 child/grandchild 没有产品记录、执行容器或 sidecar 残留".to_string(),
        format!(
            "删除子树有残留：records_absent={records_absent} containers_absent={deleted_containers_absent}"
        ),
    );
    evidence.check(
        "t29.maintainer_is_only_execution_container",
        maintainer_only,
        "临时项目只剩 Maintainer 的物理执行容器".to_string(),
        format!("临时项目执行容器不符合唯一 Maintainer：{execution_containers:?}"),
    );

    let after = super::invariants::fixed_project_snapshot(host).await?;
    evidence.fact("t29.fixed_projects_after", json!(&after));
    evidence.check(
        "t29.fixed_projects_unchanged",
        before == &after,
        "固定 A/B 项目配置、Agent generation 与受管容器事实保持不变".to_string(),
        format!("固定项目事实变化：before={before:?} after={after:?}"),
    );

    if !(restored
        && maintainer_ready
        && records_absent
        && deleted_containers_absent
        && maintainer_only
        && before == &after)
    {
        return Err(error(
            "t29_final_state_invalid",
            "T29 终态未同时满足原引用应用、Maintainer Ready/唯一、子树清理与固定项目不变",
        ));
    }
    evidence.pass(
        "t29.maintainer_ready_and_project_has_no_extra_container",
        "临时项目只剩 Maintainer 的物理执行容器且状态 Ready",
    );
    Ok(())
}
