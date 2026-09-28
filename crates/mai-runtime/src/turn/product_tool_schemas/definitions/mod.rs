pub(crate) mod github;
pub(crate) mod review;
pub(crate) mod workflow;

use schemars::JsonSchema;
use serde_json::Value;

pub(crate) use github::{GITHUB_API_REQUEST_DESCRIPTION, GithubApiRequest, GithubHttpMethod};
#[cfg(test)]
pub(crate) use review::QueueProjectReviewPr;
pub(crate) use review::{QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION, QueueProjectReviewPrsInput};
pub(crate) use workflow::{
    READ_TOOL_ARTIFACT_DESCRIPTION, ReadToolArtifactInput, SAVE_ARTIFACT_DESCRIPTION,
    SAVE_TASK_PLAN_DESCRIPTION, SUBMIT_REVIEW_RESULT_DESCRIPTION, SaveArtifactInput,
    SaveTaskPlanInput, SubmitReviewResultInput, ToolArtifactRange,
};

/// 生成产品工具的稳定 JSON Schema。
///
/// 删除调试元数据并显式收紧对象输入，保证模型声明与本地反序列化 contract 一致。
pub(crate) fn input_schema<Input>() -> Value
where
    Input: JsonSchema,
{
    let mut schema = schemars::schema_for!(Input).to_value();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
        if object.get("type").and_then(Value::as_str) == Some("object")
            || object.contains_key("properties")
        {
            object.insert("additionalProperties".into(), Value::Bool(false));
        }
    }
    schema
}

#[cfg(test)]
pub(crate) fn builtin_tool_specs() -> Vec<pl_protocol::ToolSpec> {
    let mut tools = Vec::new();
    tools.extend(workflow::definitions());
    tools.extend(github::definitions());
    tools.extend(review::definitions());
    tools
}
