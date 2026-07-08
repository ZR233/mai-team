use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole};
use pl_core::RegisteredTool;
use pl_model::ToolSchema;
use pl_protocol::PureError;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::state::AgentRecord;
use crate::turn::tool_output::ToolExecution;
use crate::{AgentRuntime, ProjectReviewQueueRequest, RuntimeError};

#[derive(Debug, Clone)]
pub(crate) struct QueueProjectReviewPr {
    pub(crate) number: u64,
    pub(crate) head_sha: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GithubApiRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: Option<Value>,
}

#[derive(Debug, Clone)]
enum MaiProductToolHandler {
    SaveTaskPlan,
    SubmitReviewResult,
    SaveArtifact,
    GithubApiRequest,
    QueueProjectReviewPrs,
}

impl MaiProductToolHandler {
    fn from_tool_name(name: &str) -> crate::Result<Self> {
        match name {
            mai_tools::TOOL_SAVE_TASK_PLAN => Ok(Self::SaveTaskPlan),
            mai_tools::TOOL_SUBMIT_REVIEW_RESULT => Ok(Self::SubmitReviewResult),
            mai_tools::TOOL_SAVE_ARTIFACT => Ok(Self::SaveArtifact),
            mai_tools::TOOL_GITHUB_API_REQUEST => Ok(Self::GithubApiRequest),
            mai_tools::TOOL_QUEUE_PROJECT_REVIEW_PRS => Ok(Self::QueueProjectReviewPrs),
            _ => Err(RuntimeError::InvalidInput(format!(
                "tool `{name}` is not a mai-team product tool"
            ))),
        }
    }

    async fn execute(
        &self,
        registry: &MaiProductToolRegistry,
        arguments: Value,
    ) -> crate::Result<ToolExecution> {
        if registry.cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        match self {
            Self::SaveTaskPlan => registry.save_task_plan(arguments).await,
            Self::SubmitReviewResult => registry.submit_review_result(arguments).await,
            Self::SaveArtifact => registry.save_artifact(arguments).await,
            Self::GithubApiRequest => registry.github_api_request(arguments).await,
            Self::QueueProjectReviewPrs => {
                let prs = queue_project_review_prs_from_arguments(&arguments)?;
                registry.queue_project_review_prs(prs).await
            }
        }
    }
}

/// 将 mai-team 产品工具挂入 pl-core agent kernel 的动态工具注册表。
///
/// 该注册器只承载 GitHub、review queue、artifact、task plan 和 MCP 资源等产品语义；
/// 工具生命周期、trace、tool result history 和模型回合调度仍由 pl-core 统一处理。
#[derive(Clone)]
pub(crate) struct MaiProductToolRegistry {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
    schemas: Vec<ToolSchema>,
    cancellation_token: CancellationToken,
}

impl MaiProductToolRegistry {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
        schemas: Vec<ToolSchema>,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
            schemas,
            cancellation_token,
        }
    }

    pub(crate) fn registered_tools(&self) -> crate::Result<Vec<RegisteredTool>> {
        self.schemas
            .iter()
            .map(|schema| {
                let ToolSchema::Function {
                    name,
                    description,
                    input_schema,
                } = schema
                else {
                    return Err(RuntimeError::InvalidInput(format!(
                        "mai-team product tool `{}` must be a function schema",
                        schema.name()
                    )));
                };
                let handler = MaiProductToolHandler::from_tool_name(name)?;
                let executor = self.clone();
                let tool_name = name.clone();
                Ok(RegisteredTool::new(
                    name.clone(),
                    description.clone(),
                    input_schema.clone(),
                    move |input, _context| {
                        let executor = executor.clone();
                        let handler = handler.clone();
                        let tool_name = tool_name.clone();
                        async move {
                            let execution = handler
                                .execute(&executor, input.arguments)
                                .await
                                .map_err(|error| PureError::ToolExecutionFailed {
                                    tool: tool_name.clone(),
                                    error: error.to_string(),
                                })?;
                            Ok(execution.into_tool_output())
                        }
                    },
                ))
            })
            .collect()
    }

    async fn save_task_plan(&self, arguments: Value) -> crate::Result<ToolExecution> {
        let title = required_string_argument(&arguments, "title")?;
        let markdown = required_string_argument(&arguments, "markdown")?;
        let task = self
            .runtime
            .save_task_plan(self.agent_id, title, markdown)
            .await?;
        Ok(json_tool_execution(&task))
    }

    async fn submit_review_result(&self, arguments: Value) -> crate::Result<ToolExecution> {
        let passed = arguments
            .get("passed")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                RuntimeError::InvalidInput("missing boolean field `passed`".to_string())
            })?;
        let findings = required_string_argument(&arguments, "findings")?;
        let summary = required_string_argument(&arguments, "summary")?;
        let review = self
            .runtime
            .submit_review_result(self.agent_id, passed, findings, summary)
            .await?;
        Ok(json_tool_execution(&review))
    }

    async fn save_artifact(&self, arguments: Value) -> crate::Result<ToolExecution> {
        let path = required_string_argument(&arguments, "path")?;
        let name = optional_string_argument(&arguments, "name");
        let artifact = self
            .runtime
            .save_artifact(self.agent_id, path, name)
            .await?;
        Ok(json_tool_execution(&artifact))
    }

    async fn github_api_request(&self, arguments: Value) -> crate::Result<ToolExecution> {
        let request = github_api_request_from_arguments(&arguments)?;
        self.runtime
            .execute_project_github_api_request(&self.agent, &request)
            .await
    }

    async fn queue_project_review_prs(
        &self,
        prs: Vec<QueueProjectReviewPr>,
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
        for pr in prs {
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
        Ok(ToolExecution::new(
            true,
            json!({
                "queued": queued,
                "deduped": deduped,
                "ignored": ignored,
            })
            .to_string(),
            false,
        ))
    }
}

