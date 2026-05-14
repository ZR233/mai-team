use std::future::Future;

use mai_docker::project_review_workspace_volume;
use mai_protocol::{
    AgentId, AgentModelPreference, AgentRole, AgentSummary, CreateAgentRequest, ProjectId,
    ProjectSummary, TurnId,
};

use crate::agents::ContainerSource;
use crate::{Result, RuntimeError};

const REVIEWER_SKILL_MENTION: &str = "reviewer-agent-review-pr";

/// Provides the agent and project operations needed by the project review loop
/// to create and run a short-lived reviewer agent.
pub(crate) trait ProjectReviewerAgentOps: Send + Sync {
    fn project_summary(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<ProjectSummary>> + Send;

    fn agent_summary(&self, agent_id: AgentId)
    -> impl Future<Output = Result<AgentSummary>> + Send;

    fn reviewer_model(&self) -> impl Future<Output = Result<AgentModelPreference>> + Send;

    fn create_agent_with_container_source(
        &self,
        request: CreateAgentRequest,
        source: ContainerSource,
        task_id: Option<mai_protocol::TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

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

pub(crate) async fn spawn_project_reviewer_agent(
    ops: &impl ProjectReviewerAgentOps,
    project_id: ProjectId,
) -> Result<AgentSummary> {
    let project_summary = ops.project_summary(project_id).await?;
    let maintainer_summary = ops
        .agent_summary(project_summary.maintainer_agent_id)
        .await?;
    let model = ops.reviewer_model().await?;
    let workspace_volume = project_review_workspace_volume(&project_id.to_string());
    ops.create_agent_with_container_source(
        CreateAgentRequest {
            name: Some(format!("{} Auto Reviewer", project_summary.name)),
            provider_id: Some(model.provider_id),
            model: Some(model.model),
            reasoning_effort: model.reasoning_effort,
            docker_image: Some(maintainer_summary.docker_image.clone()),
            parent_id: Some(project_summary.maintainer_agent_id),
            system_prompt: Some(super::project_reviewer_system_prompt().to_string()),
        },
        ContainerSource::ImageWithWorkspace { workspace_volume },
        maintainer_summary.task_id,
        Some(project_id),
        Some(AgentRole::Reviewer),
    )
    .await
}

pub(crate) async fn project_reviewer_initial_message(
    ops: &impl ProjectReviewerAgentOps,
    project_id: ProjectId,
    reviewer_id: AgentId,
    target_pr: Option<u64>,
) -> Result<String> {
    let summary = ops.project_summary(project_id).await?;
    Ok(project_reviewer_initial_message_from_summary(
        &summary,
        reviewer_id,
        target_pr,
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
    target_pr: Option<u64>,
) -> String {
    let extra = summary
        .reviewer_extra_prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("None");
    let target = target_pr
        .map(|pr| format!("Target pull request: review PR #{pr} only. Do not select another pull request. Use `select-pr --target-pr {pr}` when invoking the helper."))
        .unwrap_or_else(|| {
            "Target pull request: none. Select exactly one eligible pull request using the helper."
                .to_string()
        });
    format!(
        "Run one automatic pull request review for project `{}`.\n\nRepository: {}/{}\nDefault branch: {}\nWorkspace repo: /workspace/repo\nReview worktree root: /workspace/reviews/{}\n{}\n\nExtra reviewer instructions:\n{}\n\nUse the $reviewer-agent-review-pr skill. At the end of the turn, return only one JSON object matching this schema exactly:\n{{\"outcome\":\"review_submitted|no_eligible_pr|failed\",\"pr\":123|null,\"summary\":\"short result\",\"error\":null|\"failure reason\"}}",
        summary.name, summary.owner, summary.repo, summary.branch, reviewer_id, target, extra
    )
}
