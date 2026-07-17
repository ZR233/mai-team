use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentModelPreference, AgentRole, AgentSummary, CreateAgentRequest, ProjectId,
    ProjectSummary, TurnId,
};

use crate::agents::ContainerSource;
use crate::projects::review::context::ProjectReviewContext;
use crate::projects::review::target::{
    ProjectReviewRequest, ProjectReviewTargetOps, ResolvedProjectReviewTarget,
    resolve_project_review_target,
};
use crate::projects::workspace::{ProjectRepositoryReviewTarget, ProjectRepositoryRevision};
use crate::{Result, RuntimeError};
use uuid::Uuid;

const REVIEWER_SKILL_MENTION: &str = "reviewer-agent-review-pr";

/// Provides the agent and project operations needed by the project review loop
/// to create and run a short-lived reviewer agent.
pub(crate) trait ProjectReviewerAgentOps: ProjectReviewTargetOps + Send + Sync {
    fn agent_summary(&self, agent_id: AgentId)
    -> impl Future<Output = Result<AgentSummary>> + Send;

    fn reviewer_model(&self) -> impl Future<Output = Result<AgentModelPreference>> + Send;

    fn project_reviewer_agents(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Vec<AgentSummary>> + Send;

    fn sync_project_repository_for_review(
        &self,
        project_id: ProjectId,
        target: ResolvedProjectReviewTarget,
    ) -> impl Future<Output = Result<ProjectRepositoryRevision>> + Send;

    fn create_project_review_context(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        target: ResolvedProjectReviewTarget,
        project_revision: ProjectRepositoryRevision,
    ) -> impl Future<Output = Result<Arc<ProjectReviewContext>>> + Send;

    fn create_agent_with_container_source(
        &self,
        request: CreateAgentRequest,
        source: ContainerSource,
        task_id: Option<mai_protocol::TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn attach_project_review_context(
        &self,
        agent_id: AgentId,
        context: Arc<ProjectReviewContext>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;

    fn start_agent_turn(
        &self,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl Future<Output = Result<TurnId>> + Send;

    fn last_turn_response(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<String>>> + Send;
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedProjectReviewer {
    pub(crate) agent: AgentSummary,
    pub(crate) target: ResolvedProjectReviewTarget,
    pub(crate) project_revision: ProjectRepositoryRevision,
}

pub(crate) async fn prepare_project_reviewer(
    ops: &impl ProjectReviewerAgentOps,
    project_id: ProjectId,
    run_id: Uuid,
    request: ProjectReviewRequest,
) -> Result<PreparedProjectReviewer> {
    let existing_reviewer_id = ops
        .project_reviewer_agents(project_id)
        .await
        .first()
        .map(|reviewer| reviewer.id);
    ensure_project_reviewer_slot_available(existing_reviewer_id)?;
    let target = resolve_project_review_target(ops, project_id, request).await?;
    let project_revision = ops
        .sync_project_repository_for_review(project_id, target.clone())
        .await?;
    let context = ops
        .create_project_review_context(project_id, run_id, target.clone(), project_revision.clone())
        .await?;
    let project_summary = ops.project_summary(project_id).await?;
    let maintainer_summary = ops
        .agent_summary(project_summary.maintainer_agent_id)
        .await?;
    let model = ops.reviewer_model().await?;
    let agent = ops
        .create_agent_with_container_source(
            CreateAgentRequest {
                name: Some(format!("{} Auto Reviewer", project_summary.name)),
                provider_id: Some(model.provider_id),
                model: Some(model.model),
                reasoning_effort: model.reasoning_effort,
                docker_image: Some(maintainer_summary.docker_image.clone()),
                parent_id: Some(project_summary.maintainer_agent_id),
                system_prompt: Some(super::project_reviewer_system_prompt().to_string()),
            },
            ContainerSource::ProjectReviewWorkspace {
                target: ProjectRepositoryReviewTarget {
                    pr: target.pr,
                    head_sha: target.head_sha.clone(),
                },
                revision: project_revision.clone(),
            },
            maintainer_summary.task_id,
            Some(project_id),
            Some(AgentRole::Reviewer),
        )
        .await?;
    if let Err(error) = ops
        .attach_project_review_context(agent.id, Arc::clone(&context))
        .await
    {
        let _ = ops.delete_agent(agent.id).await;
        return Err(error);
    }
    Ok(PreparedProjectReviewer {
        agent,
        target,
        project_revision,
    })
}

fn ensure_project_reviewer_slot_available(existing_reviewer_id: Option<AgentId>) -> Result<()> {
    if let Some(existing_reviewer_id) = existing_reviewer_id {
        return Err(RuntimeError::InvalidInput(format!(
            "project already owns reviewer agent `{existing_reviewer_id}`"
        )));
    }
    Ok(())
}

pub(crate) async fn project_reviewer_initial_message(
    ops: &impl ProjectReviewerAgentOps,
    project_id: ProjectId,
    reviewer_id: AgentId,
    target: ResolvedProjectReviewTarget,
    project_revision: ProjectRepositoryRevision,
) -> Result<String> {
    let summary = ops.project_summary(project_id).await?;
    Ok(project_reviewer_initial_message_from_summary(
        &summary,
        reviewer_id,
        &target,
        &project_revision,
    ))
}

pub(crate) async fn start_reviewer_turn(
    ops: &impl ProjectReviewerAgentOps,
    reviewer_id: AgentId,
    message: String,
) -> Result<TurnId> {
    ops.start_agent_turn(
        reviewer_id,
        message,
        vec![REVIEWER_SKILL_MENTION.to_string()],
    )
    .await
}

pub(crate) async fn last_turn_response(
    ops: &impl ProjectReviewerAgentOps,
    reviewer_id: AgentId,
) -> Result<String> {
    ops.last_turn_response(reviewer_id).await?.ok_or_else(|| {
        RuntimeError::InvalidInput("reviewer did not return a final response".to_string())
    })
}

fn project_reviewer_initial_message_from_summary(
    summary: &ProjectSummary,
    reviewer_id: AgentId,
    target: &ResolvedProjectReviewTarget,
    project_revision: &ProjectRepositoryRevision,
) -> String {
    let extra = summary
        .reviewer_extra_prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("None");
    format!(
        "Run one automatic pull request review for project `{}`.\n\nRepository: {}/{}\nDefault branch: {}\nBase SHA: {}\nTarget pull request: review PR #{} only\nPR head SHA: {}\nReviewer working tree: /workspace/repo\nReviewer agent: {}\n\nThe service already fetched both revisions and verified that `/workspace/repo` is clean and checked out at the exact PR head. Do not select another pull request and do not checkout another revision. Compare against `origin/{}`.\n\nExtra reviewer instructions:\n{}\n\nUse the $reviewer-agent-review-pr skill. At the end of the turn, return only one JSON object matching this schema exactly:\n{{\"outcome\":\"review_submitted|failed\",\"review_event\":\"approve|request_changes|comment\"|null,\"pr\":123|null,\"summary\":\"short result\",\"error\":null|\"failure reason\"}}",
        summary.name,
        summary.owner,
        summary.repo,
        project_revision.branch,
        project_revision.base_sha,
        target.pr,
        target.head_sha,
        reviewer_id,
        project_revision.branch,
        extra
    )
}

#[cfg(test)]
mod tests {
    use mai_protocol::{
        ProjectCloneStatus, ProjectReviewOutcome, ProjectReviewStatus, ProjectStatus,
        ProjectSummary, now,
    };
    use uuid::Uuid;

    use super::{
        ProjectRepositoryRevision, ResolvedProjectReviewTarget,
        ensure_project_reviewer_slot_available, project_reviewer_initial_message_from_summary,
    };

    #[test]
    fn existing_reviewer_blocks_second_project_reviewer() {
        let reviewer_id = Uuid::new_v4();

        let error = ensure_project_reviewer_slot_available(Some(reviewer_id))
            .expect_err("duplicate reviewer must be rejected");

        assert_eq!(
            error.to_string(),
            format!("invalid input: project already owns reviewer agent `{reviewer_id}`")
        );
    }

    #[test]
    fn target_pr_message_delegates_selection_to_system_selector() {
        let reviewer_id = Uuid::new_v4();
        let message = project_reviewer_initial_message_from_summary(
            &test_project_summary(),
            reviewer_id,
            &ResolvedProjectReviewTarget {
                pr: 24,
                head_sha: "head-sha".to_string(),
            },
            &ProjectRepositoryRevision {
                branch: "main".to_string(),
                base_sha: "base-sha".to_string(),
            },
        );

        assert!(message.contains("review PR #24 only"));
        assert!(message.contains("Base SHA: base-sha"));
        assert!(message.contains("PR head SHA: head-sha"));
        assert!(message.contains("checked out at the exact PR head"));
        assert!(!message.contains("select-pr"));
        assert!(!message.contains("eligibility"));
        assert!(!message.contains("filtering"));
    }

    fn test_project_summary() -> ProjectSummary {
        ProjectSummary {
            id: Uuid::new_v4(),
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
            docker_image: "mai-sidecar:local".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: Uuid::new_v4(),
            created_at: now(),
            updated_at: now(),
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Idle,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: Some(ProjectReviewOutcome::NoEligiblePr),
            review_last_error: None,
        }
    }
}
