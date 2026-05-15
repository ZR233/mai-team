use std::future::Future;

use mai_protocol::{
    AgentId, AgentModelPreference, AgentRole, AgentStatus, AgentSummary, CreateAgentRequest,
    ProjectId, ProjectSummary, TurnId,
};
use tokio_util::sync::CancellationToken;

use crate::agents::ContainerSource;
use crate::{Result, RuntimeError};

const SELECTOR_SKILL_MENTION: &str = "reviewer-agent-select-prs";

/// Provides agent lifecycle operations for the short-lived project PR selector.
pub(crate) trait ProjectReviewSelectorOps: Send + Sync {
    fn project_summary(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<ProjectSummary>> + Send;

    fn agent_summary(&self, agent_id: AgentId)
    -> impl Future<Output = Result<AgentSummary>> + Send;

    fn selector_model(&self) -> impl Future<Output = Result<AgentModelPreference>> + Send;

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

    fn wait_agent_until_complete_with_cancel(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn last_turn_response(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<String>>> + Send;

    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) async fn run_project_review_selector(
    ops: &impl ProjectReviewSelectorOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let selector = spawn_project_review_selector_agent(ops, project_id).await?;
    let selector_id = selector.id;
    let result = async {
        let message = project_review_selector_initial_message(ops, project_id, selector_id).await?;
        ops.start_agent_turn(
            selector_id,
            message,
            vec![SELECTOR_SKILL_MENTION.to_string()],
        )
        .await?;
        let summary = ops
            .wait_agent_until_complete_with_cancel(selector_id, &cancellation_token)
            .await?;
        if summary.status == AgentStatus::Cancelled && cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        if summary.status != AgentStatus::Completed {
            return Err(RuntimeError::InvalidInput(format!(
                "selector ended with status {:?}",
                summary.status
            )));
        }
        let response = ops.last_turn_response(selector_id).await?.ok_or_else(|| {
            RuntimeError::InvalidInput("selector did not return a final response".to_string())
        })?;
        parse_selector_final_response(&response)?;
        Ok(())
    }
    .await;
    let _ = ops.delete_agent(selector_id).await;
    result
}

async fn spawn_project_review_selector_agent(
    ops: &impl ProjectReviewSelectorOps,
    project_id: ProjectId,
) -> Result<AgentSummary> {
    let project_summary = ops.project_summary(project_id).await?;
    let maintainer_summary = ops
        .agent_summary(project_summary.maintainer_agent_id)
        .await?;
    let model = ops.selector_model().await?;
    ops.create_agent_with_container_source(
        CreateAgentRequest {
            name: Some(format!("{} PR Selector", project_summary.name)),
            provider_id: Some(model.provider_id),
            model: Some(model.model),
            reasoning_effort: model.reasoning_effort,
            docker_image: Some(maintainer_summary.docker_image.clone()),
            parent_id: Some(project_summary.maintainer_agent_id),
            system_prompt: Some(super::project_review_selector_system_prompt().to_string()),
        },
        ContainerSource::FreshImage,
        maintainer_summary.task_id,
        Some(project_id),
        Some(AgentRole::Explorer),
    )
    .await
}

async fn project_review_selector_initial_message(
    ops: &impl ProjectReviewSelectorOps,
    project_id: ProjectId,
    selector_id: AgentId,
) -> Result<String> {
    let summary = ops.project_summary(project_id).await?;
    Ok(format!(
        "Select eligible pull requests for project `{}`.\n\nRepository: {}/{}\nDefault branch: {}\nSelector agent: {}\nReviewer clone: /workspace/repo\n\nUse the $reviewer-agent-select-prs skill. Queue every eligible PR with `queue_project_review_prs`. Do not submit reviews, do not modify the repository, and do not start reviewer agents.\n\nAt the end of the turn, return only one JSON object matching this schema exactly:\n{{\"outcome\":\"queued|no_eligible_pr|failed\",\"prs\":[123],\"summary\":\"short result\",\"error\":null|\"failure reason\"}}",
        summary.name, summary.owner, summary.repo, summary.branch, selector_id
    ))
}

fn parse_selector_final_response(text: &str) -> Result<()> {
    let value = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => value,
        Err(_) => {
            let json = extract_json_object(text).ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "invalid selector final JSON: missing json object".to_string(),
                )
            })?;
            serde_json::from_str::<serde_json::Value>(json).map_err(|err| {
                RuntimeError::InvalidInput(format!("invalid selector final JSON: {err}"))
            })?
        }
    };
    let outcome = value.get("outcome").and_then(serde_json::Value::as_str);
    match outcome {
        Some("queued" | "no_eligible_pr") => Ok(()),
        Some("failed") => Err(RuntimeError::InvalidInput(
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("selector reported failure")
                .to_string(),
        )),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "invalid selector outcome `{other}`"
        ))),
        None => Err(RuntimeError::InvalidInput(
            "selector final JSON missing `outcome`".to_string(),
        )),
    }
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}
