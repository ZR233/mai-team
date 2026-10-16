//! `gate` 场景：活动 turn 期间的参考配置换代门禁。
//!
//! 覆盖顺序（`--gate-mode` 可裁剪）：
//! 1. WhenIdle：先起一个**真实活动**的 turn（provider 延迟 + 容器内 sleep），随后保存下一修订
//!    （改 A 私有夹具 alias ⇒ 真实挂载变化）并请求应用；断言旧 turn 的租约/真实挂载保持不变、
//!    新修订尚未 applied、排队消息在应用完成前不开始；收敛后排队 turn 必须看到新配置。
//! 2. StopAndApply：再起一个活动 turn，用 StopAndApply 还原夹具配置；断言活动 turn 被取消
//!    （不是 completed）且应用真实收敛。
//!
//! 最后把夹具配置还原为原始 alias，避免影响其它场景与后续运行。

use mai_protocol::{
    ApplyProjectReferencesRequest, ProjectReferenceApplicationState, ReferenceApplyPolicy,
    ReplaceProjectReferencesRequest,
};
use serde_json::{Value, json};

use crate::applied::{
    add_convergence_evidence, agent_state, assert_fixture_restored, assert_model_facts, renamed,
    restore_configuration, wait_for_applied,
};
use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::{self, RuntimeHost};
use crate::probe::{self, Probe, ProbeCall, Watch};
use crate::runtime_config::DriverConfig;

/// 换代 alias：只改 alias，目标路径随之变化，因而必须真实换代。
const GATED_ALIAS_SUFFIX: &str = "-gated";

/// `gate` 命令入口。
pub async fn run(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    let store = host::open_store(config).await?;
    let runtime = host::start(config, store).await?;
    evidence.fact("provider", runtime.configure_models().await?);
    let outcome = scenario(config, &runtime, evidence).await;
    runtime.shutdown().await;
    outcome
}

