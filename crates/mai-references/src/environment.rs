//! 从已核验的实际卷身份构造换代计划；不把工作区和镜像可写层混为一谈。

use crate::{ReferenceError, Result};
use mai_docker::{
    AgentEnvironmentPlan, ContainerCreateOptions, ContainerEnvironmentFacts, ContainerMount,
    ContainerMountType, ContainerNetworkMode, ContainerVolumeMount, ReadOnlyBindMount,
    WorkspaceMountPlan,
};
use mai_protocol::{AgentId, AppliedReferenceSnapshot, ReferenceErrorCode};
use uuid::Uuid;

pub fn reference_mounts(snapshot: &AppliedReferenceSnapshot) -> Result<Vec<ContainerMount>> {
    snapshot
        .references
        .iter()
        .map(|reference| {
            ReadOnlyBindMount::new(&reference.host_path, &reference.container_path)
                .map(ContainerMount::ReadOnlyBind)
                .map_err(docker_error)
        })
        .collect()
}

/// 旧环境的未知 bind、tmpfs、附加 capability 不允许在换代中悄悄继承授权。
/// 所有合法卷（包括匿名卷）以 inspect 返回的真实名称进入最终计划。
pub fn plan_from_environment(
    agent_id: AgentId,
    generation: Option<Uuid>,
    image: &str,
    snapshot: &AppliedReferenceSnapshot,
    facts: &ContainerEnvironmentFacts,
) -> Result<AgentEnvironmentPlan> {
    if facts.agent_id.as_deref() != Some(agent_id.to_string().as_str())
        || facts.sidecar
        || facts.host.privileged
        || !facts.host.cap_add.is_empty()
    {
        return Err(ReferenceError::new(
            ReferenceErrorCode::EnvironmentApplyFailed,
            "既有容器身份或权限不满足安全换代条件",
        ));
    }
    let workspace = facts.mount_at("/workspace").ok_or_else(|| {
        ReferenceError::new(
            ReferenceErrorCode::EnvironmentRecoveryUnavailable,
            "既有容器缺少 workspace 卷身份",
        )
    })?;
    if workspace.mount_type != ContainerMountType::Volume {
        return Err(ReferenceError::new(
            ReferenceErrorCode::EnvironmentRecoveryUnavailable,
            "workspace 必须是可恢复的 Docker 卷",
        ));
    }
    let workspace = WorkspaceMountPlan::new(
        workspace.volume_name.clone().ok_or_else(|| {
            ReferenceError::new(
                ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                "workspace 卷名称缺失",
            )
        })?,
        workspace.volume_subpath.clone(),
    )
    .map_err(docker_error)?;
    let mut mounts = reference_mounts(snapshot)?;
    for mount in &facts.mounts {
        if mount.target == "/workspace" {
            continue;
        }
        match mount.mount_type {
            ContainerMountType::Bind if mount.target.starts_with("/references/") => {}
            ContainerMountType::Volume
                if mount.target != "/references" && !mount.target.starts_with("/references/") =>
            {
                let name = mount.volume_name.clone().ok_or_else(|| {
                    ReferenceError::new(
                        ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                        "数据卷缺少实际名称",
                    )
                })?;
                let volume = match (&mount.volume_subpath, mount.read_only) {
                    (Some(path), true) => {
                        ContainerVolumeMount::read_only_subpath(name, &mount.target, path)
                    }
                    (None, true) => ContainerVolumeMount::read_only(name, &mount.target),
                    (None, false) => ContainerVolumeMount::read_write(name, &mount.target),
                    (Some(_), false) => {
                        return Err(ReferenceError::new(
                            ReferenceErrorCode::EnvironmentRecoveryUnavailable,
                            "不能恢复未知的可写子路径卷",
                        ));
                    }
                }
                .map_err(docker_error)?;
                mounts.push(ContainerMount::Volume(volume));
            }
            ContainerMountType::Bind
            | ContainerMountType::Volume
            | ContainerMountType::Tmpfs
            | ContainerMountType::Other(_) => {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::EnvironmentApplyFailed,
                    format!("不能安全继承挂载 {}", mount.target),
                ));
            }
        }
    }
    let network = facts
        .host
        .network_mode
        .as_deref()
        .and_then(ContainerNetworkMode::from_docker_value)
        .ok_or_else(|| {
            ReferenceError::new(
                ReferenceErrorCode::EnvironmentApplyFailed,
                "不能继承未知网络模式",
            )
        })?;
    let options = ContainerCreateOptions {
        memory: facts
            .host
            .memory_bytes
            .filter(|value| *value > 0)
            .map(|value| value.to_string()),
        cpus: facts
            .host
            .nano_cpus
            .filter(|value| *value > 0)
            .map(|value| format!("{:.9}", value as f64 / 1_000_000_000.0)),
        pids_limit: facts
            .host
            .pids_limit
            .filter(|value| *value > 0)
            .map(u32::try_from)
            .transpose()
            .map_err(|error| {
                ReferenceError::new(ReferenceErrorCode::EnvironmentApplyFailed, "PID 限制不合法")
                    .with_cause(error)
            })?,
        cap_drop_all: facts
            .host
            .cap_drop
            .iter()
            .any(|value| value.eq_ignore_ascii_case("ALL")),
        no_new_privileges: facts
            .host
            .security_opt
            .iter()
            .any(|value| value == "no-new-privileges" || value == "no-new-privileges:true"),
    };
    let mut builder = AgentEnvironmentPlan::builder(
        agent_id.to_string(),
        if image == facts.image_id {
            &facts.image_reference
        } else {
            image
        },
        workspace,
    )
    .effective_image_id(image)
    .mounts(mounts)
    .options(options)
    .network(network);
    if !snapshot.project_id.is_nil() {
        builder = builder.project_id(snapshot.project_id.to_string());
    }
    if let Some(generation) = generation {
        builder = builder.generation(generation);
    }
    builder.build().map_err(docker_error)
}

pub(crate) fn docker_error(error: mai_docker::DockerError) -> ReferenceError {
    ReferenceError::new(
        ReferenceErrorCode::EnvironmentApplyFailed,
        error.to_string(),
    )
    .with_cause(error)
}
