//! 项目/Agent 夹具的幂等持久化与资源状态投影。

use super::*;

/// 幂等写入项目与 Agent 夹具（不删除任何记录）。
pub(super) async fn persist_fixtures(probe: &Probe, report: &mut Report) -> Result<()> {
    let snapshot = probe.store.load_runtime_snapshot(0).await?;
    let summaries = snapshot
        .agents
        .iter()
        .map(|agent| agent.summary.clone())
        .collect::<Vec<_>>();
    let existing = crate::fixtures::existing_agents(&summaries);
    let now = Utc::now();
    for kind in [ProjectKind::A, ProjectKind::B] {
        if !snapshot
            .projects
            .iter()
            .any(|project| project.id == kind.id())
        {
            probe
                .store
                .insert_project(&crate::fixtures::project_summary(&probe.config, kind, now))
                .await?;
        }
    }
    for agent in &probe.fixtures.agents {
        let summary = crate::fixtures::agent_summary(
            &probe.config,
            &probe.fixtures,
            agent,
            existing.get(&agent.id()),
            now,
        );
        if existing.contains_key(&agent.id()) {
            probe.store.save_agent(&summary, None).await?;
        } else {
            probe.store.insert_agent(&summary, None).await?;
        }
    }
    report.fact(
        "fixtures",
        json!({
            "project_a": ProjectKind::A.id(),
            "project_b": ProjectKind::B.id(),
            "agents": probe
                .fixtures
                .agents
                .iter()
                .map(|agent| json!({
                    "key": agent.key,
                    "agent_id": agent.id(),
                    "role": agent.role_label(),
                    "project": agent.project.map(|kind| kind.slug()),
                    "parent": agent.parent,
                    "required_environment": agent.required,
                }))
                .collect::<Vec<_>>(),
        }),
    )?;
    Ok(())
}

/// 把已建好环境的夹具 Agent 标记为 Ready（产品资源状态，不代表已跑模型）。
pub(super) async fn mark_agents_ready(probe: &Probe, keys: &[&'static str]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let snapshot = probe.store.load_runtime_snapshot(0).await?;
    let summaries = snapshot
        .agents
        .iter()
        .map(|agent| agent.summary.clone())
        .collect::<Vec<_>>();
    let existing = crate::fixtures::existing_agents(&summaries);
    for key in keys {
        let Some(fixture) = probe.fixtures.agent(key) else {
            continue;
        };
        let binding = probe.store.reference_binding(fixture.id()).await?;
        let Some(mut summary) = existing.get(&fixture.id()).cloned() else {
            continue;
        };
        summary.resource.state = mai_protocol::AgentResourceState::Ready;
        summary.resource.error = None;
        if let Some(binding) = &binding {
            summary.container_id = Some(binding.container_id.clone());
        }
        summary.updated_at = Utc::now();
        probe.store.save_agent(&summary, None).await?;
    }
    Ok(())
}