async fn scenario(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
) -> Result<()> {
    let tag = probe::run_tag();
    let project = ProjectKind::A;
    let project_id = project.id();
    let maintainer = fixtures::agent_id(fixtures::AGENT_MAINTAINER);
    let original = fixtures::project_configuration(&host.store, project).await?;
    evidence.fact("configuration.original", json!(&original));
    if fixtures::binding(&original, fixtures::ALIAS_PRIVATE).is_none() {
        return Err(error(
            "fixture_binding_missing",
            "project A 缺少 private-fixture 绑定，无法构造换代目标",
        ));
    }
    let gated_alias = format!("{}{GATED_ALIAS_SUFFIX}", fixtures::ALIAS_PRIVATE);
    let stale_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
    let gated_target = fixtures::container_path(&gated_alias);

    let container_before = host.container_id(maintainer).await?;
    let targets_before = host.reference_targets(maintainer).await?;
    evidence.fact(
        "container.before",
        json!({ "container_id": &container_before, "reference_targets": &targets_before }),
    );

    let mut timeline: Vec<Value> = Vec::new();

    if config.gate_mode.covers_when_idle() {
        // 1) 真实活动 turn：provider 延迟 5 秒，容器内再 sleep，保证窗口真实存在。
        let active = Probe::new(
            format!("gate-active-{tag}"),
            vec![ProbeCall::new(
                "exec",
                json!({ "command": format!("test -r {stale_target}/README.md && test ! -e {gated_target}/README.md && echo gate-fixed-old-lease") }),
            )],
        )
        .with_provider_delay(25);
        let active_input = probe::send(host, maintainer, &active).await?;
        let active_watch = probe::watch(
            host,
            maintainer,
            &active_input,
            Watch::Active,
            config.turn_timeout,
        )
        .await?;
        timeline.push(json!({
            "step": "active_turn_running",
            "at": chrono::Utc::now().to_rfc3339(),
            "state": &active_watch.state,
            "thread": &active_watch.thread_status,
        }));
        evidence.check(
            "gate.active_turn_running",
            active_watch.state == "running",
            format!(
                "活动 turn 真的在运行（thread {}）",
                active_watch.thread_status
            ),
            format!(
                "turn 没有进入 running：state={} thread={}",
                active_watch.state, active_watch.thread_status
            ),
        );

        // 2) 保存下一修订（alias 变化 ⇒ 真实挂载变化）并请求 WhenIdle 应用。
        let bindings = renamed(&original, fixtures::ALIAS_PRIVATE, &gated_alias);
        let saved = host
            .runtime
            .replace_project_references(
                project_id,
                ReplaceProjectReferencesRequest {
                    expected_revision: original.revision,
                    bindings,
                },
            )
            .await
            .map_err(|cause| {
                error(
                    "replace_references_failed",
                    format!("保存下一修订失败：{cause}"),
                )
            })?;
        evidence.fact("configuration.gated", json!(&saved));
        evidence.check(
            "gate.revision_advanced",
            saved.revision > original.revision,
            format!("配置修订从 {} 前进到 {}", original.revision, saved.revision),
            format!("配置修订没有前进（仍是 {}）", saved.revision),
        );
        host.runtime
            .apply_project_references(
                project_id,
                ApplyProjectReferencesRequest {
                    expected_revision: saved.revision,
                    policy: ReferenceApplyPolicy::WhenIdle,
                },
            )
            .await
            .map_err(|cause| {
                error(
                    "apply_request_failed",
                    format!("WhenIdle 应用请求被拒绝：{cause}"),
                )
            })?;

        // 3) 立刻核对：新修订还没生效，旧 turn 的真实挂载仍是旧目标。
        let pending = host.reference_status(project).await?;
        let maintainer_state = agent_state(&pending, maintainer);
        let targets_pending = host.reference_targets(maintainer).await?;
        evidence.fact(
            "pending",
            json!({
                "project_state": pending.state,
                "target_revision": pending.target_revision,
                "maintainer": maintainer_state,
                "reference_targets": &targets_pending,
            }),
        );
        evidence.check(
            "gate.not_applied_while_turn_active",
            pending.state != ProjectReferenceApplicationState::Applied
                && maintainer_state
                    .as_ref()
                    .is_some_and(|state| state.applied_revision < saved.revision),
            format!(
                "活动 turn 期间目标修订 {} 仍未 applied（project state={}）",
                saved.revision, pending.state
            ),
            format!(
                "活动 turn 期间目标修订就已经 applied（project state={}）",
                pending.state
            ),
        );
        evidence.check(
            "gate.old_lease_keeps_old_mounts",
            targets_pending.contains(&stale_target) && !targets_pending.contains(&gated_target),
            format!("旧 turn 的真实容器挂载仍是旧目标 {stale_target}"),
            format!("活动 turn 期间挂载已经变化：{targets_pending:?}"),
        );

        // 普通用户继续消息会主动抢占当前 Turn。先让原 Turn 自然结束，再验证 gate 中等待的输入。
        let active_done = probe::watch(
            host,
            maintainer,
            &active_input,
            Watch::Terminal,
            config.turn_timeout,
        )
        .await?;
        timeline.push(json!({
            "step": "active_turn_terminal",
            "at": chrono::Utc::now().to_rfc3339(),
            "state": &active_done.state,
        }));
        evidence.check(
            "gate.active_turn_completed",
            active_done.completed(),
            format!("活动 turn 正常完成（{}）", active_done.state),
            format!("活动 turn 没有正常完成：{}", active_done.state),
        );
        let active_outcome =
            probe::collect(host, maintainer, &active, &active_input, active_done).await?;
        evidence.check(
            "gate.active_tool_retains_old_lease",
            active_outcome.call(0).is_some_and(|call| {
                call.observed
                    && call.success == Some(true)
                    && call.output.contains("gate-fixed-old-lease")
            }),
            "当前 Turn 在配置变化后仍通过原租约完成真实命令",
            "当前 Turn 的原租约执行失败",
        );
        evidence.fact("probe.active", json!(&active_outcome));

        // 4) 排队下一条消息：必须等到应用完成才允许开始。
        let queued = Probe::new(
            format!("gate-queued-{tag}"),
            vec![
                ProbeCall::new(
                    "read_file",
                    json!({ "path": format!("{gated_target}/README.md") }),
                ),
                ProbeCall::new(
                    "read_file",
                    json!({ "path": format!("{stale_target}/README.md") }),
                ),
            ],
        );
        let queued_input = probe::send(host, maintainer, &queued).await?;
        let queued_now = probe::snapshot(host, maintainer, &queued_input).await?;
        timeline.push(json!({
            "step": "queued_message_accepted",
            "at": chrono::Utc::now().to_rfc3339(),
            "state": &queued_now.state,
            "thread": &queued_now.thread_status,
        }));
        evidence.check(
            "gate.queued_not_started",
            queued_now.state == "not_started",
            "排队消息已受理但没有开始执行",
            format!(
                "排队消息在应用完成前就开始了：state={} thread={}",
                queued_now.state, queued_now.thread_status
            ),
        );

        let converged =
            wait_for_applied(host, project, saved.revision, config.operation_timeout).await?;
        add_convergence_evidence(
            evidence,
            "gate.applied_gated",
            &converged,
            saved.revision,
            maintainer,
        );
        let targets_after = host.reference_targets(maintainer).await?;
        let container_after = host.container_id(maintainer).await?;
        evidence.fact(
            "container.after",
            json!({ "container_id": container_after, "reference_targets": targets_after }),
        );
        evidence.check(
            "gate.generation_swapped",
            container_after != container_before,
            format!("参考挂载变化触发了真实换代：{container_before:?} -> {container_after:?}"),
            format!("参考挂载变化没有换代，容器仍是 {container_after:?}"),
        );
        evidence.check(
            "gate.new_mounts_present",
            targets_after.contains(&gated_target) && !targets_after.contains(&stale_target),
            format!("真实容器挂载已切换为 {gated_target}"),
            format!("真实容器挂载没有切换：{targets_after:?}"),
        );
        if let Some(old) = &container_before {
            let running_old = host
                .docker
                .list_managed_containers()
                .await
                .map_err(|cause| error("docker_list_failed", format!("列出容器失败：{cause}")))?
                .into_iter()
                .any(|container| container.id == *old && container.state == "running");
            evidence.check(
                "gate.old_container_not_running",
                !running_old,
                format!("旧容器 {old} 已不再运行"),
                format!("旧容器 {old} 仍在运行，存在双执行环境"),
            );
        }

        // 6) 排队 turn 收敛后必须看到新配置。
        let queued_outcome = probe::collect(
            host,
            maintainer,
            &queued,
            &queued_input,
            probe::watch(
                host,
                maintainer,
                &queued_input,
                Watch::Terminal,
                config.turn_timeout,
            )
            .await?,
        )
        .await?;
        evidence.fact("probe.queued", json!(&queued_outcome));
        evidence.check(
            "gate.queued_turn_sees_new_configuration",
            queued_outcome
                .call(0)
                .is_some_and(|call| call.observed && call.success == Some(true))
                && queued_outcome
                    .call(1)
                    .is_some_and(|call| call.observed && call.success == Some(false)),
            format!("排队 turn 读到新 alias {gated_target} 且读不到旧 alias {stale_target}"),
            format!(
                "排队 turn 的挂载视图不符合目标修订：{:?}",
                queued_outcome
                    .calls
                    .iter()
                    .map(|call| (call.name.clone(), call.observed, call.success))
                    .collect::<Vec<_>>()
            ),
        );
        assert_model_facts(
            config,
            evidence,
            queued.probe_id(),
            &[gated_target.as_str()],
            &[stale_target.as_str()],
        );
    }

    if config.gate_mode.covers_stop_and_apply() {
        // 单独运行 stop 模式时，先真实建立 gated 修订并收敛，保证 StopAndApply 有真实目标
        // （否则"还原到当前配置"不产生换代，也不会取消活动 turn）。
        let current = fixtures::project_configuration(&host.store, project).await?;
        if current.bindings == original.bindings {
            let gated = host
                .runtime
                .replace_project_references(
                    project_id,
                    ReplaceProjectReferencesRequest {
                        expected_revision: current.revision,
                        bindings: renamed(&original, fixtures::ALIAS_PRIVATE, &gated_alias),
                    },
                )
                .await
                .map_err(|cause| {
                    error(
                        "replace_references_failed",
                        format!("stop 模式建立 gated 修订失败：{cause}"),
                    )
                })?;
            evidence.fact("configuration.gated_for_stop", json!(&gated));
            let converged =
                wait_for_applied(host, project, gated.revision, config.operation_timeout).await?;
            add_convergence_evidence(
                evidence,
                "gate.applied_gated_for_stop",
                &converged,
                gated.revision,
                maintainer,
            );
        }
        // 7) StopAndApply：活动 turn 必须被真正取消后才能发布新环境。
        let active = Probe::new(
            format!("gate-stop-active-{tag}"),
            vec![ProbeCall::new(
                "exec",
                json!({ "command": "sleep 40; echo stop-probe-finished" }),
            )],
        )
        .with_provider_delay(5);
        let active_input = probe::send(host, maintainer, &active).await?;
        let running = probe::watch(
            host,
            maintainer,
            &active_input,
            Watch::Active,
            config.turn_timeout,
        )
        .await?;
        evidence.check(
            "gate.stop.active_turn_running",
            running.state == "running",
            format!(
                "StopAndApply 之前 turn 正在运行（thread {}）",
                running.thread_status
            ),
            format!("StopAndApply 之前 turn 未运行：{}", running.state),
        );
        let stopping_started = std::time::Instant::now();
        let restored =
            restore_configuration(host, project, &original, ReferenceApplyPolicy::StopAndApply)
                .await?;
        evidence.fact("configuration.restored", json!(&restored));
        host.runtime
            .apply_project_references(
                project_id,
                ApplyProjectReferencesRequest {
                    expected_revision: restored.revision,
                    policy: ReferenceApplyPolicy::StopAndApply,
                },
            )
            .await
            .map_err(|cause| {
                error(
                    "stop_and_apply_rejected",
                    format!("StopAndApply 请求被拒绝：{cause}"),
                )
            })?;
        let cancelled = probe::watch(
            host,
            maintainer,
            &active_input,
            Watch::Terminal,
            config.turn_timeout,
        )
        .await?;
        evidence.fact(
            "stop.cancelled_turn",
            json!({
                "state": &cancelled.state,
                "is_cancelled_state": cancelled.cancelled(),
                "thread": &cancelled.thread_status,
            }),
        );
        evidence.check(
            "gate.stop.active_turn_cancelled",
            cancelled.cancelled(),
            format!(
                "StopAndApply 让活动 turn 进入非完成终态：{}",
                cancelled.state
            ),
            format!("活动 turn 竟然正常完成：{}", cancelled.state),
        );
        evidence.check(
            "gate.stop.cancel_without_waiting_for_docker_capacity",
            stopping_started.elapsed() < std::time::Duration::from_secs(5),
            format!(
                "停止请求在 {:?} 内取消当前 Turn",
                stopping_started.elapsed()
            ),
            format!(
                "取消当前 Turn 被重型换代排队阻塞：{:?}",
                stopping_started.elapsed()
            ),
        );
        let converged =
            wait_for_applied(host, project, restored.revision, config.operation_timeout).await?;
        add_convergence_evidence(
            evidence,
            "gate.applied_restored",
            &converged,
            restored.revision,
            maintainer,
        );
        let restored_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
        let targets = host.reference_targets(maintainer).await?;
        evidence.check(
            "gate.stop.restored_mounts",
            targets.contains(&restored_target),
            format!("还原后的真实挂载包含 {restored_target}"),
            format!("还原后的真实挂载异常：{targets:?}"),
        );
        let probe_after = Probe::new(
            format!("gate-applied-{tag}"),
            vec![ProbeCall::new(
                "read_file",
                json!({ "path": format!("{restored_target}/README.md") }),
            )],
        );
        let outcome = probe::run(host, maintainer, &probe_after, config.turn_timeout).await?;
        evidence.fact("probe.after_restore", json!(&outcome));
        evidence.check(
            "gate.applied_snapshot_visible",
            outcome
                .call(0)
                .is_some_and(|call| call.observed && call.success == Some(true)),
            format!("应用收敛后的 turn 通过真实工具读到 {restored_target}"),
            format!("应用收敛后真实工具仍读不到 {restored_target}"),
        );
        assert_model_facts(
            config,
            evidence,
            probe_after.probe_id(),
            &[restored_target.as_str()],
            &[gated_target.as_str()],
        );
    } else {
        // 未覆盖 StopAndApply 时把配置也还原，避免留下 gated alias 影响其它场景。
        let restored =
            restore_configuration(host, project, &original, ReferenceApplyPolicy::WhenIdle).await?;
        evidence.fact("configuration.restored", json!(&restored));
        let converged =
            wait_for_applied(host, project, restored.revision, config.operation_timeout).await?;
        add_convergence_evidence(
            evidence,
            "gate.applied_restored",
            &converged,
            restored.revision,
            maintainer,
        );
    }

    evidence.fact("timeline", json!(timeline));
    assert_fixture_restored(evidence, &host.store, project, &gated_alias).await
}
