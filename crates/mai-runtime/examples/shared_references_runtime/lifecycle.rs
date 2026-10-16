//! `lifecycle` 场景：真实 `spawn_agent` 生成 child/grandchild、继承 project A 与参考源、
//! 关闭后重建 Runtime 复用同一物理容器。
//!
//! 关键约束：
//! - child 只能通过**真实注册的 `spawn_agent` 工具**产生：权限判定走产品 Profile 能力
//!   （`spawn_agents`），不绕过 PL actor 直接写库；
//! - project_id 继承来自父 Agent 的可信 `project_id`（不是从父容器 inspect 猜）；
//! - child 的初始消息就是本驱动的 probe，因此“新 child 能读 A 私有夹具”由真实工具 trace
//!   证明，而不是由容器挂载或数据库字段推断。
//!
//! 唯一在 Runtime 启动前做的持久化改动：给父 Agent 冻结被授权 Profile（`executor`）。
//! 夹具 Agent 原本 `profile_id = None`，而协作能力必须来自冻结 Profile，无法通过
//! `UpdateAgentRequest` 设置；这里显式记录该前置改动，其余字段原样保留。

use mai_protocol::AgentId;
use std::sync::Arc;

use serde_json::json;

use crate::collaboration::{
    PARENT_PROFILE, assert_inheritance, ensure_parent_profile, extract_agent_id, first_line,
    spawned_children,
};
use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::{self, RuntimeHost};
use crate::probe::{self, Probe, ProbeCall};
use crate::runtime_config::{DriverConfig, LifecyclePhase};

/// `lifecycle` 命令入口。
pub async fn run(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    match config.lifecycle_phase {
        LifecyclePhase::Spawn => {
            let host = spawn_and_prove(config, evidence).await?;
            host.shutdown().await;
            Ok(())
        }
        LifecyclePhase::Resume => resume_phase(config, evidence).await,
        LifecyclePhase::Full => {
            let host = spawn_and_prove(config, evidence).await?;
            let children = spawned_children(&host.store).await?;
            if children.is_empty() {
                return Err(error(
                    "spawned_child_missing",
                    "spawn 阶段结束后产品库里没有新 child，无法验证复用",
                ));
            }
            prove_reuse(config, host, evidence, &children).await
        }
    }
}

/// 冻结父 Agent 的 Profile、真实 spawn child/grandchild、关闭 child。
async fn spawn_and_prove(config: &DriverConfig, evidence: &mut Evidence) -> Result<RuntimeHost> {
    let store = host::open_store(config).await?;
    let maintainer = fixtures::agent_id(fixtures::AGENT_MAINTAINER);
    let frozen = ensure_parent_profile(&store, maintainer).await?;
    evidence.fact(
        "parent_profile",
        json!({ "agent_id": maintainer, "profile_id": PARENT_PROFILE, "frozen_now": frozen }),
    );
    if frozen {
        evidence.limitation(format!(
            "{} 原本没有冻结 Profile；本驱动在 Runtime 启动前把它设为 `{PARENT_PROFILE}`（协作能力必须来自 Profile，UpdateAgentRequest 不提供该字段）",
            fixtures::AGENT_MAINTAINER
        ));
    }
    let runtime = host::start(config, Arc::clone(&store)).await?;
    evidence.fact("provider", runtime.configure_models().await?);
    scenario(config, &runtime, evidence).await?;
    Ok(runtime)
}

