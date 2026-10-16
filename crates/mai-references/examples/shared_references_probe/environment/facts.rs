//! 事实采集：Agent 视角（绑定、容器、sidecar、收据、标记、模型可见清单）与当前参考快照。

use super::markers::{MarkerFact, read_markers};
use super::*;

/// 采集到的事实：一次性读出来，判定与比较都基于同一份观察。
#[derive(Debug, Clone, Serialize)]
pub struct AgentFacts {
    pub agent_key: String,
    pub agent_id: Uuid,
    pub role: String,
    pub project_id: Option<Uuid>,
    pub project_slug: Option<String>,
    pub parent_id: Option<Uuid>,
    pub config_revision: u64,
    pub binding: Option<BindingFacts>,
    pub container: Option<ContainerFacts>,
    pub container_lookup: String,
    pub sidecars: Vec<SidecarFacts>,
    pub operations: Vec<OperationFacts>,
    pub model_manifest: Option<String>,
    pub markers: Vec<MarkerFact>,
}

/// 已发布的物理绑定事实（AgentSummary.container_id 的受控投影来源）。
#[derive(Debug, Clone, Serialize)]
pub struct BindingFacts {
    pub generation: Uuid,
    pub container_id: String,
    pub effective_image_id: String,
    pub applied_revision: u64,
    pub fingerprint: String,
    pub snapshot_revision: u64,
    pub references: Vec<AppliedReference>,
}

/// 实际容器事实的验收投影。
#[derive(Debug, Clone, Serialize)]
pub struct ContainerFacts {
    pub id: String,
    pub name: String,
    pub state: String,
    pub running: bool,
    pub generation: Option<Uuid>,
    pub image_reference: String,
    pub image_id: String,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub network_mode: Option<String>,
    pub mounts: Vec<ContainerMountFacts>,
}

/// MCP sidecar 事实。
#[derive(Debug, Clone, Serialize)]
pub struct SidecarFacts {
    pub id: String,
    pub name: String,
    pub generation: Option<Uuid>,
    pub image_id: String,
    pub mounts: Vec<ContainerMountFacts>,
}

