#[cfg(test)]
use pl_core::FunctionToolDefinition;
#[cfg(test)]
use pl_model::ToolSchema;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[cfg(test)]
use super::super::names::TOOL_GITHUB_API_REQUEST;

pub(crate) const GITHUB_API_REQUEST_DESCRIPTION: &str = "Call the current Mai project's GitHub REST API through the managed gh sidecar. \
     Use this for PR review submission, issue comments, labels, and other GitHub reads or writes. \
     For pull request reviews, submit the final review in one single POST to `/repos/OWNER/REPO/pulls/PR/reviews` with `event`, non-empty `body`, and optional inline comments in the `comments` array; do not create pending reviews, submit `/reviews/ID/events`, or POST inline comments to `/pulls/PR/comments`. \
     Credentials are supplied server-side and are not available to the agent container.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum GithubHttpMethod {
    Get,
    Post,
    Patch,
    Put,
    Delete,
}

impl GithubHttpMethod {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GithubApiRequest {
    /// HTTP method for gh api.
    pub(crate) method: GithubHttpMethod,
    /// GitHub API path beginning with `/`, optionally including a query string.
    pub(crate) path: String,
    /// Optional JSON object request body passed to gh api via stdin. Do not provide this field as a JSON-encoded string.
    #[serde(default, deserialize_with = "deserialize_optional_json_object")]
    pub(crate) body: Option<serde_json::Map<String, Value>>,
    /// Optional top-level response fields to retain. For array responses the selection is applied to each object. Error and pagination metadata are always retained.
    #[serde(default)]
    #[validate(length(max = 32))]
    pub(crate) fields: Vec<String>,
}

fn deserialize_optional_json_object<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Map<String, Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        Some(Value::Object(object)) => Ok(Some(object)),
        Some(_) => Err(serde::de::Error::custom(
            "field `body` must be a JSON object or null",
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
pub(crate) fn definitions() -> Vec<ToolSchema> {
    vec![ToolSchema::function(
        TOOL_GITHUB_API_REQUEST,
        GITHUB_API_REQUEST_DESCRIPTION,
        FunctionToolDefinition::<GithubApiRequest>::new(
            TOOL_GITHUB_API_REQUEST,
            GITHUB_API_REQUEST_DESCRIPTION,
        )
        .input_schema(),
    )]
}
