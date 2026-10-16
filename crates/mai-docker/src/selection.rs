use std::collections::HashSet;

use uuid::Uuid;

use crate::error::{DockerError, Result};
use crate::inspect::ManagedContainer;
use crate::naming::{AGENT_MCP_SIDECAR_KIND, PROJECT_SIDECAR_KIND};

/// 定向删除时对“ID 已经不存在”的处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRemovalPolicy {
    /// 未知 ID 视为错误；用于必须完成删除的清理步骤。
    RequireManaged,
    /// 已消失的 ID 视为已完成；用于崩溃后重试同一次清理。
    TolerateMissing,
}

/// 定位某个计划对应的候选 Agent 容器。
///
/// 指定 generation 时只接受同一 generation 的容器；未指定 generation 时优先已发布
/// 的容器 ID，其次任意属于该 Agent 的非 sidecar 容器。sidecar 永远不是候选。
pub(crate) fn find_environment_candidate<'a>(
    containers: &'a [ManagedContainer],
    agent_id: &str,
    generation: Option<Uuid>,
    published_container_id: Option<&str>,
) -> Option<&'a ManagedContainer> {
    let Some(generation) = generation else {
        // 未指定 generation 时沿用既有选择语义（优先已发布 ID，其次标签回退）。
        return find_reusable_agent_container(containers, agent_id, published_container_id);
    };

    let matches_generation =
        |container: &ManagedContainer| container.generation == Some(generation);
    let published = published_container_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(published) = published
        && let Some(container) = containers.iter().find(|container| {
            !container.sidecar
                && matches_generation(container)
                && container.matches_identifier(published)
        })
    {
        return Some(container);
    }
    containers.iter().find(|container| {
        !container.sidecar
            && container.agent_id.as_deref() == Some(agent_id)
            && matches_generation(container)
    })
}

/// 把精确 ID 解析成实际存在的受管容器 ID。
///
/// 未出现在受管列表中的 ID 一律不删除：既避免删掉非本项目的容器，也避免把名字前缀
/// 当成授权。受保护 ID（旧候选、旧快照来源）同样拒绝。
pub(crate) fn exact_container_removal_targets(
    containers: &[ManagedContainer],
    requested_ids: &[String],
    protected_ids: &[String],
    policy: ContainerRemovalPolicy,
) -> Result<Vec<String>> {
    let mut targets = Vec::new();
    for requested in requested_ids {
        let requested = requested.trim();
        if requested.is_empty() {
            continue;
        }
        // 先要求完整 ID/名字相等，再允许受管容器内的前缀匹配，避免前缀歧义。
        let container = containers
            .iter()
            .find(|container| container.id == requested || container.name == requested)
            .or_else(|| {
                containers
                    .iter()
                    .find(|container| container.matches_identifier(requested))
            });
        let Some(container) = container else {
            if policy == ContainerRemovalPolicy::TolerateMissing {
                continue;
            }
            return Err(DockerError::UnknownContainer(requested.to_string()));
        };
        if protected_ids.iter().any(|protected| {
            let protected = protected.trim();
            !protected.is_empty() && container.matches_identifier(protected)
        }) {
            return Err(DockerError::ProtectedContainer(container.id.clone()));
        }
        if !targets.contains(&container.id) {
            targets.push(container.id.clone());
        }
    }
    Ok(targets)
}

pub(crate) fn orphaned_container_ids(
    containers: &[ManagedContainer],
    active_agent_ids: &HashSet<String>,
    active_project_ids: &HashSet<String>,
) -> Vec<String> {
    dedupe_container_ids(containers.iter().filter_map(|container| {
        let is_orphaned = if container.sidecar {
            if container.sidecar_kind.as_deref() == Some(AGENT_MCP_SIDECAR_KIND) {
                container
                    .agent_id
                    .as_ref()
                    .is_none_or(|agent_id| !active_agent_ids.contains(agent_id))
            } else {
                container
                    .project_id
                    .as_ref()
                    .is_none_or(|project_id| !active_project_ids.contains(project_id))
            }
        } else {
            container
                .agent_id
                .as_ref()
                .is_none_or(|agent_id| !active_agent_ids.contains(agent_id))
        };
        is_orphaned.then(|| container.id.clone())
    }))
}

