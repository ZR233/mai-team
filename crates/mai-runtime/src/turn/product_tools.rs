use std::sync::Arc;

use mai_protocol::{AgentId, AgentRole};
use pl_core::RegisteredTool;
use pl_model::ToolSchema;
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeOwned},
};
use serde_json::{Value, json};

use crate::state::AgentRecord;
use crate::turn::tool_output::ToolExecution;
use crate::{AgentRuntime, ProjectReviewQueueRequest, RuntimeError};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QueueProjectReviewPr {
    pub(crate) number: u64,
    pub(crate) head_sha: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GithubApiRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    #[serde(default, deserialize_with = "deserialize_optional_json_object")]
    pub(crate) body: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveTaskPlanInput {
    title: String,
    markdown: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmitReviewResultInput {
    passed: bool,
    findings: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveArtifactInput {
    path: String,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueProjectReviewPrsInput {
    prs: Vec<QueueProjectReviewPr>,
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
}

impl MaiProductToolRegistry {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
        schemas: Vec<ToolSchema>,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
            schemas,
        }
    }

    pub(crate) fn registered_tools(&self) -> crate::Result<Vec<RegisteredTool>> {
        self.schemas
            .iter()
            .cloned()
            .map(|schema| self.registered_tool(schema))
            .collect()
    }

    fn registered_tool(&self, schema: ToolSchema) -> crate::Result<RegisteredTool> {
        let name = schema.name().to_string();
        match name.as_str() {
            crate::turn::product_tool_schemas::TOOL_SAVE_TASK_PLAN => {
                let executor = self.clone();
                Self::registered_schema_tool(schema, move |input: SaveTaskPlanInput, _context| {
                    let executor = executor.clone();
                    async move { executor.save_task_plan(input).await }
                })
            }
            crate::turn::product_tool_schemas::TOOL_SUBMIT_REVIEW_RESULT => {
                let executor = self.clone();
                Self::registered_schema_tool(
                    schema,
                    move |input: SubmitReviewResultInput, _context| {
                        let executor = executor.clone();
                        async move { executor.submit_review_result(input).await }
                    },
                )
            }
            crate::turn::product_tool_schemas::TOOL_SAVE_ARTIFACT => {
                let executor = self.clone();
                Self::registered_schema_tool(schema, move |input: SaveArtifactInput, _context| {
                    let executor = executor.clone();
                    async move { executor.save_artifact(input).await }
                })
            }
            crate::turn::product_tool_schemas::TOOL_GITHUB_API_REQUEST => {
                let executor = self.clone();
                Self::registered_schema_tool(schema, move |input: GithubApiRequest, _context| {
                    let executor = executor.clone();
                    async move { executor.github_api_request(input).await }
                })
            }
            crate::turn::product_tool_schemas::TOOL_QUEUE_PROJECT_REVIEW_PRS => {
                let executor = self.clone();
                Self::registered_schema_tool(
                    schema,
                    move |input: QueueProjectReviewPrsInput, _context| {
                        let executor = executor.clone();
                        async move { executor.queue_project_review_prs(input).await }
                    },
                )
            }
            _ => Err(RuntimeError::InvalidInput(format!(
                "tool `{name}` is not a mai-team product tool"
            ))),
        }
    }

    fn registered_schema_tool<Input, F, Fut>(
        schema: ToolSchema,
        handler: F,
    ) -> crate::Result<RegisteredTool>
    where
        Input: DeserializeOwned + Send + 'static,
        F: Fn(Input, pl_core::ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<ToolExecution>> + Send + 'static,
    {
        RegisteredTool::from_schema_typed_fallible_execution_result(schema, handler)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
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

    async fn github_api_request(&self, request: GithubApiRequest) -> crate::Result<ToolExecution> {
        self.runtime
            .execute_project_github_api_request(&self.agent, &request)
            .await
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

fn deserialize_optional_json_object<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    if value
        .as_ref()
        .is_none_or(|value| value.is_object() || value.is_null())
    {
        return Ok(value);
    }
    Err(de::Error::custom(
        "field `body` must be a JSON object or null",
    ))
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
            source.contains(&format!(
                "{}{}",
                "RegisteredTool::from_schema_typed_fallible_", "execution_result"
            )),
            "产品工具应把 schema 解包、输入解析和 ToolExecutionResult 投影全部交给 pl-core"
        );
        assert!(
            !source.contains(&format!("{}{}", "product_tool", "_error")),
            "产品工具注册器不应在 mai-team 手动包装 pl-core 工具错误"
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
    fn product_tools_delegate_cancellation_to_pl_core_registered_tool() {
        let source = include_str!("product_tools.rs");

        assert!(
            !source.contains(&format!("{}{}", "ensure_not", "_cancelled")),
            "产品工具取消检查应由 pl-core RegisteredTool 统一处理"
        );
        assert!(
            !source.contains(&format!(
                "{}{}{}",
                "RuntimeError::Turn", "Cancelled", ".to_string()"
            )),
            "产品工具不应自行把 turn cancelled 映射为工具错误"
        );
    }

    #[test]
    fn product_tools_reuse_pl_core_json_execution_result() {
        let source = include_str!("product_tools.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("ToolExecution::json("),
            "产品工具 JSON 输出应复用 pl-core ToolExecutionResult::json"
        );
        assert!(
            !production.contains(&format!("{}{}", "fn json_tool", "_execution")),
            "mai-team 不应保留本地 JSON 工具输出 helper"
        );
        assert!(
            !production.contains("serde_json::to_string(value).unwrap_or_else"),
            "产品工具不应自行吞掉 JSON 序列化错误"
        );
    }

    #[test]
    fn product_tools_use_pl_core_typed_input_registration() {
        let source = include_str!("product_tools.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("RegisteredTool::from_schema_typed_fallible_execution_result"),
            "产品工具 schema 解包和输入反序列化应由 pl-core typed RegisteredTool 统一处理"
        );
        for forbidden in [
            "ToolSchema::Function",
            "description.clone()",
            "input_schema.clone()",
            "input.arguments",
            "arguments.get(",
            "fn required_string_argument",
            "fn optional_string_argument",
            "fn github_api_request_from_arguments",
            "fn queue_project_review_prs_from_arguments",
            "serde_json::from_value",
        ] {
            assert!(
                !production.contains(forbidden),
                "产品工具注册器不应手写输入解析 `{forbidden}`"
            );
        }
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
            vec![QueueProjectReviewPr {
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
