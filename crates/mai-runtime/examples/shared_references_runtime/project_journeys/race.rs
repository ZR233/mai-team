//! T29：在真实项目上并发触发引用应用、协作 spawn 与子树删除。
//!
//! 本场景只作用于 T18 创建的临时项目，并始终在退出前还原原始引用配置。
//! child/grandchild 只能由已注册的 `spawn_agent` 工具产生；删除、引用替换与应用都走
//! `AgentRuntime` 公开入口。当前主集成树没有公开 `recover_agent`，因此该竞争面显式记录
//! boundary，不用 `get_agent`、重建 Runtime 或直写存储伪装恢复。

#[path = "race/concurrency.rs"]
mod concurrency;
#[path = "race/invariants.rs"]
mod invariants;
#[path = "race/references.rs"]
mod references;
#[path = "race/support.rs"]
mod support;
#[path = "race/verification.rs"]
mod verification;

use mai_protocol::ProjectReferenceConfiguration;
use serde_json::json;
use std::time::Instant;

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe;

use super::support::ProjectHandles;
use concurrency::CreatedAgents;

/// T29 场景入口。主体失败时仍尝试清理已创建子树并还原项目引用。
pub async fn scenario(
    host: &RuntimeHost,
    handles: &ProjectHandles,
    evidence: &mut Evidence,
) -> Result<()> {
    let original = references::configuration(host, handles.project).await?;
    evidence.fact("t29.original_configuration", json!(&original));
    verification::record_recover_api_boundary(evidence);

    let fixed_before = invariants::fixed_project_snapshot(host).await?;
    evidence.fact("t29.fixed_projects_before", json!(&fixed_before));

    let mut created = CreatedAgents::default();
    let executed = execute(host, handles, &original, &mut created, evidence).await;
    let cleaned = cleanup_created(host, &created, evidence).await;
    let restored_revision = references::restore(host, handles, &original, evidence).await;
    let restored = restored_revision.clone().map(|_| ());
    let expected_revision = restored_revision.unwrap_or(original.revision);
    let verified = verification::final_state(
        host,
        handles,
        &original,
        expected_revision,
        &created,
        &fixed_before,
        evidence,
    )
    .await;

    evidence.fact(
        "t29.outcomes",
        json!({
            "execute": format!("{executed:?}"),
            "cleanup": format!("{cleaned:?}"),
            "restore": format!("{restored:?}"),
            "verify": format!("{verified:?}"),
        }),
    );
    executed?;
    cleaned?;
    restored?;
    verified?;
    evidence.pass(
        "t29.feasible_concurrency_completed",
        "真实 spawn/apply/delete 竞争已收敛，原引用和固定 A/B 项目事实保持不变",
    );
    Ok(())
}

