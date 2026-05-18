use mai_protocol::{
    AgentRole, AgentStatus, AgentSummary, ProjectReviewOutcome, ProjectReviewStatus,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::Duration;

use crate::{Result, RuntimeError};

pub(crate) mod backoff;
pub(crate) mod cleanup;
pub(crate) mod cycle;
pub(crate) mod eligibility;
pub(crate) mod pool;
pub(crate) mod relay_queue;
pub(crate) mod relay_selector;
pub(crate) mod reviewer;
pub(crate) mod runs;
pub(crate) mod selection;
pub(crate) mod selector;
pub(crate) mod state;
pub(crate) mod worker;

const PROJECT_REVIEW_IDLE_RETRY_SECS: u64 = 120;
const PROJECT_REVIEW_SELECTOR_INTERVAL_SECS: u64 = 1800;
const PROJECT_REVIEW_RETRY_INITIAL_SECS: u64 = 1;
const PROJECT_REVIEW_FAILURE_RETRY_SECS: u64 = 600;

fn project_review_retry_backoff() -> backoff::ProjectReviewRetryBackoff {
    backoff::ProjectReviewRetryBackoff::new(backoff::ProjectReviewRetryBackoffConfig {
        initial_delay: Duration::from_secs(PROJECT_REVIEW_RETRY_INITIAL_SECS),
        max_delay: Duration::from_secs(PROJECT_REVIEW_FAILURE_RETRY_SECS),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct ProjectReviewCycleReport {
    outcome: ProjectReviewOutcome,
    #[serde(default)]
    pr: Option<u64>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectReviewCycleResult {
    pub(crate) outcome: ProjectReviewOutcome,
    pub(crate) pr: Option<u64>,
    pub(crate) summary: Option<String>,
    pub(crate) error: Option<String>,
}

pub(crate) struct ProjectReviewLoopDecision {
    pub(crate) delay: Duration,
    pub(crate) status: ProjectReviewStatus,
    pub(crate) outcome: Option<ProjectReviewOutcome>,
    pub(crate) summary: Option<String>,
    pub(crate) error: Option<String>,
}

pub(crate) fn project_reviewer_system_prompt() -> &'static str {
    "You are an autonomous project pull request reviewer. Review exactly one eligible GitHub pull request for this project, using only the Mai GitHub API tools visible in the current tool list for GitHub reads/writes and local shell commands for git/test work. The repository has already been cloned and synced at /workspace/repo, and this reviewer owns that isolated clone. Do not look for GitHub tokens in the environment or write credentials. Finish with only the required JSON object so the project scheduler can decide the next cycle."
}

pub(crate) fn parse_project_review_cycle_report(text: &str) -> Result<ProjectReviewCycleResult> {
    let value = match serde_json::from_str::<ProjectReviewCycleReport>(text) {
        Ok(value) => value,
        Err(_) => {
            let json = extract_json_object(text).ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "invalid project review final JSON: missing json object".to_string(),
                )
            })?;
            serde_json::from_str::<ProjectReviewCycleReport>(json).map_err(|err| {
                RuntimeError::InvalidInput(format!("invalid project review final JSON: {err}"))
            })?
        }
    };
    let summary = value
        .summary
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let pr_summary = value.pr.map(|pr| format!("PR #{pr}"));
    Ok(ProjectReviewCycleResult {
        outcome: value.outcome,
        pr: value.pr,
        summary: summary.or(pr_summary),
        error: value
            .error
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    })
}

pub(crate) fn project_review_github_api_body_with_model_footer(
    method: &str,
    path: &str,
    mut body: Option<Value>,
    role: Option<&AgentRole>,
    model_id: &str,
) -> Option<Value> {
    if !matches!(role, Some(AgentRole::Reviewer))
        || !is_github_pull_request_review_path(method, path)
    {
        return body;
    }
    let Some(review_body) = body
        .as_mut()
        .and_then(Value::as_object_mut)
        .and_then(|object| object.get_mut("body"))
    else {
        return body;
    };
    let Some(text) = review_body.as_str() else {
        return body;
    };
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return body;
    }
    let footer = format!("Powered by {model_id}");
    if text.contains(&footer) {
        return body;
    }
    *review_body = Value::String(format!("{}\n\n{}", text.trim_end(), footer));
    body
}

