use std::collections::{HashMap, HashSet};

use mai_docker::{
    DockerClient, ManagedVolume, project_agent_workspace_volume, project_cache_volume,
};
use mai_protocol::{
    AgentId, AgentResourceState, AgentSummary, ProjectCloneStatus, ProjectId, ProjectStatus,
    ProjectSummary,
};

use crate::{Result, RuntimeError};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DockerVolumeReconcileReport {
    pub(crate) orphan_agent_workspace_volumes_removed: Vec<String>,
    pub(crate) orphan_agent_workspace_volume_removal_failed: Vec<String>,
    pub(crate) orphan_project_cache_volumes_removed: Vec<String>,
    pub(crate) orphan_project_cache_volume_removal_failed: Vec<String>,
    pub(crate) legacy_agent_workspace_volumes_present: Vec<String>,
    pub(crate) legacy_project_cache_volumes_present: Vec<String>,
    pub(crate) quarantined_volumes: Vec<String>,
    pub(crate) attached_orphan_volumes: Vec<String>,
    pub(crate) missing_project_cache_volumes: Vec<ProjectId>,
    pub(crate) missing_agent_workspace_volumes: Vec<AgentId>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DockerVolumeReconcilePlan {
    orphan_agent_workspace_volumes: Vec<String>,
    orphan_project_cache_volumes: Vec<String>,
    quarantined_volumes: Vec<String>,
    missing_project_cache_volumes: Vec<ProjectId>,
    missing_agent_workspace_volumes: Vec<AgentId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaiVolumeOwner {
    ProjectCache(ProjectId),
    ProjectAgentWorkspace(ProjectId, AgentId),
    LegacyAgentWorkspace(AgentId),
    LegacyProject(ProjectId),
    LegacyProjectReview(ProjectId),
}

pub(crate) async fn reconcile_project_volumes(
    docker: &DockerClient,
    live_projects: &[ProjectSummary],
    live_agents: &[AgentSummary],
) -> Result<DockerVolumeReconcileReport> {
    let volumes = docker.list_managed_volumes().await?;
    let plan = plan_project_volume_reconcile(&volumes, live_projects, live_agents);
    let live_agent_projects = live_agents
        .iter()
        .filter_map(|agent| agent.project_id.map(|project_id| (agent.id, project_id)))
        .collect::<HashMap<_, _>>();
    let mut legacy_project_cache_volumes_present = Vec::new();
    let mut missing_project_cache_volumes = Vec::new();
    for project_id in plan.missing_project_cache_volumes {
        let volume = project_cache_volume(&project_id.to_string());
        if docker.volume_exists(&volume).await? {
            legacy_project_cache_volumes_present.push(volume);
        } else {
            missing_project_cache_volumes.push(project_id);
        }
    }
    let mut legacy_agent_workspace_volumes_present = Vec::new();
    let mut missing_agent_workspace_volumes = Vec::new();
    for agent_id in plan.missing_agent_workspace_volumes {
        let Some(project_id) = live_agent_projects.get(&agent_id) else {
            missing_agent_workspace_volumes.push(agent_id);
            continue;
        };
        let volume = project_agent_workspace_volume(&project_id.to_string(), &agent_id.to_string());
        if docker.volume_exists(&volume).await? {
            legacy_agent_workspace_volumes_present.push(volume);
        } else {
            missing_agent_workspace_volumes.push(agent_id);
        }
    }

    let mut report = DockerVolumeReconcileReport {
        legacy_agent_workspace_volumes_present,
        legacy_project_cache_volumes_present,
        quarantined_volumes: plan.quarantined_volumes,
        missing_project_cache_volumes,
        missing_agent_workspace_volumes,
        ..DockerVolumeReconcileReport::default()
    };

    for volume in plan.orphan_agent_workspace_volumes {
        if docker.volume_is_attached(&volume).await? {
            tracing::warn!(
                volume,
                "quarantined attached orphan Mai agent workspace volume"
            );
            report.attached_orphan_volumes.push(volume);
            continue;
        }
        match docker.delete_volume(&volume).await {
            Ok(()) => report.orphan_agent_workspace_volumes_removed.push(volume),
            Err(err) => {
                tracing::warn!(
                    volume,
                    "failed to remove orphan agent workspace volume during startup reconcile: {err}"
                );
                report
                    .orphan_agent_workspace_volume_removal_failed
                    .push(volume);
            }
        }
    }
    for volume in plan.orphan_project_cache_volumes {
        if docker.volume_is_attached(&volume).await? {
            tracing::warn!(volume, "quarantined attached orphan Mai project volume");
            report.attached_orphan_volumes.push(volume);
            continue;
        }
        match docker.delete_volume(&volume).await {
            Ok(()) => report.orphan_project_cache_volumes_removed.push(volume),
            Err(err) => {
                tracing::warn!(
                    volume,
                    "failed to remove orphan project cache volume during startup reconcile: {err}"
                );
                report
                    .orphan_project_cache_volume_removal_failed
                    .push(volume);
            }
        }
    }

    report.orphan_agent_workspace_volumes_removed.sort();
    report.orphan_agent_workspace_volume_removal_failed.sort();
    report.orphan_project_cache_volumes_removed.sort();
    report.orphan_project_cache_volume_removal_failed.sort();
    report.legacy_agent_workspace_volumes_present.sort();
    report.legacy_project_cache_volumes_present.sort();
    report.quarantined_volumes.sort();
    report.attached_orphan_volumes.sort();
    report.missing_project_cache_volumes.sort();
    report.missing_agent_workspace_volumes.sort();
    Ok(report)
}

pub(crate) async fn delete_project_volumes(
    docker: &DockerClient,
    project_id: ProjectId,
) -> Result<()> {
    for volume in docker.list_managed_volumes().await? {
        let Some(owner) = parse_mai_volume_owner(&volume.name) else {
            continue;
        };
        if !volume_labels_match_owner(&volume, owner) || owner.project_id() != Some(project_id) {
            continue;
        }
        if docker.volume_is_attached(&volume.name).await? {
            return Err(RuntimeError::InvalidInput(format!(
                "refusing to delete attached project volume {}",
                volume.name
            )));
        }
        docker.delete_volume(&volume.name).await?;
    }
    Ok(())
}

impl MaiVolumeOwner {
    fn project_id(self) -> Option<ProjectId> {
        match self {
            Self::ProjectCache(project_id)
            | Self::ProjectAgentWorkspace(project_id, _)
            | Self::LegacyProject(project_id)
            | Self::LegacyProjectReview(project_id) => Some(project_id),
            Self::LegacyAgentWorkspace(_) => None,
        }
    }
}

fn plan_project_volume_reconcile(
    volumes: &[ManagedVolume],
    live_projects: &[ProjectSummary],
    live_agents: &[AgentSummary],
) -> DockerVolumeReconcilePlan {
    let live_project_ids = live_projects
        .iter()
        .map(|project| project.id)
        .collect::<HashSet<_>>();
    let live_projects_by_id = live_projects
        .iter()
        .map(|project| (project.id, project))
        .collect::<HashMap<_, _>>();
    let live_agent_projects = live_agents
        .iter()
        .filter_map(|agent| agent.project_id.map(|project_id| (agent.id, project_id)))
        .collect::<HashMap<_, _>>();
    let mut present_project_cache_volumes = HashSet::new();
    let mut present_agent_workspace_volumes = HashSet::new();
    let mut orphan_project_cache_volumes = Vec::new();
    let mut orphan_agent_workspace_volumes = Vec::new();
    let mut quarantined_volumes = Vec::new();

    for volume in volumes {
        let Some(owner) = parse_mai_volume_owner(&volume.name) else {
            quarantined_volumes.push(volume.name.clone());
            continue;
        };
        if !volume_labels_match_owner(volume, owner) {
            quarantined_volumes.push(volume.name.clone());
            continue;
        }
        match owner {
            MaiVolumeOwner::ProjectCache(project_id) => {
                if live_project_ids.contains(&project_id) {
                    present_project_cache_volumes.insert(volume.name.clone());
                } else {
                    orphan_project_cache_volumes.push(volume.name.clone());
                }
            }
            MaiVolumeOwner::ProjectAgentWorkspace(project_id, agent_id) => {
                if live_agent_projects.get(&agent_id) == Some(&project_id) {
                    present_agent_workspace_volumes.insert(volume.name.clone());
                } else {
                    orphan_agent_workspace_volumes.push(volume.name.clone());
                }
            }
            MaiVolumeOwner::LegacyAgentWorkspace(agent_id) => {
                if !live_agents.iter().any(|agent| agent.id == agent_id) {
                    orphan_agent_workspace_volumes.push(volume.name.clone());
                }
            }
            MaiVolumeOwner::LegacyProject(project_id)
            | MaiVolumeOwner::LegacyProjectReview(project_id) => {
                if !live_project_ids.contains(&project_id) {
                    orphan_project_cache_volumes.push(volume.name.clone());
                }
            }
        }
    }

    let mut missing_project_cache_volumes = live_projects
        .iter()
        .filter(|project| project_workspace_should_exist(project))
        .filter_map(|project| {
            let volume = project_cache_volume(&project.id.to_string());
            (!present_project_cache_volumes.contains(&volume)).then_some(project.id)
        })
        .collect::<Vec<_>>();
    let mut missing_agent_workspace_volumes = live_agents
        .iter()
        .filter_map(|agent| {
            let project_id = agent.project_id?;
            let project = live_projects_by_id.get(&project_id)?;
            if !agent_workspace_should_exist(agent) || !project_workspace_should_exist(project) {
                return None;
            }
            let volume =
                project_agent_workspace_volume(&project_id.to_string(), &agent.id.to_string());
            (!present_agent_workspace_volumes.contains(&volume)).then_some(agent.id)
        })
        .collect::<Vec<_>>();

    orphan_project_cache_volumes.sort();
    orphan_agent_workspace_volumes.sort();
    quarantined_volumes.sort();
    missing_project_cache_volumes.sort();
    missing_agent_workspace_volumes.sort();

    DockerVolumeReconcilePlan {
        orphan_agent_workspace_volumes,
        orphan_project_cache_volumes,
        quarantined_volumes,
        missing_project_cache_volumes,
        missing_agent_workspace_volumes,
    }
}

fn parse_mai_volume_owner(name: &str) -> Option<MaiVolumeOwner> {
    if let Some(agent_id) = name.strip_prefix("mai-team-workspace-") {
        return AgentId::parse_str(agent_id)
            .ok()
            .map(MaiVolumeOwner::LegacyAgentWorkspace);
    }
    if let Some(project_id) = name.strip_prefix("mai-team-project-review-") {
        return ProjectId::parse_str(project_id)
            .ok()
            .map(MaiVolumeOwner::LegacyProjectReview);
    }
    let remainder = name.strip_prefix("mai-team-project-")?;
    let project_id_text = remainder.get(..36)?;
    let project_id = ProjectId::parse_str(project_id_text).ok()?;
    match remainder.get(36..)? {
        "" => Some(MaiVolumeOwner::LegacyProject(project_id)),
        "-cache" => Some(MaiVolumeOwner::ProjectCache(project_id)),
        agent_suffix if agent_suffix.starts_with("-agent-") => {
            let agent_id = AgentId::parse_str(agent_suffix.trim_start_matches("-agent-")).ok()?;
            Some(MaiVolumeOwner::ProjectAgentWorkspace(project_id, agent_id))
        }
        _ => None,
    }
}

fn volume_labels_match_owner(volume: &ManagedVolume, owner: MaiVolumeOwner) -> bool {
    let (expected_kind, project_id, agent_id) = match owner {
        MaiVolumeOwner::ProjectCache(project_id) => (Some("project-cache"), Some(project_id), None),
        MaiVolumeOwner::ProjectAgentWorkspace(project_id, agent_id) => {
            (Some("agent-workspace"), Some(project_id), Some(agent_id))
        }
        MaiVolumeOwner::LegacyAgentWorkspace(agent_id) => {
            (Some("agent-workspace"), None, Some(agent_id))
        }
        MaiVolumeOwner::LegacyProject(project_id)
        | MaiVolumeOwner::LegacyProjectReview(project_id) => (None, Some(project_id), None),
    };
    if let Some(kind) = volume.kind.as_deref()
        && Some(kind) != expected_kind
    {
        return false;
    }
    if let Some(label) = volume.project_id.as_deref()
        && Some(label) != project_id.map(|id| id.to_string()).as_deref()
    {
        return false;
    }
    if let Some(label) = volume.agent_id.as_deref()
        && Some(label) != agent_id.map(|id| id.to_string()).as_deref()
    {
        return false;
    }
    true
}

fn project_workspace_should_exist(project: &ProjectSummary) -> bool {
    match (&project.status, &project.clone_status) {
        (ProjectStatus::Ready, ProjectCloneStatus::Ready) => true,
        (ProjectStatus::Creating, ProjectCloneStatus::Pending)
        | (ProjectStatus::Creating, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Creating, ProjectCloneStatus::Ready)
        | (ProjectStatus::Creating, ProjectCloneStatus::Failed)
        | (ProjectStatus::Failed, ProjectCloneStatus::Pending)
        | (ProjectStatus::Failed, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Failed, ProjectCloneStatus::Ready)
        | (ProjectStatus::Failed, ProjectCloneStatus::Failed)
        | (ProjectStatus::Ready, ProjectCloneStatus::Pending)
        | (ProjectStatus::Ready, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Ready, ProjectCloneStatus::Failed)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Pending)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Cloning)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Ready)
        | (ProjectStatus::Deleting, ProjectCloneStatus::Failed) => false,
    }
}