pub(crate) fn agent_container_delete_ids(
    containers: &[ManagedContainer],
    agent_id: &str,
    preferred_container_id: Option<&str>,
) -> Vec<String> {
    let preferred = preferred_container_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let labeled = containers
        .iter()
        .filter(|container| container.agent_id.as_deref() == Some(agent_id))
        .map(|container| container.id.clone());
    dedupe_container_ids(preferred.into_iter().chain(labeled))
}

pub(crate) fn find_reusable_agent_container<'a>(
    containers: &'a [ManagedContainer],
    agent_id: &str,
    preferred_container_id: Option<&str>,
) -> Option<&'a ManagedContainer> {
    if let Some(preferred_container_id) = preferred_container_id
        && let Some(container) = containers.iter().find(|container| {
            !container.sidecar
                && container.agent_id.as_deref() == Some(agent_id)
                && container.matches_identifier(preferred_container_id)
        })
    {
        return Some(container);
    }

    containers
        .iter()
        .find(|container| !container.sidecar && container.agent_id.as_deref() == Some(agent_id))
}

pub(crate) fn project_sidecar_container_delete_ids(
    containers: &[ManagedContainer],
    project_id: &str,
    preferred_container_id: Option<&str>,
) -> Vec<String> {
    let preferred = preferred_container_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let labeled = containers
        .iter()
        .filter(|container| {
            container.sidecar
                && container.sidecar_kind.as_deref() == Some(PROJECT_SIDECAR_KIND)
                && container.project_id.as_deref() == Some(project_id)
        })
        .map(|container| container.id.clone());
    dedupe_container_ids(preferred.into_iter().chain(labeled))
}

pub(crate) fn find_reusable_project_sidecar_container<'a>(
    containers: &'a [ManagedContainer],
    project_id: &str,
    preferred_container_id: Option<&str>,
) -> Option<&'a ManagedContainer> {
    let matches_project = |container: &&ManagedContainer| {
        container.sidecar
            && container.sidecar_kind.as_deref() == Some(PROJECT_SIDECAR_KIND)
            && container.project_id.as_deref() == Some(project_id)
    };

    if let Some(preferred_container_id) = preferred_container_id
        && let Some(container) = containers.iter().find(|container| {
            matches_project(container) && container.matches_identifier(preferred_container_id)
        })
    {
        return Some(container);
    }

    containers.iter().find(matches_project)
}