/// 主体流程。`created` 记录已经确认存在的产品 child，便于失败路径统一清理。
async fn execute(
    host: &RuntimeHost,
    handles: &ProjectHandles,
    original: &ProjectReferenceConfiguration,
    created: &mut CreatedAgents,
    evidence: &mut Evidence,
) -> Result<()> {
    let tag = probe::run_tag();
    let first_alias = format!(
        "private-fixture-t29-a-{}",
        tag.rsplit('-').next().expect("验收 tag 包含随机后缀")
    );
    let first_target = crate::fixtures::container_path(&first_alias);
    let first_bindings =
        references::retarget(original, crate::fixtures::ALIAS_PRIVATE, &first_alias)?;
    let maintainer = support::bounded(
        host,
        "t29_maintainer_summary",
        host.agent(handles.maintainer),
    )
    .await?;
    evidence.fact(
        "t29.maintainer_profile",
        json!({
            "profile_id": maintainer.profile_id,
            "project_id": maintainer.project_id,
        }),
    );
    if maintainer.project_id != Some(handles.project) {
        return Err(error(
            "t29_maintainer_project_invalid",
            "临时项目 Maintainer 的可信 project_id 不匹配",
        ));
    }
    if maintainer.profile_id.as_deref() != Some("project-maintainer") {
        return Err(error(
            "t29_maintainer_profile_invalid",
            "临时项目 Maintainer 没有冻结 project-maintainer Profile，spawn_agent 不能放行",
        ));
    }

    // 第一段：Maintainer 的活动 Turn 先取得旧执行租约；CAS 保存新目标并申请 WhenIdle
    // 应用后，provider 延迟结束才执行真实 spawn_agent。
    let child_initial = probe::Probe::new(
        format!("t29-child-initial-{tag}"),
        vec![probe::ProbeCall::new(
            "read_file",
            json!({ "path": format!("{first_target}/README.md") }),
        )],
    );
    let parent_spawn = probe::Probe::new(
        format!("t29-spawn-child-{tag}"),
        vec![probe::ProbeCall::new(
            "spawn_agent",
            json!({
                "profileId": support::EXECUTOR_PROFILE,
                "taskSummary": "T29 race child",
                "message": child_initial.message()?,
            }),
        )],
    )
    .with_provider_delay(support::PROVIDER_DELAY_SECS);

    let parent_window_started = Instant::now();
    let parent_input = support::bounded(
        host,
        "t29_parent_turn_submit",
        probe::send(host, handles.maintainer, &parent_spawn),
    )
    .await?;
    let active = probe::watch(
        host,
        handles.maintainer,
        &parent_input,
        probe::Watch::Active,
        support::turn_limit(host),
    )
    .await?;
    evidence.pass(
        "t29.parent_turn_active_before_reference_change",
        format!(
            "Maintainer Turn {} 已进入 running（state={}）后才开始替换引用",
            active
                .turn
                .as_ref()
                .map(|turn| turn.id.clone())
                .unwrap_or_default(),
            active.state
        ),
    );
    if parent_window_started.elapsed()
        >= std::time::Duration::from_secs(support::PROVIDER_DELAY_SECS)
    {
        return Err(error(
            "t29_reference_change_missed_provider_window",
            "第一段提交/观察未能在 provider 延迟窗口内进入 running，不能证明 CAS/apply 与活动 Turn 并发",
        ));
    }

    let first =
        references::save_and_apply(host, handles.project, original.revision, first_bindings)
            .await?;
    if parent_window_started.elapsed()
        >= std::time::Duration::from_secs(support::PROVIDER_DELAY_SECS)
    {
        return Err(error(
            "t29_reference_save_missed_provider_window",
            "第一段 CAS/apply 申请未在 provider 发出 spawn_agent 前完成，child 初次目标不可证明",
        ));
    }
    evidence.fact(
        "t29.first_reference_request",
        json!({
            "configuration": &first.configuration,
            "initial_status": &first.initial_status,
        }),
    );
    let parent_watched = probe::watch(
        host,
        handles.maintainer,
        &parent_input,
        probe::Watch::Terminal,
        support::turn_limit(host),
    )
    .await?;
    let parent_outcome = support::bounded(
        host,
        "t29_parent_spawn_collect",
        probe::collect(
            host,
            handles.maintainer,
            &parent_spawn,
            &parent_input,
            parent_watched,
        ),
    )
    .await?;
    evidence.fact("t29.parent_spawn_turn", json!(&parent_outcome));
    verification::assert_spawn_success(
        evidence,
        "t29.parent_spawn_agent_tool",
        &parent_outcome,
        "真实 spawn_agent 已在活动 Turn 后执行",
    )?;

    let child = support::agent_id_from_receipt(
        parent_outcome
            .call(0)
            .map(|call| call.output.as_str())
            .unwrap_or_default(),
    )
    .ok_or_else(|| error("t29_child_receipt_missing", "spawn_agent 回执缺少 agentId"))?;
    created.child = Some(child);
    verification::record_spawned_agent(
        host,
        handles,
        child,
        handles.maintainer,
        &first_alias,
        "child",
        evidence,
    )
    .await?;
    let child_read = support::bounded(
        host,
        "t29_child_initial_collect",
        probe::collect_latest(host, child, &child_initial, support::turn_limit(host)),
    )
    .await?;
    evidence.fact("t29.child_initial_read", json!(&child_read));
    verification::assert_initial_read(
        evidence,
        "t29.child_initial_tool_read",
        &child_read,
        &first_target,
    )?;
    references::wait_applied(
        host,
        handles.project,
        first.configuration.revision,
        evidence,
    )
    .await?;
    verification::assert_single_owner(
        host,
        handles,
        child,
        first.configuration.revision,
        &first_target,
        "t29.child",
        evidence,
    )
    .await?;

    // 第二段：先证明 child 能通过自己的合法 executor Profile 真实 spawn grandchild，
    // 再让一个新的 child Turn 被 provider hold，同时执行子树删除与第二个引用应用。
    let grandchild_initial = probe::Probe::new(
        format!("t29-grandchild-initial-{tag}"),
        vec![probe::ProbeCall::new(
            "read_file",
            json!({ "path": format!("{first_target}/src/fixture.c") }),
        )],
    );
    let child_spawn = probe::Probe::new(
        format!("t29-spawn-grandchild-{tag}"),
        vec![probe::ProbeCall::new(
            "spawn_agent",
            json!({
                "profileId": support::EXECUTOR_PROFILE,
                "taskSummary": "T29 race grandchild",
                "message": grandchild_initial.message()?,
            }),
        )],
    );
    let child_spawn_outcome = support::bounded(
        host,
        "t29_grandchild_spawn_turn",
        probe::run(host, child, &child_spawn, support::turn_limit(host)),
    )
    .await?;
    evidence.fact("t29.child_spawn_turn", json!(&child_spawn_outcome));
    verification::assert_spawn_success(
        evidence,
        "t29.child_spawn_agent_tool",
        &child_spawn_outcome,
        "child 的合法 executor Profile 真实生成了 grandchild",
    )?;
    let grandchild = support::agent_id_from_receipt(
        child_spawn_outcome
            .call(0)
            .map(|call| call.output.as_str())
            .unwrap_or_default(),
    )
    .ok_or_else(|| {
        error(
            "t29_grandchild_receipt_missing",
            "grandchild spawn_agent 回执缺少 agentId",
        )
    })?;
    created.grandchild = Some(grandchild);
    verification::record_spawned_agent(
        host,
        handles,
        grandchild,
        child,
        &first_alias,
        "grandchild",
        evidence,
    )
    .await?;
    let grandchild_read = support::bounded(
        host,
        "t29_grandchild_initial_collect",
        probe::collect_latest(
            host,
            grandchild,
            &grandchild_initial,
            support::turn_limit(host),
        ),
    )
    .await?;
    evidence.fact("t29.grandchild_initial_read", json!(&grandchild_read));
    verification::assert_initial_read(
        evidence,
        "t29.grandchild_initial_tool_read",
        &grandchild_read,
        &first_target,
    )?;

    let held = probe::Probe::new(
        format!("t29-child-held-{tag}"),
        vec![probe::ProbeCall::new(
            "read_file",
            json!({ "path": format!("{first_target}/README.md") }),
        )],
    )
    .with_provider_delay(support::PROVIDER_DELAY_SECS);
    let child_window_started = Instant::now();
    let child_input = support::bounded(
        host,
        "t29_child_turn_submit",
        probe::send(host, child, &held),
    )
    .await?;
    let child_active = probe::watch(
        host,
        child,
        &child_input,
        probe::Watch::Active,
        support::turn_limit(host),
    )
    .await?;
    evidence.pass(
        "t29.child_turn_active_before_delete_and_apply",
        format!(
            "child Turn 已进入 running（{}）后同时触发删除与应用",
            child_active.state
        ),
    );
    if child_window_started.elapsed()
        >= std::time::Duration::from_secs(support::PROVIDER_DELAY_SECS)
    {
        return Err(error(
            "t29_delete_apply_missed_provider_window",
            "第二段提交/观察未能在 provider 延迟窗口内进入 running，不能证明删除与应用并发",
        ));
    }

    let second_alias = format!(
        "private-fixture-t29-b-{}",
        tag.rsplit('-').next().expect("验收 tag 包含随机后缀")
    );
    let second_target = crate::fixtures::container_path(&second_alias);
    let second_bindings = references::retarget(&first.configuration, &first_alias, &second_alias)?;
    let race = concurrency::delete_apply_and_watch(
        host,
        handles.project,
        first.configuration.revision,
        second_bindings,
        child,
        &child_input,
        support::operation_limit(host),
    )
    .await;
    evidence.fact("t29.delete_apply_race", json!(&race));
    race.deleted_result()?;
    let second = race.reference_result()?;
    references::wait_applied(
        host,
        handles.project,
        second.configuration.revision,
        evidence,
    )
    .await?;
    verification::assert_deleted_turn(evidence, &race.watch, "t29.child_delete_race_terminal")?;
    evidence.fact(
        "t29.second_target",
        json!({
            "alias": second_alias,
            "target": second_target,
            "revision": second.configuration.revision,
        }),
    );
    Ok(())
}

/// 失败路径也只通过 Runtime 删除已确认创建的子树，不直写存储。
async fn cleanup_created(
    host: &RuntimeHost,
    created: &CreatedAgents,
    evidence: &mut Evidence,
) -> Result<()> {
    let agents =
        support::bounded_value(host, "t29_cleanup_agent_list", host.runtime.list_agents()).await;
    let agents = agents?;
    let child_exists = created
        .child
        .is_some_and(|child| agents.iter().any(|summary| summary.id == child));
    let target = if child_exists {
        created.child
    } else {
        created
            .grandchild
            .filter(|grandchild| agents.iter().any(|summary| summary.id == *grandchild))
    };
    let Some(target) = target else {
        evidence.fact(
            "t29.cleanup_already_deleted",
            json!({
                "child": created.child,
                "grandchild": created.grandchild,
            }),
        );
        return Ok(());
    };
    support::bounded(
        host,
        "t29_failure_cleanup",
        host.runtime.delete_agent(target),
    )
    .await?;
    evidence.pass(
        "t29.failure_cleanup",
        format!("失败路径已通过 Runtime 删除残留子树根 {target}"),
    );
    Ok(())
}
