#[cfg(test)]
use pl_core::TypedTool;
#[cfg(test)]
use pl_model::ToolSpec;
use schemars::JsonSchema;
use serde::Deserialize;

#[cfg(test)]
use super::super::names::{
    TOOL_READ_TOOL_ARTIFACT, TOOL_SAVE_ARTIFACT, TOOL_SAVE_TASK_PLAN, TOOL_SUBMIT_REVIEW_RESULT,
};

pub(crate) const SAVE_TASK_PLAN_DESCRIPTION: &str = "Save or update the task plan. Each call replaces the previous plan and increments the version. \
     Plans must be decision-complete: the Executor should not need to make design decisions. \
     Use request_user_input to resolve any remaining ambiguity before saving.";

pub(crate) const SUBMIT_REVIEW_RESULT_DESCRIPTION: &str = "Submit the structured review result for a task workflow. Only reviewer agents attached to a task may call this.";

pub(crate) const SAVE_ARTIFACT_DESCRIPTION: &str = "Register a file as a downloadable artifact for the user. \
     Use this when you have produced a deliverable file (report, code output, data export, generated document, etc.) \
     that the user should be able to download from the web interface.";

pub(crate) const READ_TOOL_ARTIFACT_DESCRIPTION: &str = "Read one bounded range from a full output artifact returned by exec or write_stdin. \
     Copy outputArtifacts[].call_id to callId and outputArtifacts[].id to artifactId exactly; \
     use the receipt values, not outputFile, a tool item ID, or a provider call ID. \
     Select exactly one range kind: lines are UTF-8 text with a 1-based offset and a limit of at most 500; \
     bytes are base64 with a 0-based offset and a limit of at most 65536.";

#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SaveTaskPlanInput {
    /// Plan title.
    pub(crate) title: String,
    /// Full plan document in Markdown.
    pub(crate) markdown: String,
}

#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SubmitReviewResultInput {
    /// Whether the reviewed work passed.
    pub(crate) passed: bool,
    /// Structured findings in Markdown.
    pub(crate) findings: String,
    /// Short summary of the review verdict.
    pub(crate) summary: String,
}

#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SaveArtifactInput {
    /// Absolute path of the file inside the agent workspace.
    pub(crate) path: String,
    /// Display name for the artifact. Defaults to the filename from path.
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReadToolArtifactInput {
    /// Call id copied from outputArtifacts[].call_id.
    pub(crate) call_id: String,
    /// Artifact id copied from outputArtifacts[].id.
    pub(crate) artifact_id: String,
    /// Use lines for text output and bytes for binary output.
    pub(crate) range: ToolArtifactRange,
    /// 1-based first line for range=lines; 0-based first byte for range=bytes. Defaults to 1 or 0 respectively.
    pub(crate) offset: Option<u64>,
    /// Maximum lines (at most 500) or bytes (at most 65536) to return.
    #[validate(range(min = 1, max = 65536))]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ToolArtifactRange {
    Lines,
    Bytes,
}

#[cfg(test)]
pub(crate) fn definitions() -> Vec<ToolSpec> {
    vec![
        schema::<SaveTaskPlanInput>(TOOL_SAVE_TASK_PLAN, SAVE_TASK_PLAN_DESCRIPTION),
        schema::<SubmitReviewResultInput>(
            TOOL_SUBMIT_REVIEW_RESULT,
            SUBMIT_REVIEW_RESULT_DESCRIPTION,
        ),
        schema::<SaveArtifactInput>(TOOL_SAVE_ARTIFACT, SAVE_ARTIFACT_DESCRIPTION),
        schema::<ReadToolArtifactInput>(TOOL_READ_TOOL_ARTIFACT, READ_TOOL_ARTIFACT_DESCRIPTION),
    ]
}

#[cfg(test)]
fn schema<Input>(name: &str, description: &str) -> ToolSpec
where
    Input: JsonSchema,
{
    ToolSpec::function(
        name,
        description,
        TypedTool::<Input>::new(name, description).input_schema(),
    )
}