async fn scenario(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
) -> Result<()> {
    let tag = probe::run_tag();
    let project_a = fixtures::project_configuration(&host.store, ProjectKind::A).await?;
    fixtures::ensure_project_a_contract(&project_a)?;
    let private_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
    let maintainer = fixtures::agent_id(fixtures::AGENT_MAINTAINER);
    let summary = host.agent(maintainer).await?;
    evidence.check(
        "lifecycle.parent_profile_frozen",
        summary.profile_id.as_deref() == Some(PARENT_PROFILE),
        format!(
            "{} 的冻结 Profile 是 {PARENT_PROFILE}（spawn_agents 能力由 Profile 决定）",
            summary.name
        ),
        format!(
            "{} 的冻结 Profile 是 {:?}，spawn_agent 会因缺少能力被拒绝",
            summary.name, summary.profile_id
        ),
    );
    evidence.check(
        "lifecycle.parent_project",
        summary.project_id == Some(ProjectKind::A.id()),
        "父 Agent 属于 project A（child 会继承这个可信 project_id）",
        format!("父 Agent 的 project_id 是 {:?}", summary.project_id),
    );

    // 1) 真实 spawn child：child 的初始消息就是读 A 私有夹具的 probe。
    let child_probe = Probe::new(
        format!("child-initial-{tag}"),
        vec![ProbeCall::new(
            "read_file",
            json!({ "path": format!("{private_target}/README.md") }),
        )],
    );
    let spawn_child = Probe::new(
        format!("lifecycle-spawn-child-{tag}"),
        vec![ProbeCall::new(
            "spawn_agent",
            json!({
                "profileId": PARENT_PROFILE,
                "taskSummary": "read project A private reference",
                "message": child_probe.message()?,
            }),
        )],
    );
    let spawn_outcome = probe::run(host, maintainer, &spawn_child, config.turn_timeout).await?;
    evidence.fact("probe.spawn_child", json!(&spawn_outcome));
    let child_receipt = spawn_outcome
        .call(0)
        .map(|call| call.output.clone())
        .unwrap_or_default();
    let spawn_ok = spawn_outcome
        .call(0)
        .is_some_and(|call| call.observed && call.success == Some(true));
    evidence.check(
        "lifecycle.spawn_child_tool",
        spawn_ok,
        format!("真实 spawn_agent 成功：{}", first_line(&child_receipt)),
        format!(
            "真实 spawn_agent 失败（observed={:?}）：{}",
            spawn_outcome.call(0).map(|call| call.observed),
            first_line(&child_receipt)
        ),
    );
    let Some(child) = extract_agent_id(&child_receipt) else {
        evidence.blocked(
            "lifecycle.child_receipt",
            format!(
                "无法从 spawn 回执里解析 child 身份：{}",
                first_line(&child_receipt)
            ),
        );
        return Ok(());
    };
    evidence.fact(
        "child",
        json!({ "agent_id": child, "receipt": child_receipt }),
    );

    // 2) child 继承 project A 且能读 A 私有夹具（真实工具 trace）。
    let child_summary = host.agent(child).await?;
    assert_inheritance(evidence, host, &child_summary, maintainer, "child").await?;
    let child_read = probe::collect_latest(host, child, &child_probe, config.turn_timeout).await?;
    evidence.fact("probe.child_read", json!(&child_read));
    evidence.check(
        "lifecycle.child_reads_project_a_reference",
        child_read
            .call(0)
            .is_some_and(|call| call.observed && call.success == Some(true)),
        format!("新 child 通过真实 read_file 读到 {private_target}/README.md"),
        format!(
            "新 child 读不到 A 私有夹具：{:?}",
            child_read
                .calls
                .iter()
                .map(|call| (call.observed, call.success))
                .collect::<Vec<_>>()
        ),
    );

    // 3) child 再 spawn grandchild（能力同样来自冻结 Profile）。
    let grandchild_probe = Probe::new(
        format!("grandchild-initial-{tag}"),
        vec![ProbeCall::new(
            "read_file",
            json!({ "path": format!("{private_target}/src/fixture.c") }),
        )],
    );
    let spawn_grandchild = Probe::new(
        format!("lifecycle-spawn-grandchild-{tag}"),
        vec![ProbeCall::new(
            "spawn_agent",
            json!({
                "profileId": PARENT_PROFILE,
                "taskSummary": "read project A private reference",
                "message": grandchild_probe.message()?,
            }),
        )],
    );
    let grandchild_outcome =
        probe::run(host, child, &spawn_grandchild, config.turn_timeout).await?;
    evidence.fact("probe.spawn_grandchild", json!(&grandchild_outcome));
    let grandchild_receipt = grandchild_outcome
        .call(0)
        .map(|call| call.output.clone())
        .unwrap_or_default();
    evidence.check(
        "lifecycle.spawn_grandchild_tool",
        grandchild_outcome
            .call(0)
            .is_some_and(|call| call.observed && call.success == Some(true)),
        format!(
            "child 通过真实 spawn_agent 产生 grandchild：{}",
            first_line(&grandchild_receipt)
        ),
        format!("grandchild 创建失败：{}", first_line(&grandchild_receipt)),
    );
    let Some(grandchild) = extract_agent_id(&grandchild_receipt) else {
        evidence.blocked(
            "lifecycle.grandchild_receipt",
            format!(
                "无法从 grandchild 回执里解析身份：{}",
                first_line(&grandchild_receipt)
            ),
        );
        return Ok(());
    };
    let grandchild_summary = host.agent(grandchild).await?;
    assert_inheritance(evidence, host, &grandchild_summary, child, "grandchild").await?;
    let grandchild_read =
        probe::collect_latest(host, grandchild, &grandchild_probe, config.turn_timeout).await?;
    evidence.fact("probe.grandchild_read", json!(&grandchild_read));
    evidence.check(
        "lifecycle.grandchild_reads_project_a_reference",
        grandchild_read
            .call(0)
            .is_some_and(|call| call.observed && call.success == Some(true)),
        format!("grandchild 通过真实 read_file 读到 {private_target}/src/fixture.c"),
        "grandchild 读不到 A 私有夹具".to_string(),
    );

    // 4) 关闭 child（真实 close_agent，保留工作区），验证关闭路径本身可用。
    let close = Probe::new(
        format!("lifecycle-close-{tag}"),
        vec![ProbeCall::new(
            "close_agent",
            json!({ "target": child.to_string() }),
        )],
    );
    let close_outcome = probe::run(host, maintainer, &close, config.turn_timeout).await?;
    evidence.fact("probe.close_child", json!(&close_outcome));
    evidence.check(
        "lifecycle.close_child_tool",
        close_outcome
            .call(0)
            .is_some_and(|call| call.observed && call.success == Some(true)),
        "真实 close_agent 关闭 child 且保留工作区",
        format!(
            "close_agent 失败：{}",
            close_outcome
                .call(0)
                .map(|call| first_line(&call.output_preview))
                .unwrap_or_default()
        ),
    );
    Ok(())
}

