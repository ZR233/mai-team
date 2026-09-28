use std::{fmt, future::Future, marker::PhantomData, sync::Arc};

use mai_protocol::{AgentId, AgentRole, AgentSummary, ToolOutputArtifactInfo};
use pl_core::{
    context::{ContextContent, OpaquePayload},
    tool::{
        ToolOutput,
        opaque::{CallContext, Registration, Tool, ToolError},
    },
};
use pl_model::runtime::thread_tool_declaration;
use pl_protocol::ToolSpec;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;

use crate::state::AgentRecord;
use crate::turn::product_tool_schemas::definitions::{
    GITHUB_API_REQUEST_DESCRIPTION, QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
    QueueProjectReviewPrsInput, READ_TOOL_ARTIFACT_DESCRIPTION, ReadToolArtifactInput,
    SAVE_ARTIFACT_DESCRIPTION, SAVE_TASK_PLAN_DESCRIPTION, SUBMIT_REVIEW_RESULT_DESCRIPTION,
    SaveArtifactInput, SaveTaskPlanInput, SubmitReviewResultInput, input_schema,
};
use crate::turn::product_tool_schemas::{
    TOOL_GITHUB_API_REQUEST, TOOL_QUEUE_PROJECT_REVIEW_PRS, TOOL_READ_TOOL_ARTIFACT,
    TOOL_SAVE_ARTIFACT, TOOL_SAVE_TASK_PLAN, TOOL_SUBMIT_REVIEW_RESULT,
};
use crate::{AgentRuntime, ProjectReviewQueueRequest, RuntimeError};

pub(crate) use crate::turn::product_tool_schemas::definitions::{
    GithubApiRequest, GithubHttpMethod,
};

/// GitHub 产品动作的完整输出与模型可见投影。
///
/// 调用方保留原始响应文本，由产品工具在拥有 call identity 后决定是否落盘 artifact。
pub(crate) struct GithubApiExecution {
    pub(crate) full_output: String,
    pub(crate) model_output: String,
}

/// 将 mai-team 产品动作注册到 pl-core Thread 的本地工具注册表。
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

    /// 按当前 Agent 角色构造产品动作注册项。
    ///
    /// 注册项所有权会转移给唯一一个 Thread；调用方不得缓存或重复安装同一批实例。
    pub(crate) fn registrations(&self, summary: &AgentSummary) -> crate::Result<Vec<Registration>> {
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
        .map(|name| self.registration(name))
        .collect()
    }

    fn registration(&self, name: &str) -> crate::Result<Registration> {
        match name {
            TOOL_SAVE_TASK_PLAN => {
                let executor = self.clone();
                product_registration(
                    TOOL_SAVE_TASK_PLAN,
                    SAVE_TASK_PLAN_DESCRIPTION,
                    move |input: SaveTaskPlanInput, _| {
                        let executor = executor.clone();
                        async move { executor.save_task_plan(input).await }
                    },
                )
            }
            TOOL_SUBMIT_REVIEW_RESULT => {
                let executor = self.clone();
                product_registration(
                    TOOL_SUBMIT_REVIEW_RESULT,
                    SUBMIT_REVIEW_RESULT_DESCRIPTION,
                    move |input: SubmitReviewResultInput, _| {
                        let executor = executor.clone();
                        async move { executor.submit_review_result(input).await }
                    },
                )
            }
            TOOL_SAVE_ARTIFACT => {
                let executor = self.clone();
                product_registration(
                    TOOL_SAVE_ARTIFACT,
                    SAVE_ARTIFACT_DESCRIPTION,
                    move |input: SaveArtifactInput, _| {
                        let executor = executor.clone();
                        async move { executor.save_artifact(input).await }
                    },
                )
            }
            TOOL_READ_TOOL_ARTIFACT => {
                let executor = self.clone();
                product_registration(
                    TOOL_READ_TOOL_ARTIFACT,
                    READ_TOOL_ARTIFACT_DESCRIPTION,
                    move |input: ReadToolArtifactInput, _| {
                        let executor = executor.clone();
                        async move { executor.read_tool_artifact(input).await }
                    },
                )
            }
            TOOL_GITHUB_API_REQUEST => {
                let executor = self.clone();
                product_registration(
                    TOOL_GITHUB_API_REQUEST,
                    GITHUB_API_REQUEST_DESCRIPTION,
                    move |input: GithubApiRequest, context| {
                        let executor = executor.clone();
                        async move { executor.github_api_request(input, context).await }
                    },
                )
            }
            TOOL_QUEUE_PROJECT_REVIEW_PRS => {
                let executor = self.clone();
                product_registration(
                    TOOL_QUEUE_PROJECT_REVIEW_PRS,
                    QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
                    move |input: QueueProjectReviewPrsInput, _| {
                        let executor = executor.clone();
                        async move { executor.queue_project_review_prs(input).await }
                    },
                )
            }
            unknown => Err(RuntimeError::InvalidInput(format!(
                "tool `{unknown}` is not a mai-team product tool"
            ))),
        }
    }

    async fn save_task_plan(&self, input: SaveTaskPlanInput) -> crate::Result<ToolOutput> {
        let task = self
            .runtime
            .save_task_plan(self.agent_id, input.title, input.markdown)
            .await?;
        json_tool_output(task)
    }

    async fn submit_review_result(
        &self,
        input: SubmitReviewResultInput,
    ) -> crate::Result<ToolOutput> {
        let review = self
            .runtime
            .submit_review_result(self.agent_id, input.passed, input.findings, input.summary)
            .await?;
        json_tool_output(review)
    }

    async fn save_artifact(&self, input: SaveArtifactInput) -> crate::Result<ToolOutput> {
        let artifact = self
            .runtime
            .save_artifact(self.agent_id, input.path, input.name)
            .await?;
        json_tool_output(artifact)
    }

    async fn read_tool_artifact(&self, input: ReadToolArtifactInput) -> crate::Result<ToolOutput> {
        let output = super::tool_artifact::read(&self.runtime, self.agent_id, input).await?;
        json_tool_output(output)
    }

    async fn github_api_request(
        &self,
        request: GithubApiRequest,
        context: CallContext,
    ) -> crate::Result<ToolOutput> {
        if request.method != GithubHttpMethod::Get {
            self.runtime
                .await_agent_durable(self.agent_id, context.history_fence)
                .await?;
        }

        let execution = self
            .runtime
            .execute_project_github_api_request(&self.agent, &request)
            .await?;
        let mut output_artifacts = Vec::new();
        if execution.full_output != execution.model_output {
            let artifact_id = uuid::Uuid::new_v4().to_string();
            let name = "github-api-response.json";
            let path = self.runtime.tool_output_artifact_file_path(
                self.agent_id,
                &context.call_id,
                &artifact_id,
                name,
            );
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, execution.full_output.as_bytes()).await?;
            output_artifacts.push(ToolOutputArtifactInfo {
                id: artifact_id,
                call_id: context.call_id.clone(),
                agent_id: self.agent_id,
                name: name.to_string(),
                stream: "response".to_string(),
                size_bytes: execution.full_output.len() as u64,
                created_at: mai_protocol::now(),
            });
        }

        let payload = GithubApiToolPayload {
            output: execution.full_output,
            output_artifacts,
        };
        let encoded = serde_json::to_string(&payload).map_err(|error| {
            RuntimeError::InvalidInput(format!("failed to encode GitHub output: {error}"))
        })?;
        let payload = OpaquePayload::new("mai.github-api-result", 1, encoded).map_err(|error| {
            RuntimeError::InvalidInput(format!("failed to freeze GitHub output: {error}"))
        })?;
        Ok(ToolOutput::new(
            payload,
            vec![ContextContent::Text {
                text: Arc::from(execution.model_output),
            }],
        ))
    }

    async fn queue_project_review_prs(
        &self,
        input: QueueProjectReviewPrsInput,
    ) -> crate::Result<ToolOutput> {
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
        json_tool_output(json!({
            "queued": queued,
            "deduped": deduped,
            "ignored": ignored,
        }))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GithubApiToolPayload {
    output: String,
    output_artifacts: Vec<ToolOutputArtifactInfo>,
}

/// 产品 JSON 函数工具的本地执行适配器。
///
/// 输入 contract 由产品 schema 模块生成；这里只做编码边界校验和反序列化。
struct ProductJsonTool<Input, Invoke> {
    tool_id: &'static str,
    invoke: Invoke,
    input: PhantomData<fn() -> Input>,
}

impl<Input, Invoke> fmt::Debug for ProductJsonTool<Input, Invoke> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProductJsonTool")
            .field("tool_id", &self.tool_id)
            .finish_non_exhaustive()
    }
}