fn dedupe_container_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphaned_container_ids_include_missing_agent_labels_and_dedupe() {
        let containers = vec![
            managed("keep", Some("agent-1")),
            managed("orphan", Some("deleted-agent")),
            managed("missing-label", None),
            managed("orphan", Some("deleted-agent")),
        ];
        let active_agent_ids = HashSet::from(["agent-1".to_string()]);

        assert_eq!(
            orphaned_container_ids(&containers, &active_agent_ids, &HashSet::new()),
            vec!["orphan".to_string(), "missing-label".to_string()]
        );
    }

    #[test]
    fn orphaned_container_ids_keep_active_project_sidecars() {
        let containers = vec![
            managed("agent-keep", Some("agent-1")),
            managed_project_sidecar("sidecar-keep", "project-1", "agent-1"),
            managed_project_sidecar("sidecar-orphan", "project-2", "agent-2"),
            managed_project_sidecar("sidecar-orphan", "project-2", "agent-2"),
        ];
        let active_agent_ids = HashSet::from(["agent-1".to_string()]);
        let active_project_ids = HashSet::from(["project-1".to_string()]);

        assert_eq!(
            orphaned_container_ids(&containers, &active_agent_ids, &active_project_ids),
            vec!["sidecar-orphan".to_string()]
        );
    }

    #[test]
    fn orphaned_container_ids_use_agent_lifecycle_for_mcp_sidecars() {
        let containers = vec![
            managed_agent_mcp_sidecar("mcp-keep", Some("agent-1")),
            managed_agent_mcp_sidecar("mcp-orphan", Some("deleted-agent")),
            managed_agent_mcp_sidecar("mcp-missing-label", None),
        ];
        let active_agent_ids = HashSet::from(["agent-1".to_string()]);

        assert_eq!(
            orphaned_container_ids(&containers, &active_agent_ids, &HashSet::new()),
            vec!["mcp-orphan".to_string(), "mcp-missing-label".to_string()]
        );
    }

    #[test]
    fn agent_container_delete_ids_use_preferred_id_and_label_fallback() {
        let containers = vec![
            managed("owned-1", Some("agent-1")),
            managed("other", Some("agent-2")),
            managed("owned-2", Some("agent-1")),
            managed("owned-1", Some("agent-1")),
        ];

        assert_eq!(
            agent_container_delete_ids(&containers, "agent-1", Some("persisted")),
            vec![
                "persisted".to_string(),
                "owned-1".to_string(),
                "owned-2".to_string()
            ]
        );
        assert_eq!(
            agent_container_delete_ids(&containers, "agent-1", Some("owned-1")),
            vec!["owned-1".to_string(), "owned-2".to_string()]
        );
    }

    #[test]
    fn reusable_container_prefers_matching_persisted_container() {
        let containers = vec![
            managed("wrong-owner", Some("agent-2")),
            managed("owned-fallback", Some("agent-1")),
            managed("owned-preferred", Some("agent-1")),
        ];

        assert_eq!(
            find_reusable_agent_container(&containers, "agent-1", Some("owned-preferred"))
                .map(|container| container.id.as_str()),
            Some("owned-preferred")
        );
        assert_eq!(
            find_reusable_agent_container(&containers, "agent-1", Some("wrong-owner"))
                .map(|container| container.id.as_str()),
            Some("owned-fallback")
        );
    }

    #[test]
    fn reusable_container_never_selects_agent_mcp_sidecar() {
        let containers = vec![
            managed_agent_mcp_sidecar("mcp-sidecar", Some("agent-1")),
            managed("agent-container", Some("agent-1")),
        ];

        assert_eq!(
            find_reusable_agent_container(&containers, "agent-1", Some("mcp-sidecar"))
                .map(|container| container.id.as_str()),
            Some("agent-container")
        );
    }

    #[test]
    fn project_sidecar_delete_ids_use_preferred_id_and_label_fallback() {
        let containers = vec![
            managed_project_sidecar("project-owned-1", "project-1", "agent-1"),
            managed_project_sidecar("project-owned-2", "project-1", "agent-1"),
            managed_project_sidecar("other-project", "project-2", "agent-2"),
        ];

        assert_eq!(
            project_sidecar_container_delete_ids(&containers, "project-1", Some("persisted")),
            vec![
                "persisted".to_string(),
                "project-owned-1".to_string(),
                "project-owned-2".to_string()
            ]
        );
        assert_eq!(
            project_sidecar_container_delete_ids(&containers, "project-1", Some("project-owned-1")),
            vec!["project-owned-1".to_string(), "project-owned-2".to_string()]
        );
    }

    #[test]
    fn reusable_project_sidecar_prefers_matching_persisted_container() {
        let containers = vec![
            managed_project_sidecar("other-project", "project-2", "agent-2"),
            managed_project_sidecar("project-fallback", "project-1", "agent-1"),
            managed_project_sidecar("project-preferred", "project-1", "agent-1"),
        ];

        assert_eq!(
            find_reusable_project_sidecar_container(
                &containers,
                "project-1",
                Some("project-preferred")
            )
            .map(|container| container.id.as_str()),
            Some("project-preferred")
        );
        assert_eq!(
            find_reusable_project_sidecar_container(
                &containers,
                "project-1",
                Some("other-project")
            )
            .map(|container| container.id.as_str()),
            Some("project-fallback")
        );
    }

    fn managed(id: &str, agent_id: Option<&str>) -> ManagedContainer {
        ManagedContainer {
            id: id.to_string(),
            name: format!("mai-team-{id}"),
            image: "ubuntu:latest".to_string(),
            state: "running".to_string(),
            agent_id: agent_id.map(str::to_string),
            project_id: None,
            sidecar: false,
            sidecar_kind: None,
            generation: None,
        }
    }

    fn managed_project_sidecar(id: &str, project_id: &str, agent_id: &str) -> ManagedContainer {
        ManagedContainer {
            id: id.to_string(),
            name: format!("mai-team-project-sidecar-{project_id}"),
            image: "ghcr.io/zr233/mai-team-sidecar:latest".to_string(),
            state: "running".to_string(),
            agent_id: Some(agent_id.to_string()),
            project_id: Some(project_id.to_string()),
            sidecar: true,
            sidecar_kind: Some(PROJECT_SIDECAR_KIND.to_string()),
            generation: None,
        }
    }

    fn managed_agent_mcp_sidecar(id: &str, agent_id: Option<&str>) -> ManagedContainer {
        ManagedContainer {
            id: id.to_string(),
            name: format!("mai-team-agent-mcp-{}", agent_id.unwrap_or("unknown")),
            image: "ghcr.io/zr233/mai-team-sidecar:latest".to_string(),
            state: "running".to_string(),
            agent_id: agent_id.map(str::to_string),
            project_id: None,
            sidecar: true,
            sidecar_kind: Some(AGENT_MCP_SIDECAR_KIND.to_string()),
            generation: None,
        }
    }

    #[test]
    fn environment_candidate_requires_the_expected_generation() {
        let generation = Uuid::parse_str("8f058410-c69e-45d6-a10d-d204db72b30a").expect("uuid");
        let containers = vec![
            managed_with_generation("old-generation", Some("agent-1"), None),
            managed_with_generation("new-generation", Some("agent-1"), Some(generation)),
            managed_with_generation("other-agent", Some("agent-2"), Some(generation)),
        ];

        assert_eq!(
            find_environment_candidate(&containers, "agent-1", Some(generation), None)
                .map(|container| container.id.as_str()),
            Some("new-generation")
        );
        assert_eq!(
            find_environment_candidate(&containers, "agent-1", None, Some("old-generation"))
                .map(|container| container.id.as_str()),
            Some("old-generation")
        );
        assert_eq!(
            find_environment_candidate(&containers, "agent-3", Some(generation), None),
            None
        );
    }

    #[test]
    fn exact_removal_never_deletes_unknown_or_protected_containers() {
        let containers = vec![managed("owned-1", Some("agent-1"))];

        assert!(matches!(
            exact_container_removal_targets(
                &containers,
                &["not-managed".to_string()],
                &[],
                ContainerRemovalPolicy::RequireManaged
            ),
            Err(DockerError::UnknownContainer(id)) if id == "not-managed"
        ));
        assert!(matches!(
            exact_container_removal_targets(
                &containers,
                &["owned-1".to_string()],
                &["owned-1".to_string()],
                ContainerRemovalPolicy::RequireManaged
            ),
            Err(DockerError::ProtectedContainer(id)) if id == "owned-1"
        ));
        assert_eq!(
            exact_container_removal_targets(
                &containers,
                &["owned-1".to_string(), "already-gone".to_string()],
                &[],
                ContainerRemovalPolicy::TolerateMissing
            )
            .expect("tolerate missing"),
            vec!["owned-1".to_string()]
        );
    }

    fn managed_with_generation(
        id: &str,
        agent_id: Option<&str>,
        generation: Option<Uuid>,
    ) -> ManagedContainer {
        let mut container = managed(id, agent_id);
        container.generation = generation;
        container
    }
}