/// 独立进程的 `resume` 阶段：只重建 Runtime 并验证 child 复用与可读性。
async fn resume_phase(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    let store = host::open_store(config).await?;
    let children = spawned_children(&store).await?;
    if children.is_empty() {
        return Err(error(
            "spawned_child_missing",
            "产品库里没有之前 spawn 出来的 child；请先运行 `lifecycle --phase spawn`",
        ));
    }
    let runtime = host::start(config, Arc::clone(&store)).await?;
    evidence.fact("provider", runtime.configure_models().await?);
    prove_reuse(config, runtime, evidence, &children).await
}

/// 关闭后重建 Runtime：证明复用同一物理容器、孤儿清理不误删、新 turn 仍能读参考源。
async fn prove_reuse(
    config: &DriverConfig,
    host: RuntimeHost,
    evidence: &mut Evidence,
    children: &[AgentId],
) -> Result<()> {
    let private_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
    let child = children[0];
    let before = host.container_id(child).await?;
    let binding_before = host.store.reference_binding(child).await.map_err(|cause| {
        error(
            "binding_unreadable",
            format!("读取 child 绑定失败：{cause}"),
        )
    })?;
    evidence.fact(
        "reuse.before",
        json!({
            "child": child,
            "container_id": &before,
            "generation": binding_before.as_ref().map(|binding| binding.generation),
            "applied_revision": binding_before.as_ref().map(|binding| binding.applied_revision),
        }),
    );
    let cleaned = host
        .runtime
        .cleanup_orphaned_containers()
        .await
        .map_err(|cause| error("cleanup_failed", format!("孤儿清理失败：{cause}")))?;
    evidence.fact("orphan_cleanup", json!(cleaned));
    evidence.check(
        "lifecycle.orphan_cleanup_keeps_child",
        before.as_ref().is_none_or(|id| !cleaned.contains(id)),
        "孤儿清理没有删除 child 的执行环境",
        "孤儿清理删除了正在使用的 child 容器",
    );

    // 真实“重启”：关闭驻留 Thread，再用同一个产品库新建 Runtime 实例。
    host.shutdown().await;
    let store = Arc::clone(&host.store);
    drop(host);
    let resumed = host::start(config, Arc::clone(&store)).await?;
    resumed.configure_models().await?;

    let after = resumed.container_id(child).await?;
    let binding_after = store.reference_binding(child).await.map_err(|cause| {
        error(
            "binding_unreadable",
            format!("读取 child 绑定失败：{cause}"),
        )
    })?;
    evidence.fact(
        "reuse.after",
        json!({
            "container_id": &after,
            "generation": binding_after.as_ref().map(|binding| binding.generation),
            "applied_revision": binding_after.as_ref().map(|binding| binding.applied_revision),
        }),
    );
    evidence.check(
        "lifecycle.container_reused_after_restart",
        after.is_some() && after == before,
        format!("重建 Runtime 后 child 仍复用同一物理容器 {after:?}"),
        format!("child 容器在重建后变化：{before:?} -> {after:?}"),
    );
    evidence.check(
        "lifecycle.binding_generation_stable",
        binding_before.as_ref().map(|binding| binding.generation)
            == binding_after.as_ref().map(|binding| binding.generation),
        "没有因为重启产生多余的换代",
        "重建 Runtime 引入了新的 generation，说明环境被无谓重建",
    );

    // 重建后的 child 仍能读 A 私有夹具（真实工具），说明 lease 与挂载都恢复了。
    let probe_after = Probe::new(
        format!("lifecycle-resume-read-{}", probe::run_tag()),
        vec![ProbeCall::new(
            "read_file",
            json!({ "path": format!("{private_target}/README.md") }),
        )],
    );
    let outcome = probe::run(&resumed, child, &probe_after, config.turn_timeout).await?;
    evidence.fact("probe.resume_read", json!(&outcome));
    evidence.check(
        "lifecycle.resumed_child_reads_reference",
        outcome
            .call(0)
            .is_some_and(|call| call.observed && call.success == Some(true)),
        "重建 Runtime 后 child 的新 turn 仍能读 A 私有夹具",
        format!(
            "重建 Runtime 后 child 读不到参考源：state={} calls={:?}",
            outcome.turn_state,
            outcome
                .calls
                .iter()
                .map(|call| (call.observed, call.success))
                .collect::<Vec<_>>()
        ),
    );
    resumed.shutdown().await;
    Ok(())
}