impl<Input, Invoke, OutputFuture> Tool for ProductJsonTool<Input, Invoke>
where
    Input: DeserializeOwned + Send + 'static,
    Invoke: Fn(Input, CallContext) -> OutputFuture + Send + Sync + 'static,
    OutputFuture: Future<Output = crate::Result<ToolOutput>> + Send,
{
    async fn execute(
        &self,
        input: OpaquePayload,
        context: CallContext,
    ) -> Result<ToolOutput, ToolError> {
        if input.format() != "application/json" || input.version() != 1 {
            return Err(ToolError::new(RuntimeError::InvalidInput(format!(
                "tool `{}` expected application/json v1 arguments",
                self.tool_id
            ))));
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolError::new(RuntimeError::TurnCancelled));
        }
        let input = serde_json::from_str(input.content())
            .map_err(|error| ToolError::new(RuntimeError::InvalidInput(error.to_string())))?;
        (self.invoke)(input, context).await.map_err(ToolError::new)
    }
}

fn product_registration<Input, Invoke, OutputFuture>(
    name: &'static str,
    description: &'static str,
    invoke: Invoke,
) -> crate::Result<Registration>
where
    Input: DeserializeOwned + JsonSchema + Send + 'static,
    Invoke: Fn(Input, CallContext) -> OutputFuture + Send + Sync + 'static,
    OutputFuture: Future<Output = crate::Result<ToolOutput>> + Send,
{
    let spec = ToolSpec::function(name, description, input_schema::<Input>());
    let declaration = thread_tool_declaration(&spec).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "failed to encode product tool `{name}` declaration: {error}"
        ))
    })?;
    Registration::new(
        name.to_string(),
        declaration,
        ProductJsonTool {
            tool_id: name,
            invoke,
            input: PhantomData,
        },
    )
    .map_err(|error| {
        RuntimeError::InvalidInput(format!("failed to register product tool `{name}`: {error}"))
    })
}

fn json_tool_output(value: impl Serialize) -> crate::Result<ToolOutput> {
    let encoded = serde_json::to_string(&value).map_err(|error| {
        RuntimeError::InvalidInput(format!("failed to encode tool output: {error}"))
    })?;
    let payload =
        OpaquePayload::new("mai.product.tool-output", 1, encoded.clone()).map_err(|error| {
            RuntimeError::InvalidInput(format!("failed to freeze tool output: {error}"))
        })?;
    Ok(ToolOutput::new(
        payload,
        vec![ContextContent::Text {
            text: Arc::from(encoded),
        }],
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

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
