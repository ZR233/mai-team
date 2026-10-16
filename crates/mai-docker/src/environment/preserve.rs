//! 环境换代时对既有挂载数据的保留规则。
//!
//! 保留分三类：
//! - 命名卷（含用户镜像声明的匿名卷、review 快照卷、缓存卷）：按 inspect 的真实名称原样复用；
//! - 旧 generation 的 `/references/*` bind：由新计划替换，因此不保留；
//! - 其它 bind、tmpfs 或未知类型：显式失败，不能盲目重新授权，也不能静默丢弃数据。

use std::collections::HashSet;

use crate::environment::plan::AgentEnvironmentPlan;
use crate::error::{DockerError, Result};
use crate::inspect::{ContainerEnvironmentFacts, ContainerMountFacts};
use crate::mount::{
    ContainerMount, ContainerMountType, ContainerVolumeMount, REFERENCE_MOUNT_ROOT,
};

/// 推导出创建新环境时需要原样复用的卷挂载。
pub fn preserved_volume_mounts(
    facts: &ContainerEnvironmentFacts,
    plan: &AgentEnvironmentPlan,
) -> Result<Vec<ContainerMount>> {
    let replaced = plan
        .creation_mounts(&[])?
        .into_iter()
        .map(|mount| mount.target().to_string())
        .collect::<HashSet<_>>();

    let mut preserved = Vec::new();
    for mount in &facts.mounts {
        if replaced.contains(&mount.target) {
            continue;
        }
        match mount.mount_type {
            ContainerMountType::Volume => {
                if within_reference_root(&mount.target) {
                    // 参考命名空间只允许由计划生成的只读挂载；既有卷属于未知来源。
                    return Err(DockerError::UnexpectedExistingMount(mount.target.clone()));
                }
                preserved.push(ContainerMount::Volume(volume_mount_from_facts(mount)?));
            }
            ContainerMountType::Bind => {
                if within_reference_root(&mount.target) {
                    // 旧 generation 的参考挂载由新计划替换。
                    continue;
                }
                return Err(DockerError::UnexpectedExistingMount(mount.target.clone()));
            }
            ContainerMountType::Tmpfs | ContainerMountType::Other(_) => {
                return Err(DockerError::UnexpectedExistingMount(mount.target.clone()));
            }
        }
    }
    Ok(preserved)
}

fn volume_mount_from_facts(mount: &ContainerMountFacts) -> Result<ContainerVolumeMount> {
    let name = mount
        .volume_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DockerError::UnexpectedExistingMount(mount.target.clone()))?;

    match mount.volume_subpath.as_deref() {
        // Docker 只允许 volume-subpath 与只读挂载组合，保持与创建端相同的形状。
        Some(subpath) => ContainerVolumeMount::read_only_subpath(name, &mount.target, subpath),
        None if mount.read_only => ContainerVolumeMount::read_only(name, &mount.target),
        None => ContainerVolumeMount::read_write(name, &mount.target),
    }
}

