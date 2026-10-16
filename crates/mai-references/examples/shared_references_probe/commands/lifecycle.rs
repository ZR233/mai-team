//! apply/status/recover/clear：保存目标并收敛、只读判定、按收据恢复与空清除。

use super::fixtures::persist_fixtures;
use super::operations::{
    concurrent_target_sequence, enqueue_unmet_targets, reuse_probe, run_operations,
};
use super::sources::{SourceMode, resolve_sources};
use super::targets::{TargetBindings, save_targets};
use super::*;

/// `apply`：保存目标配置并驱动状态机到收敛。
pub(super) async fn apply(config: &ProbeConfig, report: &mut Report) -> Result<()> {
    let mut probe = build_probe(config, report).await?;
    resolve_sources(&mut probe, SourceMode::Ensure, report).await?;
    persist_fixtures(&probe, report).await?;
    // 启动屏障：任何操作之前先按持久收据重建准入。
    probe.applications.restore_admission().await?;

    let scenario = config.scenario_or_bind();
    report.fact("scenario", scenario)?;
    match scenario {
        Scenario::Bind => {
            save_targets(&probe, TargetBindings::LinuxAndFixture).await?;
        }
        Scenario::MetadataOnly => {
            save_targets(&probe, TargetBindings::MetadataOnly).await?;
        }
        Scenario::Clear => {
            save_targets(&probe, TargetBindings::Empty).await?;
        }
        Scenario::Reuse => {
            report.boundary(
                "reuse_scenario",
                "reuse 场景不改配置，只做两次 ensure 与跨轮身份比较",
            );
        }
        Scenario::Concurrent => {
            concurrent_target_sequence(&probe, report).await?;
            // 与 Runtime 的收据调度一致：旧目标让位后，为最新目标补齐意图。
            enqueue_unmet_targets(&probe, report).await?;
        }
    }
    run_operations(&probe, report).await?;
    if scenario == Scenario::Reuse {
        reuse_probe(&probe, report).await?;
    }
    finish(&probe, report).await
}

/// `status`：只读收集事实并判定；不登记源、不写夹具、不驱动状态机。
pub(super) async fn status(config: &ProbeConfig, report: &mut Report) -> Result<()> {
    let mut probe = build_probe(config, report).await?;
    resolve_sources(&mut probe, SourceMode::ReadOnly, report).await?;
    finish(&probe, report).await
}

/// `recover`：先重建准入，再按最新收据恢复，必要时按当前修订重新入队，最后判定。
pub(super) async fn recover(config: &ProbeConfig, report: &mut Report) -> Result<()> {
    let mut probe = build_probe(config, report).await?;
    resolve_sources(&mut probe, SourceMode::Ensure, report).await?;
    probe.applications.restore_admission().await?;

    let resumed = run_operations(&probe, report).await?;
    report.fact("recovery_first_pass", &resumed)?;
    enqueue_unmet_targets(&probe, report).await?;
    let second = run_operations(&probe, report).await?;
    report.fact("recovery_second_pass", &second)?;
    finish(&probe, report).await
}

/// `clear`：把项目绑定清空并收敛，验证旧挂载真实消失。
pub(super) async fn clear(config: &ProbeConfig, report: &mut Report) -> Result<()> {
    let mut probe = build_probe(config, report).await?;
    resolve_sources(&mut probe, SourceMode::Ensure, report).await?;
    probe.applications.restore_admission().await?;
    save_targets(&probe, TargetBindings::Empty).await?;
    run_operations(&probe, report).await?;
    let sources_kept = json!({
        "linux_source_exists": probe.config.linux_source.is_dir(),
        "fixture_repo_exists": probe.config.fixture_repo.is_dir(),
    });
    report.fact("source_files_after_clear", &sources_kept)?;
    if !probe.config.linux_source.is_dir() || !probe.config.fixture_repo.is_dir() {
        report.check(Check::fail(
            "T39.source_files_kept",
            "清空引用后宿主源码目录不存在（不得删除原件）",
        ));
    } else {
        report.check(Check::pass(
            "T39.source_files_kept",
            "清空引用后宿主源码目录仍然存在",
        ));
    }
    finish(&probe, report).await
}
