//! T27 的 Docker 环境事实采集与卷身份判定。

use mai_docker::{ContainerEnvironmentFacts, ContainerMountType, ExistingEnvironment};
use mai_protocol::AgentId;
use mai_references::plan_from_environment;
use mai_store::PersistedReferenceBinding;

use crate::error::{Result, error};
use crate::host::RuntimeHost;

/// 绑定、真实容器与按该绑定重建计划后的 `inspect_environment` 结论。
#[derive(Debug, Clone)]
pub(super) struct BindingEnvironment {
    pub(super) facts: ContainerEnvironmentFacts,
    pub(super) binding: PersistedReferenceBinding,
    pub(super) inspected: ExistingEnvironment,
}

/// 一个实际 Docker volume 挂载的身份投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VolumeIdentity {
    pub(super) target: String,
    pub(super) source: String,
    pub(super) name: String,
    pub(super) read_write: bool,
}

impl VolumeIdentity {
    /// 是否精确匹配已知卷名。
    pub(super) fn matches_name(&self, expected: &str) -> bool {
        self.read_write && self.source == expected && self.name == expected
    }

    /// Docker 为镜像声明卷生成的匿名名通常是 64 位十六进制随机 ID。
    pub(super) fn is_generated_anonymous(&self) -> bool {
        self.read_write
            && self.source == self.name
            && self.name.len() == 64
            && self.name.bytes().all(|byte| byte.is_ascii_hexdigit())
    }
}

/// 用持久绑定快照重建期望环境，再走真实 `DockerClient.inspect_environment`。
pub(super) async fn inspect_binding(
    host: &RuntimeHost,
    agent_id: AgentId,
    container_id: &str,
) -> Result<BindingEnvironment> {
    let binding = host
        .store
        .reference_binding(agent_id)
        .await
        .map_err(|cause| {
            error(
                "t27_binding_unreadable",
                format!("读取环境绑定失败：{cause}"),
            )
        })?
        .ok_or_else(|| {
            error(
                "t27_binding_missing",
                format!("Agent {agent_id} 没有环境绑定"),
            )
        })?;
    let facts = host.container_facts(container_id).await?;
    let generation = facts.generation.map(|_| binding.generation);
    let plan = plan_from_environment(
        agent_id,
        generation,
        &binding.effective_image_id,
        &binding.snapshot,
        &facts,
    )
    .map_err(|cause| {
        error(
            "t27_environment_plan_failed",
            format!("从绑定快照重建环境计划失败：{cause}"),
        )
    })?;
    let inspected = host
        .docker
        .inspect_environment(&plan)
        .await
        .map_err(|cause| {
            error(
                "t27_inspect_environment_failed",
                format!("DockerClient.inspect_environment 失败：{cause}"),
            )
        })?;
    Ok(BindingEnvironment {
        facts,
        binding,
        inspected,
    })
}

/// 提取一个可读写、source/name 一致且无子路径的真实 volume 身份。
pub(super) fn volume_identity(
    facts: &ContainerEnvironmentFacts,
    target: &str,
) -> Option<VolumeIdentity> {
    let mount = facts.mount_at(target)?;
    if mount.mount_type != ContainerMountType::Volume
        || mount.read_only
        || mount.volume_subpath.is_some()
    {
        return None;
    }
    let name = mount.volume_name.as_deref()?.trim();
    if name.is_empty() || mount.source.trim() != name {
        return None;
    }
    Some(VolumeIdentity {
        target: mount.target.clone(),
        source: mount.source.trim().to_string(),
        name: name.to_string(),
        read_write: !mount.read_only,
    })
}