fn within_reference_root(target: &str) -> bool {
    target == REFERENCE_MOUNT_ROOT || target.starts_with(&format!("{REFERENCE_MOUNT_ROOT}/"))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::*;
    use crate::environment::plan::WorkspaceMountPlan;
    use crate::inspect::ContainerHostFacts;

    const GENERATION: &str = "8f058410-c69e-45d6-a10d-d204db72b30a";

    fn plan(mounts: Vec<ContainerMount>) -> AgentEnvironmentPlan {
        AgentEnvironmentPlan::builder(
            "agent-1",
            "ghcr.io/example/agent:latest",
            WorkspaceMountPlan::new("mai-team-workspace-agent-1", None::<String>)
                .expect("workspace"),
        )
        .effective_image_id("sha256:effective")
        .generation(Uuid::parse_str(GENERATION).expect("uuid"))
        .mounts(mounts)
        .build()
        .expect("plan")
    }

    fn volume_fact(target: &str, name: &str, read_only: bool) -> ContainerMountFacts {
        ContainerMountFacts {
            target: target.to_string(),
            mount_type: ContainerMountType::Volume,
            source: name.to_string(),
            read_only,
            propagation: None,
            recursive_read_only: None,
            volume_name: Some(name.to_string()),
            volume_subpath: None,
        }
    }

    fn bind_fact(target: &str, read_only: bool) -> ContainerMountFacts {
        ContainerMountFacts {
            target: target.to_string(),
            mount_type: ContainerMountType::Bind,
            source: "/srv/mai-references/linux/repo".to_string(),
            read_only,
            propagation: Some("rprivate".to_string()),
            recursive_read_only: Some(true),
            volume_name: None,
            volume_subpath: None,
        }
    }

    fn facts(mounts: Vec<ContainerMountFacts>) -> ContainerEnvironmentFacts {
        ContainerEnvironmentFacts {
            id: "abc123".to_string(),
            name: "mai-team-agent-1-g8f058410c69e".to_string(),
            state: "exited".to_string(),
            running: false,
            image_reference: "ghcr.io/example/agent:latest".to_string(),
            image_id: "sha256:effective".to_string(),
            agent_id: Some("agent-1".to_string()),
            project_id: None,
            generation: Some(Uuid::parse_str(GENERATION).expect("uuid")),
            sidecar: false,
            sidecar_kind: None,
            mounts,
            host: ContainerHostFacts::default(),
        }
    }

    #[test]
    fn preserves_workspace_anonymous_and_review_volumes() {
        let plan = plan(vec![ContainerMount::Volume(
            ContainerVolumeMount::read_only_subpath(
                "mai-team-project-1-cache",
                "/project/repo",
                "review-contexts/run-1/repo",
            )
            .expect("review mount"),
        )]);
        let facts = facts(vec![
            volume_fact("/workspace", "mai-team-workspace-agent-1", false),
            volume_fact("/var/lib/postgresql/data", "anonymous-data", false),
            ContainerMountFacts {
                volume_subpath: Some("review-contexts/run-1/repo".to_string()),
                ..volume_fact("/project/repo", "mai-team-project-1-cache", true)
            },
        ]);

        let preserved = preserved_volume_mounts(&facts, &plan).expect("preserved volumes");

        assert_eq!(preserved.len(), 1);
        let preserved = preserved[0].volume().expect("volume mount");
        assert_eq!(preserved.volume(), "anonymous-data");
        assert_eq!(preserved.target(), "/var/lib/postgresql/data");
        assert_eq!(
            preserved.access(),
            crate::mount::ContainerMountAccess::ReadWrite
        );
    }

    #[test]
    fn drops_old_generation_reference_binds_but_keeps_new_plan() {
        let plan = plan(vec![ContainerMount::ReadOnlyBind(
            crate::mount::ReadOnlyBindMount::for_reference(
                "/srv/mai-references/linux/repo",
                "linux",
            )
            .expect("reference mount"),
        )]);
        let facts = facts(vec![
            volume_fact("/workspace", "mai-team-workspace-agent-1", false),
            bind_fact("/references/linux", true),
        ]);

        let preserved = preserved_volume_mounts(&facts, &plan).expect("preserved volumes");

        assert!(preserved.is_empty(), "参考 bind 由新计划提供，不得重复保留");
    }

    #[test]
    fn rejects_unknown_binds_tmpfs_and_reference_volumes() {
        let plan = plan(Vec::new());

        let unknown_bind = facts(vec![
            volume_fact("/workspace", "mai-team-workspace-agent-1", false),
            bind_fact("/data/share", true),
        ]);
        assert!(matches!(
            preserved_volume_mounts(&unknown_bind, &plan),
            Err(DockerError::UnexpectedExistingMount(target)) if target == "/data/share"
        ));

        let tmpfs = facts(vec![
            volume_fact("/workspace", "mai-team-workspace-agent-1", false),
            ContainerMountFacts {
                mount_type: ContainerMountType::Tmpfs,
                ..bind_fact("/tmp/scratch", false)
            },
        ]);
        assert!(matches!(
            preserved_volume_mounts(&tmpfs, &plan),
            Err(DockerError::UnexpectedExistingMount(target)) if target == "/tmp/scratch"
        ));

        let reference_volume = facts(vec![
            volume_fact("/workspace", "mai-team-workspace-agent-1", false),
            volume_fact("/references/linux", "unexpected-volume", true),
        ]);
        assert!(matches!(
            preserved_volume_mounts(&reference_volume, &plan),
            Err(DockerError::UnexpectedExistingMount(target)) if target == "/references/linux"
        ));
    }

    #[test]
    fn rejects_volume_without_a_real_name() {
        let plan = plan(Vec::new());
        let facts = facts(vec![
            volume_fact("/workspace", "mai-team-workspace-agent-1", false),
            ContainerMountFacts {
                volume_name: None,
                ..volume_fact("/var/lib/postgresql/data", "anonymous-data", false)
            },
        ]);

        assert!(matches!(
            preserved_volume_mounts(&facts, &plan),
            Err(DockerError::UnexpectedExistingMount(target))
                if target == "/var/lib/postgresql/data"
        ));
    }
}
