//! 建或复用真实环境：工作区卷、项目只读 view 卷、等价匿名卷与参考只读 bind。

use super::*;

/// 一条卷事实。
#[derive(Debug, Clone, Serialize)]
pub struct VolumeFact {
    pub name: String,
    pub target: String,
    pub read_only: bool,
}

/// 一次晋升（建或复用）的结果。
#[derive(Debug, Clone, Serialize)]
pub struct ProvisionOutcome {
    pub created: bool,
    pub container_id: String,
    pub generation: Option<Uuid>,
    pub effective_image_id: String,
    pub base_image_id: String,
    pub volumes: Vec<VolumeFact>,
    /// 复用到的既有环境是否与当前目标配置一致（不一致时由 apply/recover 收敛）。
    pub matches_current_targets: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<String>,
}

/// 建或复用真实环境；绝不新建第二个环境，也绝不删除任何既有资源。
pub async fn provision_agent(
    probe: &Probe,
    agent: &AgentFixture,
    snapshot: &AppliedReferenceSnapshot,
) -> Result<ProvisionOutcome> {
    let agent_id = agent.id();
    let agent_text = agent_id.to_string();
    let base_image_id = probe.docker.image_id(&probe.config.agent_image).await?;
    let binding = probe.store.reference_binding(agent_id).await?;

    let workspace_volume = agent_workspace_volume(&agent_text);
    let anon_volume = anon_volume_name(agent.key);
    probe
        .docker
        .ensure_volume(
            &workspace_volume,
            &[("mai.team.acceptance.fixture", agent.key.to_string())],
        )
        .await?;
    probe
        .docker
        .ensure_volume(
            &anon_volume,
            &[("mai.team.acceptance.fixture", agent.key.to_string())],
        )
        .await?;

    let mut mounts: Vec<ContainerMount> = Vec::new();
    if let (Some(kind), true) = (agent.project, agent.review_view) {
        let cache = project_cache_volume(&kind.id().to_string());
        probe
            .docker
            .ensure_volume(
                &cache,
                &[
                    ("mai.team.acceptance.fixture", agent.key.to_string()),
                    ("mai.team.acceptance.project", kind.slug().to_string()),
                ],
            )
            .await?;
        mounts.push(ContainerMount::Volume(ContainerVolumeMount::read_only(
            cache,
            REVIEW_VIEW_TARGET,
        )?));
    }
    mounts.push(ContainerMount::Volume(ContainerVolumeMount::read_write(
        anon_volume.clone(),
        ANON_VOLUME_TARGET,
    )?));
    mounts.extend(reference_mounts(snapshot)?);

    let workspace = WorkspaceMountPlan::new(workspace_volume.clone(), None::<String>)?;
    // 与产品首次创建一致：generation 取 Agent ID，崩溃后才能找回同一个候选名。
    let generation = agent_id;
    let mut builder = AgentEnvironmentPlan::builder(
        agent_text.clone(),
        probe.config.agent_image.clone(),
        workspace,
    )
    .generation(generation)
    .effective_image_id(base_image_id.clone())
    .mounts(mounts);
    if let Some(kind) = agent.project {
        builder = builder.project_id(kind.id().to_string());
    }
    let plan = builder.build()?;

    let containers = probe.docker.list_managed_containers().await?;
    let owned = containers
        .into_iter()
        .filter(|container| {
            !container.sidecar && container.agent_id.as_deref() == Some(agent_text.as_str())
        })
        .collect::<Vec<_>>();
    let preferred = binding.as_ref().map(|binding| binding.container_id.clone());
    let chosen = preferred
        .as_deref()
        .and_then(|published| owned.iter().find(|container| container.id == published))
        .or_else(|| owned.first())
        .cloned();

    let Some(existing) = chosen else {
        let container = probe.docker.create_environment(&plan, &[]).await?;
        let facts = probe
            .docker
            .inspect_container_environment(&container.id)
            .await?;
        let final_plan =
            plan_from_environment(agent_id, Some(generation), &base_image_id, snapshot, &facts)?;
        let verified = probe
            .docker
            .verify_environment(&final_plan, &container.id)
            .await?;
        probe.docker.prepare_verified_container(&verified).await?;
        let facts = probe
            .docker
            .inspect_container_environment(&container.id)
            .await?;
        publish_binding(
            probe,
            agent,
            Some(generation),
            &container.id,
            &facts,
            snapshot,
        )
        .await?;
        return Ok(ProvisionOutcome {
            created: true,
            container_id: container.id,
            generation: Some(generation),
            effective_image_id: facts.image_id.clone(),
            base_image_id,
            volumes: volume_facts(&facts),
            matches_current_targets: true,
            mismatch: None,
        });
    };

    // 复用既有环境：只观察，不启动、不删除、不重建。
    let facts = probe
        .docker
        .inspect_container_environment(&existing.id)
        .await?;
    let plan = plan_from_environment(
        agent_id,
        facts.generation,
        &facts.image_id,
        snapshot,
        &facts,
    )?;
    let (matches, mismatch) = match probe.docker.inspect_environment(&plan).await? {
        ExistingEnvironment::Matching(_) => (true, None),
        ExistingEnvironment::ReconfigurationRequired(diff) => (false, Some(diff.to_string())),
        ExistingEnvironment::Absent => (false, Some("已登记容器不在受管容器列表里".to_string())),
    };
    if matches {
        publish_binding(probe, agent, facts.generation, &facts.id, &facts, snapshot).await?;
    }
    Ok(ProvisionOutcome {
        created: false,
        container_id: facts.id.clone(),
        generation: facts.generation,
        effective_image_id: facts.image_id.clone(),
        base_image_id,
        volumes: volume_facts(&facts),
        matches_current_targets: matches,
        mismatch,
    })
}

/// 发布物理绑定（唯一物理身份入口）并登记租约；运行时的准入仍由应用状态机管理。
async fn publish_binding(
    probe: &Probe,
    agent: &AgentFixture,
    generation: Option<Uuid>,
    container_id: &str,
    facts: &ContainerEnvironmentFacts,
    snapshot: &AppliedReferenceSnapshot,
) -> Result<()> {
    let generation = generation.unwrap_or_else(|| agent.id());
    let binding = PersistedReferenceBinding {
        agent_id: agent.id(),
        project_id: agent.project.map(|kind| kind.id()),
        generation,
        container_id: container_id.to_string(),
        effective_image_id: facts.image_id.clone(),
        applied_revision: snapshot.configuration_revision,
        fingerprint: snapshot.fingerprint.clone(),
        snapshot: snapshot.clone(),
        updated_at: Utc::now(),
    };
    probe.store.publish_reference_binding(binding).await?;
    probe
        .applications
        .leases
        .publish(AgentEnvironmentLease::new(
            agent.id(),
            container_id,
            generation,
            std::sync::Arc::new(snapshot.clone()),
        ))
        .await?;
    Ok(())
}

fn volume_facts(facts: &ContainerEnvironmentFacts) -> Vec<VolumeFact> {
    facts
        .mounts
        .iter()
        .filter(|mount| mount.mount_type == mai_docker::ContainerMountType::Volume)
        .map(|mount| VolumeFact {
            name: mount
                .volume_name
                .clone()
                .unwrap_or_else(|| mount.source.clone()),
            target: mount.target.clone(),
            read_only: mount.read_only,
        })
        .collect()
}
