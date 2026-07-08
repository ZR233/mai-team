use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole};
use pl_core::RegisteredTool;
use pl_model::ToolSchema;
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
            .map(|schema| self.registered_tool(schema))
            .collect()
    }

    fn registered_tool(&self, schema: &ToolSchema) -> crate::Result<RegisteredTool> {
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
        match name.as_str() {
            mai_tools::TOOL_SAVE_TASK_PLAN => {
                let executor = self.clone();
                let tool_name = name.clone();
                Ok(RegisteredTool::from_execution_result(
                    name.clone(),
                    description.clone(),
                    input_schema.clone(),
                    move |input, _context| {
                        let executor = executor.clone();
                        let tool_name = tool_name.clone();
                        async move {
                            executor.ensure_not_cancelled(&tool_name)?;
                            let execution = executor
                                .save_task_plan(input.arguments)
                                .await
                                .map_err(|error| product_tool_error(&tool_name, error))?;
                            Ok(execution)
                        }
                    },
                ))
            }
            mai_tools::TOOL_SUBMIT_REVIEW_RESULT => {
                let executor = self.clone();
                let tool_name = name.clone();
                Ok(RegisteredTool::from_execution_result(
                    name.clone(),
                    description.clone(),
                    input_schema.clone(),
                    move |input, _context| {
                        let executor = executor.clone();
                        let tool_name = tool_name.clone();
                        async move {
                            executor.ensure_not_cancelled(&tool_name)?;
                            let execution = executor
                                .submit_review_result(input.arguments)
                                .await
                                .map_err(|error| product_tool_error(&tool_name, error))?;
                            Ok(execution)
                        }
                    },
                ))
            }
            mai_tools::TOOL_SAVE_ARTIFACT => {
                let executor = self.clone();
                let tool_name = name.clone();
                Ok(RegisteredTool::from_execution_result(
                    name.clone(),
                    description.clone(),
                    input_schema.clone(),
                    move |input, _context| {
                        let executor = executor.clone();
                        let tool_name = tool_name.clone();
                        async move {
                            executor.ensure_not_cancelled(&tool_name)?;
                            let execution = executor
                                .save_artifact(input.arguments)
                                .await
                                .map_err(|error| product_tool_error(&tool_name, error))?;
                            Ok(execution)
                        }
                    },
                ))
            }
            mai_tools::TOOL_GITHUB_API_REQUEST => {
                let executor = self.clone();
                let tool_name = name.clone();
                Ok(RegisteredTool::from_execution_result(
                    name.clone(),
                    description.clone(),
                    input_schema.clone(),
                    move |input, _context| {
                        let executor = executor.clone();
                        let tool_name = tool_name.clone();
                        async move {
                            executor.ensure_not_cancelled(&tool_name)?;
                            let execution = executor
                                .github_api_request(input.arguments)
                                .await
                                .map_err(|error| product_tool_error(&tool_name, error))?;
                            Ok(execution)
                        }
                    },
                ))
            }
            mai_tools::TOOL_QUEUE_PROJECT_REVIEW_PRS => {
                let executor = self.clone();
                let tool_name = name.clone();
                Ok(RegisteredTool::from_execution_result(
                    name.clone(),
                    description.clone(),
                    input_schema.clone(),
                    move |input, _context| {
                        let executor = executor.clone();
                        let tool_name = tool_name.clone();
                        async move {
                            executor.ensure_not_cancelled(&tool_name)?;
                            let prs = queue_project_review_prs_from_arguments(&input.arguments)
                                .map_err(|error| product_tool_error(&tool_name, error))?;
                            let execution = executor
                                .queue_project_review_prs(prs)
                                .await
                                .map_err(|error| product_tool_error(&tool_name, error))?;
                            Ok(execution)
                        }
                    },
                ))
            }
            _ => Err(RuntimeError::InvalidInput(format!(
                "tool `{name}` is not a mai-team product tool"
            ))),
        }
    }

    fn ensure_not_cancelled(&self, tool_name: &str) -> pl_protocol::Result<()> {
        if self.cancellation_token.is_cancelled() {
            return Err(pl_protocol::PureError::ToolExecutionFailed {
                tool: tool_name.to_string(),
                error: RuntimeError::TurnCancelled.to_string(),
            });
        }
        Ok(())
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

fn product_tool_error(tool_name: &str, error: impl std::fmt::Display) -> pl_protocol::PureError {
    pl_protocol::PureError::ToolExecutionFailed {
        tool: tool_name.to_string(),
        error: error.to_string(),
    }
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
            !source.contains(&format!("{}{}", "enum MaiProduct", "ToolHandler")),
            "mai-team 产品工具不应再通过本地 handler enum 做二次 route"
        );
        assert!(
            !source.contains(&format!("{}{}", "from_tool", "_name")),
            "产品工具 schema 应直接绑定具体 RegisteredTool handler"
        );
        assert!(
            source.contains("RegisteredTool::from_execution_result"),
            "产品工具应让 pl-core 负责 ToolExecutionResult 到 ToolOutput 的映射"
        );
        assert!(
            !source.contains(&format!("{}{}", "into_tool", "_output")),
            "产品工具注册器不应在 mai-team 手动映射 ToolOutput"
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
