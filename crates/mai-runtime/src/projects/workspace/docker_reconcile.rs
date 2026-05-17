use std::collections::{HashMap, HashSet};

use mai_docker::{
    DockerClient, ManagedVolume, project_agent_workspace_volume, project_cache_volume,
};
use mai_protocol::{
    AgentId, AgentStatus, AgentSummary, ProjectCloneStatus, ProjectId, ProjectStatus,
    ProjectSummary,
};

use crate::Result;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DockerVolumeReconcileReport {
    pub(crate) orphan_agent_workspace_volumes_removed: Vec<String>,
    pub(crate) orphan_agent_workspace_volume_removal_failed: Vec<String>,
    pub(crate) orphan_project_cache_volumes_removed: Vec<String>,
    pub(crate) orphan_project_cache_volume_removal_failed: Vec<String>,
    pub(crate) missing_project_cache_volumes: Vec<ProjectId>,
    pub(crate) missing_agent_workspace_volumes: Vec<AgentId>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DockerVolumeReconcilePlan {
    orphan_agent_workspace_volumes: Vec<String>,
    orphan_project_cache_volumes: Vec<String>,
    missing_project_cache_volumes: Vec<ProjectId>,
    missing_agent_workspace_volumes: Vec<AgentId>,
}

pub(crate) async fn reconcile_project_volumes(
    docker: &DockerClient,
    live_projects: &[ProjectSummary],
    live_agents: &[AgentSummary],
) -> Result<DockerVolumeReconcileReport> {
    let volumes = docker.list_managed_volumes().await?;
    let plan = plan_project_volume_reconcile(&volumes, live_projects, live_agents);
    let mut report = DockerVolumeReconcileReport {
        missing_project_cache_volumes: plan.missing_project_cache_volumes,
        missing_agent_workspace_volumes: plan.missing_agent_workspace_volumes,
        ..DockerVolumeReconcileReport::default()
    };

    for volume in plan.orphan_agent_workspace_volumes {
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
    report.missing_project_cache_volumes.sort();
    report.missing_agent_workspace_volumes.sort();
    Ok(report)
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
    let mut expected_project_cache_volumes = HashMap::new();
    let mut expected_agent_workspace_volumes = HashMap::new();

    for project in live_projects {
        expected_project_cache_volumes
            .insert(project_cache_volume(&project.id.to_string()), project.id);
    }
    for agent in live_agents {
        let Some(project_id) = agent.project_id else {
            continue;
        };
        if !agent_workspace_should_exist(agent) || !live_projects_by_id.contains_key(&project_id) {
            continue;
        };
        expected_agent_workspace_volumes.insert(
            project_agent_workspace_volume(&project_id.to_string(), &agent.id.to_string()),
            agent.id,
        );
    }

    let mut present_project_cache_volumes = HashSet::new();
    let mut present_agent_workspace_volumes = HashSet::new();
    let mut orphan_project_cache_volumes = Vec::new();
    let mut orphan_agent_workspace_volumes = Vec::new();

    for volume in volumes {
        match volume.kind.as_deref() {
            Some("project-cache") => {
                let Some(project_id) = parse_project_id(volume.project_id.as_deref()) else {
                    orphan_project_cache_volumes.push(volume.name.clone());
                    continue;
                };
                let expected_name = project_cache_volume(&project_id.to_string());
                if live_project_ids.contains(&project_id)
                    && expected_project_cache_volumes.contains_key(&volume.name)
                    && volume.name == expected_name
                {
                    present_project_cache_volumes.insert(volume.name.clone());
                } else {
                    orphan_project_cache_volumes.push(volume.name.clone());
                }
            }
            Some("agent-workspace") => {
                let Some(project_id) = parse_project_id(volume.project_id.as_deref()) else {
                    orphan_agent_workspace_volumes.push(volume.name.clone());
                    continue;
                };
                let Some(agent_id) = parse_agent_id(volume.agent_id.as_deref()) else {
                    orphan_agent_workspace_volumes.push(volume.name.clone());
                    continue;
                };
                let expected_name =
                    project_agent_workspace_volume(&project_id.to_string(), &agent_id.to_string());
                if live_agent_projects.get(&agent_id) == Some(&project_id)
                    && expected_agent_workspace_volumes.contains_key(&volume.name)
                    && volume.name == expected_name
                {
                    present_agent_workspace_volumes.insert(volume.name.clone());
                } else {
                    orphan_agent_workspace_volumes.push(volume.name.clone());
                }
            }
            Some(_) | None => {}
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
    missing_project_cache_volumes.sort();
    missing_agent_workspace_volumes.sort();

    DockerVolumeReconcilePlan {
        orphan_agent_workspace_volumes,
        orphan_project_cache_volumes,
        missing_project_cache_volumes,
        missing_agent_workspace_volumes,
    }
}

fn parse_project_id(value: Option<&str>) -> Option<ProjectId> {
    value.and_then(|value| ProjectId::parse_str(value).ok())
}

fn parse_agent_id(value: Option<&str>) -> Option<AgentId> {
    value.and_then(|value| AgentId::parse_str(value).ok())
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
    match agent.status {
        AgentStatus::Created
        | AgentStatus::StartingContainer
        | AgentStatus::Idle
        | AgentStatus::RunningTurn
        | AgentStatus::WaitingTool
        | AgentStatus::Completed
        | AgentStatus::Failed
        | AgentStatus::Cancelled
        | AgentStatus::DeletingContainer => true,
        AgentStatus::Deleted => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{AgentRole, TokenUsage, now};
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
                missing_project_cache_volumes: Vec::new(),
                missing_agent_workspace_volumes: Vec::new(),
            }
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
            status: AgentStatus::Idle,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "provider".to_string(),
            provider_name: "Provider".to_string(),
            model: "model".to_string(),
            reasoning_effort: None,
            created_at: now,
            updated_at: now,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        }
    }
}
