#[cfg(test)]
use pl_core::FunctionToolDefinition;
#[cfg(test)]
use pl_model::ToolSchema;
use schemars::JsonSchema;
use serde::Deserialize;

#[cfg(test)]
use super::super::names::TOOL_QUEUE_PROJECT_REVIEW_PRS;

pub(crate) const QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION: &str =
    "Queue one or more pull requests for the current Mai project's automatic review pool. \
     The server infers the project from the calling agent; do not provide a project id. \
     Use this only from project PR selector or reviewer workflows.";

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QueueProjectReviewPrsInput {
    /// Pull requests to queue for review.
    pub(crate) prs: Vec<QueueProjectReviewPr>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QueueProjectReviewPr {
    /// GitHub pull request number.
    #[validate(range(min = 1))]
    pub(crate) number: u64,
    /// Optional current PR head commit SHA.
    pub(crate) head_sha: Option<String>,
    /// Optional short reason this PR was selected.
    pub(crate) reason: Option<String>,
}

#[cfg(test)]
pub(crate) fn definitions() -> Vec<ToolSchema> {
    vec![ToolSchema::function(
        TOOL_QUEUE_PROJECT_REVIEW_PRS,
        QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
        FunctionToolDefinition::<QueueProjectReviewPrsInput>::new(
            TOOL_QUEUE_PROJECT_REVIEW_PRS,
            QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION,
        )
        .input_schema(),
    )]
}
