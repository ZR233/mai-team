pub(crate) mod github;
pub(crate) mod review;
pub(crate) mod workflow;

pub(crate) use github::{GITHUB_API_REQUEST_DESCRIPTION, GithubApiRequest};
#[cfg(test)]
pub(crate) use review::QueueProjectReviewPr;
pub(crate) use review::{QUEUE_PROJECT_REVIEW_PRS_DESCRIPTION, QueueProjectReviewPrsInput};
pub(crate) use workflow::{
    READ_TOOL_ARTIFACT_DESCRIPTION, ReadToolArtifactInput, SAVE_ARTIFACT_DESCRIPTION,
    SAVE_TASK_PLAN_DESCRIPTION, SUBMIT_REVIEW_RESULT_DESCRIPTION, SaveArtifactInput,
    SaveTaskPlanInput, SubmitReviewResultInput, ToolArtifactRange,
};

#[cfg(test)]
pub(crate) fn builtin_tool_schemas() -> Vec<pl_model::ToolSchema> {
    let mut tools = Vec::new();
    tools.extend(workflow::definitions());
    tools.extend(github::definitions());
    tools.extend(review::definitions());
    tools
}