fn agent_workspace_should_exist(agent: &AgentSummary) -> bool {
    agent.resource.state != AgentResourceState::Deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{AgentResourceSnapshot, AgentResourceState, AgentRole, TokenUsage, now};
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    #[test]
    fn reconcile_plan_keeps_live_agent_volumes_and_removes_orphans() {
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let orphan_agent_id = Uuid::new_v4();
        let project = project_summary(project_id);
        let agent = agent_summary(project_id, agent_id);
        let live_volume = ManagedVolume {
            name: project_agent_workspace_volume(&project_id.to_string(), &agent_id.to_string()),
            kind: Some("agent-workspace".to_string()),
            project_id: Some(project_id.to_string()),
            agent_id: Some(agent_id.to_string()),
            role: Some("worker".to_string()),
        };
        let orphan_volume = ManagedVolume {
            name: project_agent_workspace_volume(
                &project_id.to_string(),
                &orphan_agent_id.to_string(),
            ),
            kind: Some("agent-workspace".to_string()),
            project_id: Some(project_id.to_string()),
            agent_id: Some(orphan_agent_id.to_string()),
            role: Some("reviewer".to_string()),
        };
        let cache_volume = ManagedVolume {
            name: project_cache_volume(&project_id.to_string()),
            kind: Some("project-cache".to_string()),
            project_id: Some(project_id.to_string()),
            agent_id: None,
            role: None,
        };

        let plan = plan_project_volume_reconcile(
            &[live_volume, orphan_volume, cache_volume],
            &[project],
            &[agent],
        );

        assert_eq!(
            plan,
            DockerVolumeReconcilePlan {
                orphan_agent_workspace_volumes: vec![project_agent_workspace_volume(
                    &project_id.to_string(),
                    &orphan_agent_id.to_string()
                )],
                orphan_project_cache_volumes: Vec::new(),
                quarantined_volumes: Vec::new(),
                missing_project_cache_volumes: Vec::new(),
                missing_agent_workspace_volumes: Vec::new(),
            }
        );
    }

    #[test]
    fn reconcile_plan_reports_missing_cache_and_agent_volume() {
        let project_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let project = project_summary(project_id);
        let agent = agent_summary(project_id, agent_id);

        let plan = plan_project_volume_reconcile(&[], &[project], &[agent]);

        assert_eq!(
            plan,
            DockerVolumeReconcilePlan {
                orphan_agent_workspace_volumes: Vec::new(),
                orphan_project_cache_volumes: Vec::new(),
                quarantined_volumes: Vec::new(),
                missing_project_cache_volumes: vec![project_id],
                missing_agent_workspace_volumes: vec![agent_id],
            }
        );
    }

    #[test]
    fn reconcile_plan_removes_orphan_project_cache_volume() {
        let project_id = Uuid::new_v4();
        let volume = project_cache_volume(&project_id.to_string());
        let plan = plan_project_volume_reconcile(
            &[ManagedVolume {
                name: volume.clone(),
                kind: Some("project-cache".to_string()),
                project_id: Some(project_id.to_string()),
                agent_id: None,
                role: None,
            }],
            &[],
            &[],
        );

        assert_eq!(
            plan,
            DockerVolumeReconcilePlan {
                orphan_agent_workspace_volumes: Vec::new(),
                orphan_project_cache_volumes: vec![volume],
                quarantined_volumes: Vec::new(),
                missing_project_cache_volumes: Vec::new(),
                missing_agent_workspace_volumes: Vec::new(),
            }
        );
    }

    #[test]
    fn reconcile_plan_handles_unlabelled_historical_names_and_quarantines_invalid_names() {
        let live_project_id = Uuid::new_v4();
        let orphan_project_id = Uuid::new_v4();
        let live_agent_id = Uuid::new_v4();
        let orphan_agent_id = Uuid::new_v4();
        let project = project_summary(live_project_id);
        let agent = agent_summary(live_project_id, live_agent_id);
        let volume = |name: String| ManagedVolume {
            name,
            kind: None,
            project_id: None,
            agent_id: None,
            role: None,
        };

        let plan = plan_project_volume_reconcile(
            &[
                volume(format!("mai-team-workspace-{live_agent_id}")),
                volume(format!("mai-team-workspace-{orphan_agent_id}")),
                volume(format!("mai-team-project-{live_project_id}")),
                volume(format!("mai-team-project-review-{orphan_project_id}")),
                volume("mai-team-workspace-not-a-uuid".to_string()),
            ],
            &[project],
            &[agent],
        );

        assert_eq!(
            vec![format!("mai-team-workspace-{orphan_agent_id}")],
            plan.orphan_agent_workspace_volumes
        );
        assert_eq!(
            vec![format!("mai-team-project-review-{orphan_project_id}")],
            plan.orphan_project_cache_volumes
        );
        assert_eq!(
            vec!["mai-team-workspace-not-a-uuid".to_string()],
            plan.quarantined_volumes
        );
    }

    fn project_summary(project_id: ProjectId) -> ProjectSummary {
        let now = now();
        ProjectSummary {
            id: project_id,
            name: "project".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account".to_string()),
            repository_id: 1,
            installation_id: 1,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "ubuntu:latest".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            last_error: None,
            auto_review_enabled: false,
            reviewer_extra_prompt: None,
            review_status: mai_protocol::ProjectReviewStatus::Disabled,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        }
    }

    fn agent_summary(project_id: ProjectId, agent_id: AgentId) -> AgentSummary {
        let now = now();
        AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            project_id: Some(project_id),
            role: Some(AgentRole::Executor),
            name: "agent".to_string(),
            resource: AgentResourceSnapshot {
                state: AgentResourceState::Ready,
                error: None,
            },
            runtime: None,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "provider".to_string(),
            provider_name: "Provider".to_string(),
            model: "model".to_string(),
            reasoning_effort: None,
            created_at: now,
            updated_at: now,
            token_usage: TokenUsage::default(),
        }
    }
}
