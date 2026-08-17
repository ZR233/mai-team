use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole};
use pl_core::{
    FunctionToolDefinition, RegisteredTool, ToolVisibilitySet,
    tool::cache::ToolCachePolicy,
};
use serde_json::{Value, json};

use crate::state::AgentRecord;
use crate::turn::product_tool_schemas::definitions::{
    GITHUB_API_REQUEST_DESCRIPTION, QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
    READ_TOOL_ARTIFACT_DESCRIPTION, SAVE_ARTIFACT_DESCRIPTION, SAVE_TASK_PLAN_DESCRIPTION,
    SUBMIT_REVIEW_RESULT_DESCRIPTION, QueueProjectReviewPrsInput, ReadToolArtifactInput,
    SaveArtifactInput, SaveTaskPlanInput, SubmitReviewResultInput,
};
use crate::turn::product_tool_schemas::{
    TOOL_GITHUB_API_REQUEST, TOOL_QUEUE_PROJECT_REVIEW_PRS, TOOL_READ_TOOL_ARTIFACT,
    TOOL_SAVE_ARTIFACT, TOOL_SAVE_TASK_PLAN, TOOL_SUBMIT_REVIEW_RESULT,
};
use crate::turn::tool_output::ToolExecution;
use crate::{AgentRuntime, ProjectReviewQueueRequest, RuntimeError};

pub(crate) use crate::turn::product_tool_schemas::definitions::GithubApiRequest;

/// 将 mai-team 产品工具挂入 pl-core agent kernel 的动态工具注册表。
///
/// 该注册器只承载 GitHub、review queue、artifact 和 task plan 等产品语义；
/// 工具生命周期、trace、tool result history 和模型回合调度仍由 pl-core 统一处理。
#[derive(Clone)]
pub(crate) struct MaiProductToolRegistry {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
    visible: ToolVisibilitySet,
}