pub(crate) fn project_review_loop_decision_for_result(
    result: ProjectReviewCycleResult,
) -> ProjectReviewLoopDecision {
    match result.outcome {
        ProjectReviewOutcome::ReviewSubmitted => ProjectReviewLoopDecision {
            delay: Duration::ZERO,
            status: ProjectReviewStatus::Idle,
            outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
            summary: result.summary,
            error: None,
        },
        ProjectReviewOutcome::NoEligiblePr => ProjectReviewLoopDecision {
            delay: Duration::from_secs(PROJECT_REVIEW_IDLE_RETRY_SECS),
            status: ProjectReviewStatus::Waiting,
            outcome: Some(ProjectReviewOutcome::NoEligiblePr),
            summary: result.summary,
            error: None,
        },
        ProjectReviewOutcome::Failed => ProjectReviewLoopDecision {
            delay: Duration::from_secs(PROJECT_REVIEW_FAILURE_RETRY_SECS),
            status: ProjectReviewStatus::Failed,
            outcome: Some(ProjectReviewOutcome::Failed),
            summary: result.summary,
            error: result
                .error
                .or_else(|| Some("reviewer reported failure".to_string())),
        },
    }
}

pub(crate) fn project_review_loop_decision_for_error(error: String) -> ProjectReviewLoopDecision {
    if project_review_error_is_retryable(&error) {
        return ProjectReviewLoopDecision {
            delay: Duration::from_secs(PROJECT_REVIEW_RETRY_INITIAL_SECS),
            status: ProjectReviewStatus::Waiting,
            outcome: None,
            summary: None,
            error: Some(error),
        };
    }
    ProjectReviewLoopDecision {
        delay: Duration::from_secs(PROJECT_REVIEW_FAILURE_RETRY_SECS),
        status: ProjectReviewStatus::Failed,
        outcome: Some(ProjectReviewOutcome::Failed),
        summary: None,
        error: Some(error),
    }
}

pub(crate) fn project_review_error_is_retryable(error: &str) -> bool {
    let error = error.trim();
    let error = error.strip_prefix("invalid input: ").unwrap_or(error);
    matches!(
        error,
        "relay is not connected"
            | "relay is enabled but not connected"
            | "relay connection closed"
            | "relay request timed out"
    )
}

