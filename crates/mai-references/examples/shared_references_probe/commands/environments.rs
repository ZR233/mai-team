//! 物理环境命令：`seed` 建/复用初始环境与标记，删除路径的容器/卷收尾。

use super::fixtures::{mark_agents_ready, persist_fixtures};
use super::sources::{SourceMode, ensure_fixture_files, resolve_sources};
use super::targets::write_seed_bindings;
use super::*;

/// `seed`：登记源、写夹具、建/复用初始真实环境与三类数据标记。
pub(super) async fn seed(config: &ProbeConfig, report: &mut Report) -> Result<()> {
    let mut probe = build_probe(config, report).await?;
    ensure_fixture_files(&probe.config, report)?;
    resolve_sources(&mut probe, SourceMode::Ensure, report).await?;
    persist_fixtures(&probe, report).await?;
    write_seed_bindings(&probe, report).await?;

    let mut provisions = Vec::new();
    let mut ready_agents = Vec::new();
    for agent in probe.fixtures.selected(config.provision) {
        let snapshot = environment::current_snapshot(&probe, agent).await?;
        let outcome = environment::provision_agent(&probe, agent, &snapshot).await?;
        let markers =
            environment::ensure_markers(&probe, agent, &outcome.container_id, outcome.created)
                .await?;
        if outcome.matches_current_targets {
            ready_agents.push(agent.key);
        }
        provisions.push(json!({
            "agent_key": agent.key,
            "created": outcome.created,
            "container_id": outcome.container_id,
            "generation": outcome.generation,
            "effective_image_id": outcome.effective_image_id,
            "base_image_id": outcome.base_image_id,
            "volumes": outcome.volumes,
            "matches_current_targets": outcome.matches_current_targets,
            "mismatch": outcome.mismatch,
            "markers": markers,
        }));
    }
    report.fact("provision", &provisions)?;
    mark_agents_ready(&probe, &ready_agents).await?;
    finish(&probe, report).await
}

/// 删除路径的收尾：确认没有残留的夹具受管容器，并报告卷仍然保留。
pub(super) async fn check_leftovers(probe: &Probe, report: &mut Report) -> Result<()> {
    let containers = probe.docker.list_managed_containers().await?;
    let fixture_ids = probe
        .fixtures
        .agents
        .iter()
        .map(|agent| agent.id().to_string())
        .collect::<Vec<_>>();
    let leftovers = containers
        .iter()
        .filter(|container| {
            container
                .agent_id
                .as_ref()
                .is_some_and(|id| fixture_ids.contains(id))
        })
        .map(|container| container.id.clone())
        .collect::<Vec<_>>();
    let mut outcome = checks::Outcome::new("T39.no_agent_containers_left");
    if leftovers.is_empty() {
        outcome.pass("夹具 Agent 的受管容器已按精确 ID 定向清理完毕");
    } else {
        outcome.fail(format!("仍有夹具受管容器 {leftovers:?}"));
    }
    report.check(outcome.finish("删除后没有残留的夹具受管容器"));

    let mut volumes = Vec::new();
    for agent in probe.fixtures.selected(probe.config.provision) {
        let workspace = mai_docker::agent_workspace_volume(&agent.id().to_string());
        let anon = environment::anon_volume_name(agent.key);
        volumes.push(json!({
            "agent_key": agent.key,
            "workspace_volume": workspace,
            "workspace_exists": probe.docker.volume_exists(&workspace).await?,
            "anon_volume": anon,
            "anon_exists": probe.docker.volume_exists(&anon).await?,
        }));
    }
    report.fact("volumes_after_delete", &volumes)?;
    Ok(())
}
