//! T29 固定 A/B 项目的不变量快照。

use std::collections::BTreeMap;

use mai_protocol::AgentId;
use serde::Serialize;

use crate::error::Result;
use crate::host::RuntimeHost;

use super::support;

/// 固定项目一个 Agent 的不变量投影。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct FixedAgent {
    agent_id: AgentId,
    container_id: Option<String>,
    generation: Option<uuid::Uuid>,
    applied_revision: Option<u64>,
    fingerprint: Option<String>,
}

/// 固定项目一个受管容器的身份与 generation 投影。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct FixedContainer {
    id: String,
    name: String,
    agent_id: Option<String>,
    project_id: Option<String>,
    sidecar: bool,
    generation: Option<uuid::Uuid>,
}

/// 固定项目 A/B 的配置、绑定与受管容器投影。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct FixedProjects {
    revisions: BTreeMap<String, u64>,
    agents: Vec<FixedAgent>,
    containers: Vec<FixedContainer>,
}

/// 采集固定 A/B 项目不变量；所有外部读取都带 T29 单步超时。
pub(super) async fn fixed_project_snapshot(host: &RuntimeHost) -> Result<FixedProjects> {
    let snapshot = support::bounded(
        host,
        "t29_runtime_snapshot",
        host.store.load_runtime_snapshot(0),
    )
    .await?;
    let fixed = [
        crate::fixtures::ProjectKind::A,
        crate::fixtures::ProjectKind::B,
    ];
    let mut revisions = BTreeMap::new();
    for kind in fixed {
        let configuration = support::bounded(
            host,
            "t29_fixed_project_configuration",
            host.store.project_reference_configuration(kind.id()),
        )
        .await?;
        revisions.insert(kind.slug().to_string(), configuration.revision);
    }
    let fixed_ids = fixed.iter().map(|kind| kind.id()).collect::<Vec<_>>();
    let mut agents = Vec::new();
    for agent in snapshot.agents.iter().filter(|agent| {
        agent
            .summary
            .project_id
            .is_some_and(|project| fixed_ids.contains(&project))
    }) {
        let binding = support::bounded(
            host,
            "t29_fixed_agent_binding",
            host.store.reference_binding(agent.summary.id),
        )
        .await?;
        agents.push(FixedAgent {
            agent_id: agent.summary.id,
            container_id: agent.summary.container_id.clone(),
            generation: binding.as_ref().map(|binding| binding.generation),
            applied_revision: binding.as_ref().map(|binding| binding.applied_revision),
            fingerprint: binding.as_ref().map(|binding| binding.fingerprint.clone()),
        });
    }
    agents.sort_by_key(|agent| agent.agent_id);
    let mut containers = support::bounded(
        host,
        "t29_fixed_managed_containers",
        host.docker.list_managed_containers(),
    )
    .await?
    .into_iter()
    .filter(|container| {
        container
            .project_id
            .as_deref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .is_some_and(|id| fixed_ids.contains(&id))
    })
    .map(|container| FixedContainer {
        id: container.id,
        name: container.name,
        agent_id: container.agent_id,
        project_id: container.project_id,
        sidecar: container.sidecar,
        generation: container.generation,
    })
    .collect::<Vec<_>>();
    containers.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(FixedProjects {
        revisions,
        agents,
        containers,
    })
}