impl MaiProductToolRegistry {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
        visible: ToolVisibilitySet,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
            visible,
        }
    }

    pub(crate) fn registered_tools(&self) -> crate::Result<Vec<RegisteredTool>> {
        [
            TOOL_SAVE_TASK_PLAN,
            TOOL_SUBMIT_REVIEW_RESULT,
            TOOL_SAVE_ARTIFACT,
            TOOL_READ_TOOL_ARTIFACT,
            TOOL_GITHUB_API_REQUEST,
            TOOL_QUEUE_PROJECT_REVIEW_PRS,
        ]
        .into_iter()
        .filter(|name| self.visible.contains(name))
        .map(|name| self.registered_tool(name))
        .collect()
    }

    fn registered_tool(&self, name: &str) -> crate::Result<RegisteredTool> {
        match name {
            TOOL_SAVE_TASK_PLAN => {
                let executor = self.clone();
                Ok(FunctionToolDefinition::<SaveTaskPlanInput>::new(
                    TOOL_SAVE_TASK_PLAN,
                    SAVE_TASK_PLAN_DESCRIPTION,
                )
                .registered(move |input, _context| {
                    let executor = executor.clone();
                    async move { executor.save_task_plan(input).await }
                }))
            }
            TOOL_SUBMIT_REVIEW_RESULT => {
                let executor = self.clone();
                Ok(FunctionToolDefinition::<SubmitReviewResultInput>::new(
                    TOOL_SUBMIT_REVIEW_RESULT,
                    SUBMIT_REVIEW_RESULT_DESCRIPTION,
                )
                .registered(move |input, _context| {
                    let executor = executor.clone();
                    async move { executor.submit_review_result(input).await }
                }))
            }
            TOOL_SAVE_ARTIFACT => {
                let executor = self.clone();
                Ok(FunctionToolDefinition::<SaveArtifactInput>::new(
                    TOOL_SAVE_ARTIFACT,
                    SAVE_ARTIFACT_DESCRIPTION,
                )
                .registered(move |input, _context| {
                    let executor = executor.clone();
                    async move { executor.save_artifact(input).await }
                }))
            }
            TOOL_READ_TOOL_ARTIFACT => {
                let executor = self.clone();
                Ok(
                    FunctionToolDefinition::<ReadToolArtifactInput>::new(
                        TOOL_READ_TOOL_ARTIFACT,
                        READ_TOOL_ARTIFACT_DESCRIPTION,
                    )
                    .registered(move |input, _context| {
                        let executor = executor.clone();
                        async move { executor.read_tool_artifact(input).await }
                    })
                    .with_cache_policy(ToolCachePolicy::WithinTurn),
                )
            }
            TOOL_GITHUB_API_REQUEST => {
                let executor = self.clone();
                Ok(
                    FunctionToolDefinition::<GithubApiRequest>::new(
                        TOOL_GITHUB_API_REQUEST,
                        GITHUB_API_REQUEST_DESCRIPTION,
                    )
                    .registered(move |input, context| {
                        let executor = executor.clone();
                        async move { executor.github_api_request(input, context).await }
                    })
                    .with_cache_policy_resolver(github_api_cache_policy)
                    .with_cache_invalidation_resolver(github_api_invalidates_cache),
                )
            }
            TOOL_QUEUE_PROJECT_REVIEW_PRS => {
                let executor = self.clone();
                Ok(FunctionToolDefinition::<QueueProjectReviewPrsInput>::new(
                    TOOL_QUEUE_PROJECT_REVIEW_PRS,
                    QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
                )
                .registered(move |input, _context| {
                    let executor = executor.clone();
                    async move { executor.queue_project_review_prs(input).await }
                }))
            }
            unknown => Err(RuntimeError::InvalidInput(format!(
                "tool `{unknown}` is not a mai-team product tool"
            ))),
        }
    }

    async fn save_task_plan(&self, input: SaveTaskPlanInput) -> crate::Result<ToolExecution> {
        let task = self
            .runtime
            .save_task_plan(self.agent_id, input.title, input.markdown)
            .await?;
        Ok(ToolExecution::json(&task)?)
    }

    async fn submit_review_result(
        &self,
        input: SubmitReviewResultInput,
    ) -> crate::Result<ToolExecution> {
        let review = self
            .runtime
            .submit_review_result(self.agent_id, input.passed, input.findings, input.summary)
            .await?;
        Ok(ToolExecution::json(&review)?)
    }

    async fn save_artifact(&self, input: SaveArtifactInput) -> crate::Result<ToolExecution> {
        let artifact = self
            .runtime
            .save_artifact(self.agent_id, input.path, input.name)
            .await?;
        Ok(ToolExecution::json(&artifact)?)
    }

    async fn read_tool_artifact(
        &self,
        input: ReadToolArtifactInput,
    ) -> crate::Result<ToolExecution> {
        let output = super::tool_artifact::read(&self.runtime, self.agent_id, input).await?;
        Ok(ToolExecution::json(output)?)
    }

    async fn github_api_request(
        &self,
        request: GithubApiRequest,
        context: pl_core::ToolContext,
    ) -> crate::Result<ToolExecution> {
        let mut execution = self
            .runtime
            .execute_project_github_api_request(&self.agent, &request)
            .await?;
        if execution.output.len() > execution.model_output.len() {
            let call_id = context
                .provider_call_id
                .unwrap_or_else(|| format!("github-{}", uuid::Uuid::new_v4()));
            let artifact_id = uuid::Uuid::new_v4().to_string();
            let name = "github-api-response.json";
            let path = self.runtime.tool_output_artifact_file_path(
                self.agent_id,
                &call_id,
                &artifact_id,
                name,
            );
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, execution.output.as_bytes()).await?;
            execution
                .output_artifacts
                .push(mai_protocol::ToolOutputArtifactInfo {
                    id: artifact_id,
                    call_id,
                    agent_id: self.agent_id,
                    name: name.to_string(),
                    stream: "response".to_string(),
                    size_bytes: execution.output.len() as u64,
                    created_at: mai_protocol::now(),
                });
        }
        Ok(execution)
    }

    async fn queue_project_review_prs(
        &self,
        input: QueueProjectReviewPrsInput,
    ) -> crate::Result<ToolExecution> {
        let agent_summary = self.agent.summary.read().await.clone();
        let project_id = agent_summary.project_id.ok_or_else(|| {
            RuntimeError::InvalidInput(
                "queue_project_review_prs is only available to project agents".to_string(),
            )
        })?;
        if !matches!(
            agent_summary.role,
            Some(AgentRole::Explorer | AgentRole::Reviewer)
        ) {
            return Err(RuntimeError::InvalidInput(
                "queue_project_review_prs is only available to project selector and reviewer agents"
                    .to_string(),
            ));
        }

        let mut queued = Vec::new();
        let mut deduped = Vec::new();
        let mut ignored = Vec::new();
        for pr in input.prs {
            if pr.number == 0 {
                return Err(RuntimeError::InvalidInput(
                    "each `prs` item must include positive integer field `number`".to_string(),
                ));
            }
            let reason = pr
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("selector")
                .to_string();
            let summary = self
                .runtime
                .enqueue_project_review(ProjectReviewQueueRequest {
                    project_id,
                    pr: pr.number,
                    head_sha: pr.head_sha,
                    delivery_id: None,
                    reason,
                })
                .await?;
            queued.extend(summary.queued);
            deduped.extend(summary.deduped);
            ignored.extend(summary.ignored);
        }
        Ok(ToolExecution::json(json!({
            "queued": queued,
            "deduped": deduped,
            "ignored": ignored,
        }))?)
    }
}

