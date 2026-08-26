use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole, AgentSummary};
use pl_core::{
    Tool, ToolCallContext, ToolDirective, ToolResult, TypedTool, tool::cache::ToolCachePolicy,
};
use serde_json::{Value, json};

use crate::state::AgentRecord;
use crate::turn::product_tool_schemas::definitions::{
    GITHUB_API_REQUEST_DESCRIPTION, QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
    QueueProjectReviewPrsInput, READ_TOOL_ARTIFACT_DESCRIPTION, ReadToolArtifactInput,
    SAVE_ARTIFACT_DESCRIPTION, SAVE_TASK_PLAN_DESCRIPTION, SUBMIT_REVIEW_RESULT_DESCRIPTION,
    SaveArtifactInput, SaveTaskPlanInput, SubmitReviewResultInput,
};
use crate::turn::product_tool_schemas::{
    TOOL_GITHUB_API_REQUEST, TOOL_QUEUE_PROJECT_REVIEW_PRS, TOOL_READ_TOOL_ARTIFACT,
    TOOL_SAVE_ARTIFACT, TOOL_SAVE_TASK_PLAN, TOOL_SUBMIT_REVIEW_RESULT,
};
use crate::{AgentRuntime, ProjectReviewQueueRequest, RuntimeError};

pub(crate) use crate::turn::product_tool_schemas::definitions::{
    GithubApiRequest, GithubHttpMethod,
};

/// 将 mai-team 产品工具挂入 pl-core agent kernel 的动态工具注册表。
///
/// 该注册器只承载 GitHub、review queue、artifact 和 task plan 等产品语义；
/// 工具生命周期、trace、tool result history 和模型回合调度仍由 pl-core 统一处理。
#[derive(Clone)]
pub(crate) struct MaiProductTools {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
}

