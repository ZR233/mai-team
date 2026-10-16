//! 发布事务已提交而 activation 尚未完成时，新修订不能删掉下一次换代的旧环境。
use super::*;
use crate::{ReferenceCatalog, ReferenceEnvironmentHost};
use chrono::{DateTime, Utc};
use mai_store::{MaiStore, PersistedReferenceBinding, ReferenceApplyScope};
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;

struct Host;
impl ReferenceEnvironmentHost for Host {
    async fn quiesce(&self, _: AgentId, _: ReferenceApplyPolicy) -> Result<()> {
        Ok(())
    }
    async fn rebind(
        &self,
        _: PersistedReferenceBinding,
        _: mai_docker::ContainerHandle,
    ) -> Result<()> {
        Ok(())
    }
    async fn activate(&self, _: PersistedReferenceBinding) -> Result<()> {
        Ok(())
    }
}

enum Publication {
    Current,
    Unpublished,
    Replaced,
}

async fn superseded_trace(publication: Publication) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(MaiStore::open_in_data_dir(directory.path()).await.unwrap());
    let mut agent = test_agent_summary(uuid::Uuid::new_v4(), None, Utc::now());
    let project = test_project_summary(uuid::Uuid::new_v4(), agent.id);
    agent.project_id = Some(project.id);
    agent.container_id = Some("old".into());
    store.insert_project(&project).await.unwrap();
    store.insert_agent(&agent, None).await.unwrap();
    let snapshot = AppliedReferenceSnapshot {
        project_id: project.id,
        configuration_revision: 0,
        fingerprint: "empty".into(),
        references: vec![],
        observed_at: Utc::now(),
    };
    store
        .enqueue_reference_application(
            project.id,
            snapshot.clone(),
            ReferenceApplyPolicy::WhenIdle,
            ReferenceApplyScope::AllPhysicalEnvironments,
        )
        .await
        .unwrap();
    let mut receipt = store.reference_operations().await.unwrap().remove(0);
    receipt.operation.phase = ReferenceApplyPhase::Publishing;
    receipt.operation.old_container_id = Some("old".into());
    receipt.operation.candidate_container_id = Some("candidate".into());
    store
        .save_reference_operation(receipt.clone())
        .await
        .unwrap();
    let current = match publication {
        Publication::Current | Publication::Replaced => "candidate",
        Publication::Unpublished => "old",
    };
    store
        .publish_reference_binding(PersistedReferenceBinding {
            agent_id: agent.id,
            project_id: Some(project.id),
            generation: receipt.operation.id,
            container_id: current.into(),
            effective_image_id: "sha256:fixed".into(),
            applied_revision: 0,
            fingerprint: snapshot.fingerprint.clone(),
            snapshot,
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    let trace = directory.path().join("trace.jsonl");
    let binary = directory.path().join("docker.py");
    let inspection = serde_json::json!([{"Id":"candidate","Name":"/candidate","Config":{"Image":"fixed","Labels":{"mai.team.managed":"true","mai.team.agent":agent.id.to_string()}}}]);
    std::fs::write(&binary, format!("#!/usr/bin/python3\nimport json,sys\nwith open({:?},'a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')\nif sys.argv[1]=='ps': print('old\\ncandidate')\nif sys.argv[1]=='inspect':\n d=json.loads({:?})[0]; print(json.dumps([dict(d,Id=x,Name='/'+x) for x in sys.argv[2:]]))\n", trace.to_str().unwrap(), inspection.to_string())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let docker = mai_docker::DockerClient::new_with_binary("fixed", binary.to_str().unwrap());
    let catalog = ReferenceCatalog::new(Arc::clone(&store), None);
    let applications = ReferenceApplications::new(Arc::clone(&store), docker, catalog, None);
    applications.supersede(&Host, &mut receipt).await.unwrap();
    assert_eq!(receipt.operation.phase, ReferenceApplyPhase::Superseded);
    if matches!(publication, Publication::Replaced) {
        let mut completed = receipt.clone();
        completed.operation.id = uuid::Uuid::new_v4();
        completed.operation.phase = ReferenceApplyPhase::Succeeded;
        completed.operation.created_at = Utc::now();
        let mut binding = store.reference_binding(agent.id).await.unwrap().unwrap();
        binding.container_id = "replacement".into();
        binding.generation = completed.operation.id;
        store.publish_reference_binding(binding).await.unwrap();
        applications
            .retire_previous_operations(&completed, "replacement")
            .await
            .unwrap();
    }

    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn supersession_retains_the_published_candidate_for_the_next_generation() {
    let trace = superseded_trace(Publication::Current).await;
    assert_eq!(
        trace
            .iter()
            .filter(|args| args.first().is_some_and(|arg| arg == "rm"))
            .collect::<Vec<_>>(),
        Vec::<&Vec<String>>::new()
    );
}

#[tokio::test]
async fn replacement_retires_resources_of_published_supersession() {
    let trace = superseded_trace(Publication::Replaced).await;
    let removed = trace
        .iter()
        .filter(|args| args.first().is_some_and(|arg| arg == "rm"))
        .flatten()
        .filter(|arg| matches!(arg.as_str(), "old" | "candidate" | "replacement"))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        removed,
        std::collections::BTreeSet::from(["old".to_string(), "candidate".to_string()])
    );
}

#[tokio::test]
async fn supersession_removes_an_unpublished_candidate() {
    let trace = superseded_trace(Publication::Unpublished).await;
    assert_eq!(
        trace
            .iter()
            .filter(|args| args.first().is_some_and(|arg| arg == "rm"))
            .count(),
        1
    );
}

fn test_project_summary(project_id: ProjectId, maintainer_agent_id: AgentId) -> ProjectSummary {
    let timestamp = Utc::now();
    ProjectSummary {
        id: project_id,
        name: "owner/repo".to_string(),
        status: ProjectStatus::Ready,
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        repository_full_name: "owner/repo".to_string(),
        git_account_id: Some("account-1".to_string()),
        repository_id: 42,
        installation_id: 0,
        installation_account: "owner".to_string(),
        branch: "main".to_string(),
        docker_image: "ubuntu:latest".to_string(),
        clone_status: ProjectCloneStatus::Ready,
        maintainer_agent_id,
        created_at: timestamp,
        updated_at: timestamp,
        last_error: None,
        auto_review_enabled: true,
        reviewer_extra_prompt: None,
        review_status: ProjectReviewStatus::Waiting,
        current_reviewer_agent_id: None,
        last_review_started_at: None,
        last_review_finished_at: None,
        next_review_at: None,
        last_review_outcome: None,
        review_last_error: None,
    }
}

fn test_agent_summary(
    id: AgentId,
    review_run_id: Option<Uuid>,
    timestamp: DateTime<Utc>,
) -> AgentSummary {
    AgentSummary {
        id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: review_run_id.map(|_| AgentRole::Reviewer),
        profile_id: None,
        workspace: None,
        review_run_id,
        name: format!("agent-{id}"),
        resource: AgentResourceSnapshot {
            state: AgentResourceState::Ready,
            error: None,
        },
        runtime: None,
        last_turn: None,
        container_id: None,
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "provider".to_string(),
        provider_name: "Provider".to_string(),
        model: "model".to_string(),
        reasoning_effort: None,
        created_at: timestamp,
        updated_at: timestamp,
        usage: RuntimeUsageSnapshot::default(),
    }
}