/// 换代收据事实。
#[derive(Debug, Clone, Serialize)]
pub struct OperationFacts {
    pub id: Uuid,
    pub phase: ReferenceApplyPhase,
    pub policy: String,
    pub target_revision: u64,
    pub old_container_id: Option<String>,
    pub candidate_container_id: Option<String>,
    pub snapshot_image_id: Option<String>,
    pub failure: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

impl OperationFacts {
    /// 收据是否还在进行中。
    pub fn active(&self) -> bool {
        !matches!(
            self.phase,
            ReferenceApplyPhase::Succeeded
                | ReferenceApplyPhase::Failed
                | ReferenceApplyPhase::Superseded
        )
    }
}

/// 一个项目/非项目 Agent 的当前参考快照；空配置走领域层的同一入口。
pub async fn current_snapshot(
    probe: &Probe,
    agent: &AgentFixture,
) -> Result<AppliedReferenceSnapshot> {
    match agent.project {
        Some(kind) => {
            let configuration = probe
                .store
                .project_reference_configuration(kind.id())
                .await?;
            Ok(probe.catalog.snapshot(kind.id(), &configuration).await?)
        }
        None => Ok(probe
            .catalog
            .snapshot(
                Uuid::nil(),
                &mai_protocol::ProjectReferenceConfiguration::empty(),
            )
            .await?),
    }
}

/// 采集一个夹具 Agent 的全部事实。
pub async fn collect_agent_facts(
    probe: &Probe,
    fixtures: &crate::fixtures::FixtureSet,
    agent: &AgentFixture,
) -> Result<AgentFacts> {
    let agent_id = agent.id();
    let project = agent.project.map(|kind| kind.id());
    let config_revision = match agent.project {
        Some(kind) => {
            probe
                .store
                .project_reference_configuration(kind.id())
                .await?
                .revision
        }
        None => 0,
    };
    let binding = probe.store.reference_binding(agent_id).await?;
    let managed = probe.docker.list_managed_containers().await?;
    let mut container_lookup = "missing".to_string();
    let container_id = match binding.as_ref() {
        Some(binding)
            if managed
                .iter()
                .any(|container| !container.sidecar && container.id == binding.container_id) =>
        {
            container_lookup = "published_binding".to_string();
            Some(binding.container_id.clone())
        }
        _ => {
            let found = managed
                .iter()
                .find(|container| {
                    !container.sidecar
                        && container.agent_id.as_deref() == Some(agent_id.to_string().as_str())
                })
                .map(|container| container.id.clone());
            if found.is_some() {
                container_lookup = "agent_label".to_string();
            }
            found
        }
    };
    let container = match &container_id {
        Some(id) => {
            let facts = probe.docker.inspect_container_environment(id).await?;
            Some(ContainerFacts {
                id: facts.id.clone(),
                name: facts.name.clone(),
                state: facts.state.clone(),
                running: facts.running,
                generation: facts.generation,
                image_reference: facts.image_reference.clone(),
                image_id: facts.image_id.clone(),
                privileged: facts.host.privileged,
                cap_add: facts.host.cap_add.clone(),
                network_mode: facts.host.network_mode.clone(),
                mounts: facts.mounts.clone(),
            })
        }
        None => None,
    };
    let sidecars = managed
        .iter()
        .filter(|candidate| {
            candidate.sidecar
                && candidate.agent_id.as_deref() == Some(agent_id.to_string().as_str())
        })
        .map(|candidate| SidecarFacts {
            id: candidate.id.clone(),
            name: candidate.name.clone(),
            generation: candidate.generation,
            image_id: String::new(),
            mounts: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut sidecars = sidecars;
    for sidecar in sidecars.iter_mut() {
        if let Ok(facts) = probe
            .docker
            .inspect_container_environment(&sidecar.id)
            .await
        {
            sidecar.image_id = facts.image_id.clone();
            sidecar.mounts = facts.mounts.clone();
        }
    }
    let operations = probe
        .store
        .reference_operations()
        .await?
        .into_iter()
        .filter(|receipt| receipt.operation.agent_id == agent_id)
        .map(|receipt| OperationFacts {
            id: receipt.operation.id,
            phase: receipt.operation.phase,
            policy: receipt.operation.policy.to_string(),
            target_revision: receipt.operation.target_revision,
            old_container_id: receipt.operation.old_container_id.clone(),
            candidate_container_id: receipt.operation.candidate_container_id.clone(),
            snapshot_image_id: receipt.operation.snapshot_image_id.clone(),
            failure: receipt
                .operation
                .failure
                .as_ref()
                .map(|failure| format!("{}: {}", failure.code, failure.message)),
            created_at: receipt.operation.created_at,
            updated_at: receipt.operation.updated_at,
        })
        .collect::<Vec<_>>();
    let binding_facts = binding.as_ref().map(|binding| BindingFacts {
        generation: binding.generation,
        container_id: binding.container_id.clone(),
        effective_image_id: binding.effective_image_id.clone(),
        applied_revision: binding.applied_revision,
        fingerprint: binding.fingerprint.clone(),
        snapshot_revision: binding.snapshot.configuration_revision,
        references: binding.snapshot.references.clone(),
    });
    let model_manifest = binding
        .as_ref()
        .and_then(|binding| render_model_manifest(&binding.snapshot));
    let markers = match &container {
        Some(container) => read_markers(probe, agent.key, &container.id).await?,
        None => Vec::new(),
    };
    Ok(AgentFacts {
        agent_key: agent.key.to_string(),
        agent_id,
        role: agent.role_label(),
        project_id: project,
        project_slug: agent.project.map(|kind| kind.slug().to_string()),
        parent_id: agent.parent.map(|key| {
            fixtures
                .agent(key)
                .map(AgentFixture::id)
                .unwrap_or_else(|| crate::fixtures::fixture_uuid("agent", key))
        }),
        config_revision,
        binding: binding_facts,
        container,
        container_lookup,
        sidecars,
        operations,
        model_manifest,
        markers,
    })
}
