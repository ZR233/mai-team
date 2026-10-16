//! `delete`：收敛后删除 Agent/项目记录并注销源；绝不删除宿主源码。

use super::environments::check_leftovers;
use super::operations::run_operations;
use super::sources::{SourceMode, resolve_sources};
use super::targets::{TargetBindings, save_targets};
use super::*;

/// `delete`：收敛后删除 Agent/项目记录并注销源；绝不删除宿主源码。
pub(super) async fn delete(config: &ProbeConfig, report: &mut Report) -> Result<()> {
    let mut probe = build_probe(config, report).await?;
    resolve_sources(&mut probe, SourceMode::ReadOnly, report).await?;
    probe.applications.restore_admission().await?;

    // 仍被引用时注销必须被拒绝（source_in_use）。
    if let Some(linux) = probe.sources.linux {
        match probe.catalog.unregister(linux).await {
            Ok(()) => report.check(Check::fail(
                "T39.unregister_while_referenced",
                "源仍被项目引用却被注销成功",
            )),
            Err(error) => report.check(Check::pass(
                "T39.unregister_while_referenced",
                format!(
                    "被引用时注销被拒绝：{}（{}）",
                    error.code(),
                    error.message()
                ),
            )),
        }
    } else {
        report.check(Check::blocked(
            "T39.unregister_while_referenced",
            "linux 源尚未登记，无法验证“引用中注销被拒绝”",
        ));
    }

    // 先收敛所有待办，再删除父记录。
    save_targets(&probe, TargetBindings::Empty).await?;
    run_operations(&probe, report).await?;

    let mut removed = Vec::new();
    for agent in probe.fixtures.selected(probe.config.provision) {
        let binding = probe.store.reference_binding(agent.id()).await?;
        let preferred = binding.as_ref().map(|binding| binding.container_id.clone());
        let containers = probe
            .docker
            .delete_agent_containers(&agent.id().to_string(), preferred.as_deref())
            .await?;
        removed.push(json!({ "agent_key": agent.key, "removed_containers": containers }));
    }
    // 未建环境的夹具 Agent 也要清掉可能存在的历史容器。
    for agent in &probe.fixtures.agents {
        if probe
            .fixtures
            .selected(probe.config.provision)
            .iter()
            .any(|selected| selected.key == agent.key)
        {
            continue;
        }
        let containers = probe
            .docker
            .delete_agent_containers(&agent.id().to_string(), None)
            .await?;
        if !containers.is_empty() {
            removed.push(json!({ "agent_key": agent.key, "removed_containers": containers }));
        }
    }
    report.fact("removed_containers", &removed)?;

    for agent in &probe.fixtures.agents {
        probe.store.delete_agent(agent.id()).await?;
    }
    for kind in [ProjectKind::A, ProjectKind::B] {
        probe.store.delete_project(kind.id()).await?;
    }
    for (role, id) in [
        ("linux", probe.sources.linux),
        ("private-fixture", probe.sources.private_fixture),
    ] {
        match id {
            Some(id) => match probe.catalog.unregister(id).await {
                Ok(()) => report.check(Check::pass(
                    "T39.unregister_after_unbind",
                    format!("{role} 在解绑并收敛后注销成功"),
                )),
                Err(error) => report.check(Check::fail(
                    "T39.unregister_after_unbind",
                    format!("{role} 注销失败：{}（{}）", error.code(), error.message()),
                )),
            },
            None => report.check(Check::blocked(
                "T39.unregister_after_unbind",
                format!("{role} 尚未登记，跳过注销"),
            )),
        }
    }

    let leftovers = probe.store.reference_operations().await?;
    let bindings = probe.store.reference_sources().await?;
    let snapshot = probe.store.load_runtime_snapshot(0).await?;
    let kept = json!({
        "linux_source_exists": probe.config.linux_source.is_dir(),
        "fixture_repo_exists": probe.config.fixture_repo.is_dir(),
        "remaining_sources": bindings.len(),
        "remaining_operations": leftovers.len(),
        "remaining_agents": snapshot.agents.len(),
    });
    report.fact("after_delete", &kept)?;
    let mut outcome = checks::Outcome::new("T39.delete_keeps_source_files");
    if probe.config.linux_source.is_dir() && probe.config.fixture_repo.is_dir() {
        outcome.pass("宿主源码目录仍然存在（删除引用/项目/Agent 不删原件）");
    } else {
        outcome.fail("宿主源码目录不存在");
    }
    if bindings.is_empty() && leftovers.is_empty() {
        outcome.pass("登记表与操作收据已清空");
    } else {
        outcome.fail(format!(
            "仍有 {} 个登记源、{} 条操作收据",
            bindings.len(),
            leftovers.len()
        ));
    }
    report.check(outcome.finish("删除引用/项目/Agent 后宿主源码保留、元数据清空"));
    check_leftovers(&probe, report).await
}