impl MaiProductTools {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
        }
    }

    pub(crate) fn tools(&self, summary: &AgentSummary) -> crate::Result<Vec<Arc<dyn Tool>>> {
        [
            TOOL_SAVE_TASK_PLAN,
            TOOL_SUBMIT_REVIEW_RESULT,
            TOOL_SAVE_ARTIFACT,
            TOOL_READ_TOOL_ARTIFACT,
            TOOL_GITHUB_API_REQUEST,
            TOOL_QUEUE_PROJECT_REVIEW_PRS,
        ]
        .into_iter()
        .filter(|name| product_tool_is_installed(name, summary))
        .map(|name| self.tool(name))
        .collect()
    }

    fn tool(&self, name: &str) -> crate::Result<Arc<dyn Tool>> {
        match name {
            TOOL_SAVE_TASK_PLAN => {
                let executor = self.clone();
                Ok(Arc::new(
                    TypedTool::<SaveTaskPlanInput>::new(
                        TOOL_SAVE_TASK_PLAN,
                        SAVE_TASK_PLAN_DESCRIPTION,
                    )
                    .handler(move |input, _context| {
                        let executor = executor.clone();
                        async move { executor.save_task_plan(input).await }
                    }),
                ) as Arc<dyn Tool>)
            }
            TOOL_SUBMIT_REVIEW_RESULT => {
                let executor = self.clone();
                Ok(Arc::new(
                    TypedTool::<SubmitReviewResultInput>::new(
                        TOOL_SUBMIT_REVIEW_RESULT,
                        SUBMIT_REVIEW_RESULT_DESCRIPTION,
                    )
                    .handler(move |input, _context| {
                        let executor = executor.clone();
                        async move { executor.submit_review_result(input).await }
                    }),
                ) as Arc<dyn Tool>)
            }
            TOOL_SAVE_ARTIFACT => {
                let executor = self.clone();
                Ok(Arc::new(
                    TypedTool::<SaveArtifactInput>::new(
                        TOOL_SAVE_ARTIFACT,
                        SAVE_ARTIFACT_DESCRIPTION,
                    )
                    .handler(move |input, _context| {
                        let executor = executor.clone();
                        async move { executor.save_artifact(input).await }
                    }),
                ) as Arc<dyn Tool>)
            }
            TOOL_READ_TOOL_ARTIFACT => {
                let executor = self.clone();
                Ok(Arc::new(
                    TypedTool::<ReadToolArtifactInput>::new(
                        TOOL_READ_TOOL_ARTIFACT,
                        READ_TOOL_ARTIFACT_DESCRIPTION,
                    )
                    .handler(move |input, _context| {
                        let executor = executor.clone();
                        async move { executor.read_tool_artifact(input).await }
                    })
                    .with_cache_policy(ToolCachePolicy::WithinTurn),
                ) as Arc<dyn Tool>)
            }
            TOOL_GITHUB_API_REQUEST => {
                let executor = self.clone();
                Ok(Arc::new(
                    TypedTool::<GithubApiRequest>::new(
                        TOOL_GITHUB_API_REQUEST,
                        GITHUB_API_REQUEST_DESCRIPTION,
                    )
                    .handler(move |input, context| {
                        let executor = executor.clone();
                        async move { executor.github_api_request(input, context).await }
                    })
                    .with_cache_policy_resolver(github_api_cache_policy)
                    .with_cache_invalidation_resolver(github_api_invalidates_cache),
                ) as Arc<dyn Tool>)
            }
            TOOL_QUEUE_PROJECT_REVIEW_PRS => {
                let executor = self.clone();
                Ok(Arc::new(
                    TypedTool::<QueueProjectReviewPrsInput>::new(
                        TOOL_QUEUE_PROJECT_REVIEW_PRS,
                        QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
                    )
                    .handler(move |input, _context| {
                        let executor = executor.clone();
                        async move { executor.queue_project_review_prs(input).await }
                    }),
                ) as Arc<dyn Tool>)
            }
            unknown => Err(RuntimeError::InvalidInput(format!(
                "tool `{unknown}` is not a mai-team product tool"
            ))),
        }
    }

    async fn save_task_plan(&self, input: SaveTaskPlanInput) -> crate::Result<ToolResult> {
        let task = self
            .runtime
            .save_task_plan(self.agent_id, input.title, input.markdown)
            .await?;
        Ok(ToolResult::json(&task)?)
    }

    async fn submit_review_result(
        &self,
        input: SubmitReviewResultInput,
    ) -> crate::Result<ToolResult> {
        let review = self
            .runtime
            .submit_review_result(self.agent_id, input.passed, input.findings, input.summary)
            .await?;
        Ok(ToolResult::json(&review)?)
    }

    async fn save_artifact(&self, input: SaveArtifactInput) -> crate::Result<ToolResult> {
        let artifact = self
            .runtime
            .save_artifact(self.agent_id, input.path, input.name)
            .await?;
        Ok(ToolResult::json(&artifact)?)
    }

    async fn read_tool_artifact(&self, input: ReadToolArtifactInput) -> crate::Result<ToolResult> {
        let output = super::tool_artifact::read(&self.runtime, self.agent_id, input).await?;
        Ok(ToolResult::json(output)?)
    }

    async fn github_api_request(
        &self,
        request: GithubApiRequest,
        context: ToolCallContext,
    ) -> crate::Result<ToolResult> {
        if request.method != GithubHttpMethod::Get {
            self.runtime
                .await_agent_durable(self.agent_id, context.identity().revision_base)
                .await?;
        }
        let mut execution = self
            .runtime
            .execute_project_github_api_request(&self.agent, &request)
            .await?;
        let output = execution.content.canonical_text();
        if output.len() > execution.model_output.len() {
            let call_id = context.identity().call_id.clone();
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
            tokio::fs::write(&path, output.as_bytes()).await?;
            execution
                .runtime_events
                .push(ToolDirective::OutputArtifacts {
                    artifacts: vec![
                        serde_json::to_value(mai_protocol::ToolOutputArtifactInfo {
                            id: artifact_id,
                            call_id,
                            agent_id: self.agent_id,
                            name: name.to_string(),
                            stream: "response".to_string(),
                            size_bytes: output.len() as u64,
                            created_at: mai_protocol::now(),
                        })
                        .map_err(|error| {
                            RuntimeError::InvalidInput(format!(
                                "failed to serialize tool output artifact: {error}"
                            ))
                        })?,
                    ],
                });
        }
        Ok(execution)
    }

    async fn queue_project_review_prs(
        &self,
        input: QueueProjectReviewPrsInput,
    ) -> crate::Result<ToolResult> {
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
        Ok(ToolResult::json(json!({
            "queued": queued,
            "deduped": deduped,
            "ignored": ignored,
        }))?)
    }
}

fn product_tool_is_installed(name: &str, summary: &AgentSummary) -> bool {
    match name {
        TOOL_SAVE_TASK_PLAN => {
            summary.task_id.is_some() && matches!(summary.role, Some(AgentRole::Planner))
        }
        TOOL_SUBMIT_REVIEW_RESULT => {
            summary.task_id.is_some() && matches!(summary.role, Some(AgentRole::Reviewer))
        }
        TOOL_QUEUE_PROJECT_REVIEW_PRS => {
            summary.project_id.is_some()
                && matches!(
                    summary.role,
                    Some(AgentRole::Explorer | AgentRole::Reviewer)
                )
        }
        TOOL_SAVE_ARTIFACT | TOOL_READ_TOOL_ARTIFACT | TOOL_GITHUB_API_REQUEST => true,
        _ => false,
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
            vec![
                crate::turn::product_tool_schemas::definitions::QueueProjectReviewPr {
                    number: 42,
                    head_sha: Some("abc123".to_string()),
                    reason: Some("ready".to_string()),
                }
            ]
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