fn required_string_argument(arguments: &Value, field: &str) -> crate::Result<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing string field `{field}`")))
}

fn optional_string_argument(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn json_tool_execution(value: &impl serde::Serialize) -> ToolExecution {
    ToolExecution::new(
        true,
        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
        false,
    )
}

fn queue_project_review_prs_from_arguments(
    arguments: &Value,
) -> crate::Result<Vec<QueueProjectReviewPr>> {
    let Some(items) = arguments.get("prs").and_then(Value::as_array) else {
        return Err(RuntimeError::InvalidInput(
            "missing array field `prs`".to_string(),
        ));
    };
    items
        .iter()
        .map(|item| {
            let number = item.get("number").and_then(Value::as_u64).ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "each `prs` item must include integer field `number`".to_string(),
                )
            })?;
            Ok(QueueProjectReviewPr {
                number,
                head_sha: optional_string_argument(item, "head_sha"),
                reason: optional_string_argument(item, "reason"),
            })
        })
        .collect()
}

fn github_api_request_from_arguments(arguments: &Value) -> crate::Result<GithubApiRequest> {
    Ok(GithubApiRequest {
        method: required_string_argument(arguments, "method")?,
        path: required_string_argument(arguments, "path")?,
        body: optional_json_body_argument(arguments, "body")?,
    })
}

fn optional_json_body_argument(arguments: &Value, field: &str) -> crate::Result<Option<Value>> {
    let Some(value) = arguments.get(field) else {
        return Ok(None);
    };
    let parsed = if let Some(raw) = value.as_str() {
        serde_json::from_str(raw).map_err(|err| {
            RuntimeError::InvalidInput(format!("field `{field}` must be JSON: {err}"))
        })?
    } else {
        value.clone()
    };
    if parsed.is_object() || parsed.is_null() {
        return Ok(Some(parsed));
    }
    Err(RuntimeError::InvalidInput(format!(
        "field `{field}` must be a JSON object or null"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn product_tools_register_as_pl_core_tools() {
        let source = include_str!("product_tools.rs");

        assert!(
            source.contains(&format!("{}{}", "Registered", "Tool")),
            "mai-team 产品工具必须作为 pl-core RegisteredTool 注册"
        );
        assert!(
            !source.contains(&format!("{}{}", "ProductTool", "Router")),
            "mai-team 不应保留产品工具 router 大分发层"
        );
        assert!(
            !source.contains(&format!("{}{}", "Tool", "Definition")),
            "mai-team 产品工具注册不应再经过 mai_protocol 旧工具定义类型"
        );
        assert!(
            !source.contains(&format!("{}{}", "execute_product", "_tool")),
            "mai-team 产品工具应在注册 RegisteredTool 时绑定具体 handler，而不是保留本地大分发入口"
        );
        assert!(
            !source.contains(&format!("{}{}", "TOOL_GIT_SYNC", "_DEFAULT_BRANCH")),
            "git_sync_default_branch 是 pl-core shared git tool，不应在产品工具注册器里兜底"
        );
        assert!(
            !source.contains("starts_with(\"mcp__\")"),
            "MCP model tools 应由 pl-core shared MCP tool pack 注册，不应作为 mai 产品工具"
        );
        assert!(
            !source.contains(&format!("{}{}", "execute_mcp", "_tool")),
            "MCP model tools 应通过 pl-core ToolSetBuilder 后端执行"
        );
    }

    #[test]
    fn github_api_request_from_arguments_parses_json_string_body() {
        let request = github_api_request_from_arguments(&json!({
            "method": "POST",
            "path": "/repos/owner/repo/pulls/42/reviews",
            "body": r#"{"event":"COMMENT","body":"Looks good."}"#
        }))
        .expect("request");

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/owner/repo/pulls/42/reviews");
        assert_eq!(
            request.body,
            Some(json!({
                "event": "COMMENT",
                "body": "Looks good."
            }))
        );
    }

    #[test]
    fn github_api_request_from_arguments_rejects_non_object_body() {
        let err = github_api_request_from_arguments(&json!({
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
}
