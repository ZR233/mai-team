use std::collections::HashSet;

use crate::inspect::ManagedContainer;
use crate::naming::PROJECT_SIDECAR_KIND;

pub(crate) fn orphaned_container_ids(
    containers: &[ManagedContainer],
    active_agent_ids: &HashSet<String>,
    active_project_ids: &HashSet<String>,
) -> Vec<String> {
    dedupe_container_ids(containers.iter().filter_map(|container| {
        let is_orphaned = if container.sidecar {
            container
                .project_id
                .as_ref()
                .is_none_or(|project_id| !active_project_ids.contains(project_id))
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
            container.agent_id.as_deref() == Some(agent_id)
                && container.matches_identifier(preferred_container_id)
        })
    {
        return Some(container);
    }

    containers
        .iter()
        .find(|container| container.agent_id.as_deref() == Some(agent_id))
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
        }
    }
}