fn github_api_cache_policy(arguments: &Value) -> ToolCachePolicy {
    match arguments
        .get("method")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("GET") => ToolCachePolicy::WithinTurn,
        Some(_) | None => ToolCachePolicy::Never,
    }
}

fn github_api_invalidates_cache(arguments: &Value) -> bool {
    arguments
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| !method.trim().eq_ignore_ascii_case("GET"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn github_cache_is_read_only_and_writes_invalidate_reads() {
        assert_eq!(
            github_api_cache_policy(&json!({"method": "get"})),
            ToolCachePolicy::WithinTurn
        );
        assert_eq!(
            github_api_cache_policy(&json!({"method": "POST"})),
            ToolCachePolicy::Never
        );
        assert!(!github_api_invalidates_cache(&json!({"method": "GET"})));
        assert!(github_api_invalidates_cache(&json!({"method": "PATCH"})));
    }

    #[test]
    fn github_api_request_input_rejects_json_string_body() {
        let err = serde_json::from_value::<GithubApiRequest>(json!({
            "method": "POST",
            "path": "/repos/owner/repo/pulls/42/reviews",
            "body": r#"{"event":"COMMENT","body":"Looks good."}"#
        }))
        .expect_err("JSON string body should be rejected");

        assert!(
            err.to_string()
                .contains("field `body` must be a JSON object or null")
    );
    }

    #[test]
    fn github_api_request_input_rejects_non_object_body() {
        let err = serde_json::from_value::<GithubApiRequest>(json!({
            "method": "POST",
            "path": "/repos/owner/repo/issues/42/comments",
            "body": "[\"not\", \"an\", \"object\"]"
        }))
        .expect_err("body array should be rejected");

        assert!(
            err.to_string()
                .contains("field `body` must be a JSON object or null")
        );
    }

    #[test]
    fn queue_project_review_prs_uses_camel_case_head_sha() {
        let input = serde_json::from_value::<QueueProjectReviewPrsInput>(json!({
            "prs": [
                { "number": 42, "headSha": "abc123", "reason": "ready" }
            ]
        }))
        .expect("queue input");

        assert_eq!(
            input.prs,
            vec![crate::turn::product_tool_schemas::definitions::QueueProjectReviewPr {
                number: 42,
                head_sha: Some("abc123".to_string()),
                reason: Some("ready".to_string()),
            }]
        );
    }

    #[test]
    fn queue_project_review_prs_rejects_snake_case_head_sha() {
        let err = serde_json::from_value::<QueueProjectReviewPrsInput>(json!({
            "prs": [
                { "number": 42, "head_sha": "abc123" }
            ]
        }))
        .expect_err("snake_case field should be rejected");

        assert!(err.to_string().contains("unknown field `head_sha`"));
    }
}