pub(crate) fn project_review_cycle_result_for_reviewer_status(
    summary: &AgentSummary,
) -> Option<ProjectReviewCycleResult> {
    if summary.status == AgentStatus::Completed {
        return None;
    }
    let status_error = format!("reviewer ended with status {:?}", summary.status);
    Some(ProjectReviewCycleResult {
        outcome: ProjectReviewOutcome::Failed,
        pr: None,
        summary: Some("Review could not be completed.".to_string()),
        error: Some(normalize_optional_text(summary.last_error.clone()).unwrap_or(status_error)),
    })
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_github_pull_request_review_path(method: &str, path: &str) -> bool {
    method.eq_ignore_ascii_case("POST") && path.contains("/pulls/") && path.ends_with("/reviews")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use mai_protocol::{AgentId, TokenUsage};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn review_body_gets_model_footer() {
        let body = Some(json!({
            "body": "Looks good after local validation.",
            "comments": [
                {
                    "path": "src/lib.rs",
                    "line": 12,
                    "side": "RIGHT",
                    "body": "Please cover this edge case."
                }
            ]
        }));

        let updated = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            body,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect("body");

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Looks good after local validation.\n\nPowered by gpt-5.4")
        );
        assert_eq!(
            updated.pointer("/comments/0/body").and_then(Value::as_str),
            Some("Please cover this edge case.")
        );
    }

    #[test]
    fn review_body_footer_is_not_duplicated() {
        let body = Some(json!({
            "body": "Looks good.\n\nPowered by gpt-5.4"
        }));

        let updated = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            body,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect("body");

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Looks good.\n\nPowered by gpt-5.4")
        );
    }

    #[test]
    fn review_body_footer_ignores_non_review_paths() {
        let body = Some(json!({
            "body": "Review submitted."
        }));

        let updated = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/issues/42/comments",
            body,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect("body");

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Review submitted.")
        );
    }

    #[test]
    fn model_footer_leaves_get_requests_unchanged() {
        let body = Some(json!({
            "body": "A regular issue comment."
        }));

        let updated = project_review_github_api_body_with_model_footer(
            "GET",
            "/repos/owner/repo/pulls/42/reviews",
            body,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect("body");

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("A regular issue comment.")
        );
    }

    #[test]
    fn model_footer_leaves_non_reviewer_agents_unchanged() {
        let body = Some(json!({
            "body": "Maintainer review body."
        }));

        let updated = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            body,
            Some(&AgentRole::Planner),
            "gpt-5.4",
        )
        .expect("body");

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Maintainer review body.")
        );
    }

    #[test]
    fn model_footer_leaves_missing_or_non_string_body_unchanged() {
        let missing = Some(json!({
            "event": "APPROVE"
        }));
        let updated_missing = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            missing,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect("body");
        assert_eq!(updated_missing, json!({ "event": "APPROVE" }));

        let non_string = Some(json!({
            "body": null
        }));
        let updated_non_string = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            non_string,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect("body");
        assert_eq!(updated_non_string, json!({ "body": null }));
    }

    #[test]
    fn submitted_review_continues_immediately() {
        let decision = project_review_loop_decision_for_result(ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::ReviewSubmitted,
            pr: Some(123),
            summary: Some("submitted".to_string()),
            error: None,
        });

        assert_eq!(decision.delay, Duration::ZERO);
        assert_eq!(decision.status, ProjectReviewStatus::Idle);
        assert_eq!(
            decision.outcome,
            Some(ProjectReviewOutcome::ReviewSubmitted)
        );
        assert_eq!(decision.summary.as_deref(), Some("submitted"));
        assert_eq!(decision.error, None);
    }

    #[test]
    fn no_eligible_pr_waits_two_minutes() {
        let decision = project_review_loop_decision_for_result(ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::NoEligiblePr,
            pr: None,
            summary: Some("nothing to review".to_string()),
            error: None,
        });

        assert_eq!(decision.delay, Duration::from_secs(120));
        assert_eq!(decision.status, ProjectReviewStatus::Waiting);
        assert_eq!(decision.outcome, Some(ProjectReviewOutcome::NoEligiblePr));
        assert_eq!(decision.summary.as_deref(), Some("nothing to review"));
        assert_eq!(decision.error, None);
    }

    #[test]
    fn failure_keeps_backoff() {
        let decision = project_review_loop_decision_for_result(ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::Failed,
            pr: None,
            summary: Some("failed".to_string()),
            error: None,
        });

        assert_eq!(decision.delay, Duration::from_secs(600));
        assert_eq!(decision.status, ProjectReviewStatus::Failed);
        assert_eq!(decision.outcome, Some(ProjectReviewOutcome::Failed));
        assert_eq!(decision.summary.as_deref(), Some("failed"));
        assert_eq!(decision.error.as_deref(), Some("reviewer reported failure"));
    }

    #[test]
    fn relay_disconnect_retries_after_one_second_without_failing_project() {
        let decision =
            project_review_loop_decision_for_error("invalid input: relay is not connected".into());

        assert_eq!(decision.delay, Duration::from_secs(1));
        assert_eq!(decision.status, ProjectReviewStatus::Waiting);
        assert_eq!(decision.outcome, None);
        assert_eq!(decision.summary, None);
        assert_eq!(
            decision.error.as_deref(),
            Some("invalid input: relay is not connected")
        );
    }

    #[test]
    fn relay_request_timeout_is_retryable() {
        let decision =
            project_review_loop_decision_for_error("invalid input: relay request timed out".into());

        assert_eq!(decision.delay, Duration::from_secs(1));
        assert_eq!(decision.status, ProjectReviewStatus::Waiting);
        assert_eq!(decision.outcome, None);
        assert_eq!(decision.summary, None);
        assert_eq!(
            decision.error.as_deref(),
            Some("invalid input: relay request timed out")
        );
    }

    #[test]
    fn parses_cycle_json_from_final_text() {
        let report = parse_project_review_cycle_report(
            r#"Done.
            {"outcome":"no_eligible_pr","pr":null,"summary":"Nothing to review.","error":null}"#,
        )
        .expect("parse report");
        assert_eq!(report.outcome, ProjectReviewOutcome::NoEligiblePr);
        assert_eq!(report.summary.as_deref(), Some("Nothing to review."));
        assert_eq!(report.error, None);
    }

    #[test]
    fn failed_reviewer_status_becomes_failure_result() {
        let mut summary = test_agent_summary(Uuid::new_v4());
        summary.status = AgentStatus::Failed;
        summary.last_error = Some("container command timed out".to_string());

        let result = project_review_cycle_result_for_reviewer_status(&summary)
            .expect("failed reviewer should produce failed review result");

        assert_eq!(result.outcome, ProjectReviewOutcome::Failed);
        assert_eq!(result.pr, None);
        assert_eq!(
            result.summary.as_deref(),
            Some("Review could not be completed.")
        );
        assert_eq!(result.error.as_deref(), Some("container command timed out"));
    }

    #[test]
    fn completed_reviewer_status_does_not_skip_final_json_parsing() {
        let mut summary = test_agent_summary(Uuid::new_v4());
        summary.status = AgentStatus::Completed;

        assert!(project_review_cycle_result_for_reviewer_status(&summary).is_none());
    }

    fn test_agent_summary(agent_id: AgentId) -> AgentSummary {
        let timestamp: DateTime<Utc> = Utc::now();
        AgentSummary {
            id: agent_id,
            parent_id: None,
            task_id: None,
            project_id: None,
            role: None,
            name: "reviewer".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            docker_image: "ubuntu:latest".to_string(),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some("medium".to_string()),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        }
    }
}
