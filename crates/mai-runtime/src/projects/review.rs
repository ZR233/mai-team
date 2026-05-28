use mai_protocol::{
    AgentRole, AgentStatus, AgentSummary, ProjectReviewDecision, ProjectReviewOutcome,
    ProjectReviewStatus,
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
    review_event: Option<ProjectReviewDecision>,
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
    pub(crate) review_event: Option<ProjectReviewDecision>,
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
    "You are an autonomous project pull request reviewer. Review exactly one target GitHub pull request for this project, using only the Mai GitHub API tools visible in the current tool list for GitHub reads/writes and local shell commands for git/test work. The repository has already been cloned and synced at /workspace/repo, and this reviewer owns that isolated clone. Do not look for GitHub tokens in the environment or write credentials. Finish with only the required JSON object so the project scheduler can decide the next cycle."
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
    let review_event = match (value.outcome.clone(), value.review_event) {
        (ProjectReviewOutcome::ReviewSubmitted, Some(review_event)) => Some(review_event),
        (ProjectReviewOutcome::ReviewSubmitted, None) => {
            return Err(RuntimeError::InvalidInput(
                "invalid project review final JSON: review_event is required for review_submitted"
                    .to_string(),
            ));
        }
        (ProjectReviewOutcome::NoEligiblePr | ProjectReviewOutcome::Failed, None) => None,
        (ProjectReviewOutcome::NoEligiblePr | ProjectReviewOutcome::Failed, Some(_)) => {
            return Err(RuntimeError::InvalidInput(
                "invalid project review final JSON: review_event must be null unless review_submitted"
                    .to_string(),
            ));
        }
    };
    let summary = value
        .summary
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let pr_summary = value.pr.map(|pr| format!("PR #{pr}"));
    Ok(ProjectReviewCycleResult {
        outcome: value.outcome,
        review_event,
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
) -> Result<Option<Value>> {
    validate_project_reviewer_github_api_request(method, path, body.as_ref(), role)?;
    if !matches!(role, Some(AgentRole::Reviewer))
        || !is_github_pull_request_review_path(method, path)
    {
        return Ok(body);
    }
    let Some(review_body) = body
        .as_mut()
        .and_then(Value::as_object_mut)
        .and_then(|object| object.get_mut("body"))
    else {
        return Ok(body);
    };
    let Some(text) = review_body.as_str() else {
        return Ok(body);
    };
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return Ok(body);
    }
    let footer = format!("Powered by {model_id}");
    if text.contains(&footer) {
        return Ok(body);
    }
    *review_body = Value::String(format!("{}\n\n{}", text.trim_end(), footer));
    Ok(body)
}

pub(crate) fn validate_project_reviewer_github_api_request(
    method: &str,
    path: &str,
    body: Option<&Value>,
    role: Option<&AgentRole>,
) -> Result<()> {
    if !matches!(role, Some(AgentRole::Reviewer)) {
        return Ok(());
    }
    if is_github_pending_review_event_path(method, path) {
        return Err(RuntimeError::InvalidInput(
            "reviewer PR review submission must use one single POST to `/pulls/PR/reviews` with `event` and a non-empty `body`; submitting `/pulls/PR/reviews/REVIEW_ID/events` depends on pending review state"
                .to_string(),
        ));
    }
    if is_github_pull_request_review_comment_path(method, path) {
        return Err(RuntimeError::InvalidInput(
            "reviewer inline comments must be submitted in one single POST to `/pulls/PR/reviews` using the `comments` array; do not POST `/pulls/PR/comments` directly"
                .to_string(),
        ));
    }
    if !is_github_pull_request_review_path(method, path) {
        return Ok(());
    }
    let body = body.and_then(Value::as_object).ok_or_else(|| {
        RuntimeError::InvalidInput(
            "reviewer PR review submission must include `event` and a non-empty `body`; otherwise GitHub creates a pending review"
                .to_string(),
        )
    })?;
    let event = body
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !matches!(event, "REQUEST_CHANGES" | "APPROVE" | "COMMENT") {
        return Err(RuntimeError::InvalidInput(
            "reviewer PR review submission must include `event` as REQUEST_CHANGES, APPROVE, or COMMENT to avoid creating a pending review"
                .to_string(),
        ));
    }
    let review_body = body
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if review_body.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "reviewer PR review submission must include a non-empty `body`".to_string(),
        ));
    }
    Ok(())
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
    if matches!(
        error,
        "relay is not connected"
            | "relay is enabled but not connected"
            | "relay connection closed"
            | "relay request timed out"
    ) {
        return true;
    }

    if !error.starts_with("model error: request to ") {
        return false;
    }

    error.contains(" failed: error decoding response body")
        || error.contains(" returned 429 ")
        || error.contains(" returned 500 ")
        || error.contains(" returned 502 ")
        || error.contains(" returned 503 ")
        || error.contains(" returned 504 ")
        || error.contains("502 Bad Gateway")
        || error.contains("503 Service Unavailable")
        || error.contains("504 Gateway Timeout")
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
        review_event: None,
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

fn github_api_path_without_query(path: &str) -> &str {
    path.split_once('?')
        .map(|(path_without_query, _query)| path_without_query)
        .unwrap_or(path)
}

fn is_github_pull_request_review_path(method: &str, path: &str) -> bool {
    let path = github_api_path_without_query(path);
    method.eq_ignore_ascii_case("POST") && path.contains("/pulls/") && path.ends_with("/reviews")
}

fn is_github_pending_review_event_path(method: &str, path: &str) -> bool {
    let path = github_api_path_without_query(path);
    method.eq_ignore_ascii_case("POST")
        && path.contains("/pulls/")
        && path.contains("/reviews/")
        && path.ends_with("/events")
}

fn is_github_pull_request_review_comment_path(method: &str, path: &str) -> bool {
    if !method.eq_ignore_ascii_case("POST") {
        return false;
    }
    let path = github_api_path_without_query(path);
    let Some((_prefix, suffix)) = path.split_once("/pulls/") else {
        return false;
    };
    let mut segments = suffix.split('/');
    let Some(pr_number) = segments.next() else {
        return false;
    };
    let Some("comments") = segments.next() else {
        return false;
    };
    pr_number.parse::<u64>().is_ok() && segments.next().is_none()
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
            "event": "COMMENT",
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
        .expect("valid reviewer review")
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
            "event": "COMMENT",
            "body": "Looks good.\n\nPowered by gpt-5.4"
        }));

        let updated = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            body,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect("valid reviewer review")
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
        .expect("valid issue comment")
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
        .expect("valid get request")
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
        .expect("valid non-reviewer request")
        .expect("body");

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Maintainer review body.")
        );
    }

    #[test]
    fn model_footer_rejects_reviewer_reviews_that_could_create_pending_review() {
        let missing = Some(json!({
            "event": "APPROVE"
        }));
        let missing_err = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            missing,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect_err("missing body should be rejected");
        assert!(missing_err.to_string().contains("non-empty `body`"));

        let non_string = Some(json!({
            "event": "APPROVE",
            "body": null
        }));
        let non_string_err = project_review_github_api_body_with_model_footer(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            non_string,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        )
        .expect_err("non-string body should be rejected");
        assert!(non_string_err.to_string().contains("non-empty `body`"));
    }

    #[test]
    fn reviewer_pr_review_submission_requires_event_to_avoid_pending_review() {
        let err = validate_project_reviewer_github_api_request(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            Some(&json!({
                "body": "Looks good after validation.",
            })),
            Some(&AgentRole::Reviewer),
        )
        .expect_err("missing event should be rejected");

        assert!(err.to_string().contains("include `event`"));
        assert!(err.to_string().contains("pending review"));
    }

    #[test]
    fn reviewer_pr_review_submission_requires_body_to_avoid_empty_review() {
        let err = validate_project_reviewer_github_api_request(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            Some(&json!({
                "event": "APPROVE",
            })),
            Some(&AgentRole::Reviewer),
        )
        .expect_err("missing review body should be rejected");

        assert!(err.to_string().contains("non-empty `body`"));
    }

    #[test]
    fn reviewer_pr_review_submission_rejects_pending_review_event_path() {
        let err = validate_project_reviewer_github_api_request(
            "POST",
            "/repos/owner/repo/pulls/42/reviews/123/events",
            Some(&json!({
                "event": "APPROVE",
                "body": "Looks good after validation.",
            })),
            Some(&AgentRole::Reviewer),
        )
        .expect_err("pending review event path should be rejected");

        assert!(err.to_string().contains("one single POST"));
        assert!(err.to_string().contains("pending review state"));
    }

    #[test]
    fn reviewer_pr_review_submission_rejects_direct_inline_comment_path() {
        let err = validate_project_reviewer_github_api_request(
            "POST",
            "/repos/owner/repo/pulls/42/comments",
            Some(&json!({
                "path": "src/lib.rs",
                "line": 12,
                "side": "RIGHT",
                "body": "Please cover this edge case.",
            })),
            Some(&AgentRole::Reviewer),
        )
        .expect_err("direct inline review comment creation should be rejected");

        assert!(err.to_string().contains("one single POST"));
        assert!(err.to_string().contains("comments` array"));
    }

    #[test]
    fn reviewer_pr_review_submission_ignores_other_event_paths() {
        validate_project_reviewer_github_api_request(
            "POST",
            "/repos/owner/repo/issues/42/events",
            None,
            Some(&AgentRole::Reviewer),
        )
        .expect("non-review event path is outside reviewer review submission guard");
    }

    #[test]
    fn reviewer_pr_review_submission_accepts_single_complete_request() {
        validate_project_reviewer_github_api_request(
            "POST",
            "/repos/owner/repo/pulls/42/reviews",
            Some(&json!({
                "event": "APPROVE",
                "body": "Looks good after validation.",
            })),
            Some(&AgentRole::Reviewer),
        )
        .expect("complete reviewer review submission is accepted");
    }

    #[test]
    fn submitted_review_continues_immediately() {
        let decision = project_review_loop_decision_for_result(ProjectReviewCycleResult {
            outcome: ProjectReviewOutcome::ReviewSubmitted,
            review_event: Some(ProjectReviewDecision::Approve),
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
            review_event: None,
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
            review_event: None,
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
    fn model_gateway_failure_is_retryable() {
        let error = "model error: request to https://token-plan-cn.xiaomimimo.com/v1/chat/completions returned 500 Internal Server Error: {\"error\":{\"message\":\"<html><body><h1>502 Bad Gateway</h1></body></html>\"}}";

        let decision = project_review_loop_decision_for_error(error.to_string());

        assert_eq!(decision.delay, Duration::from_secs(1));
        assert_eq!(decision.status, ProjectReviewStatus::Waiting);
        assert_eq!(decision.outcome, None);
        assert_eq!(decision.summary, None);
        assert_eq!(decision.error.as_deref(), Some(error));
    }

    #[test]
    fn model_response_decode_failure_is_retryable() {
        let error = "model error: request to https://token-plan-cn.xiaomimimo.com/v1/chat/completions failed: error decoding response body";

        let decision = project_review_loop_decision_for_error(error.to_string());

        assert_eq!(decision.delay, Duration::from_secs(1));
        assert_eq!(decision.status, ProjectReviewStatus::Waiting);
        assert_eq!(decision.outcome, None);
        assert_eq!(decision.summary, None);
        assert_eq!(decision.error.as_deref(), Some(error));
    }

    #[test]
    fn model_bad_request_is_not_retryable() {
        let error = "model error: request to https://example.test/v1/chat/completions returned 400 Bad Request: invalid request";

        let decision = project_review_loop_decision_for_error(error.to_string());

        assert_eq!(decision.delay, Duration::from_secs(600));
        assert_eq!(decision.status, ProjectReviewStatus::Failed);
        assert_eq!(decision.outcome, Some(ProjectReviewOutcome::Failed));
        assert_eq!(decision.summary, None);
        assert_eq!(decision.error.as_deref(), Some(error));
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
    fn parses_review_submitted_events() {
        let cases = [
            ("approve", ProjectReviewDecision::Approve),
            ("request_changes", ProjectReviewDecision::RequestChanges),
            ("comment", ProjectReviewDecision::Comment),
        ];
        for (raw_event, expected_event) in cases {
            let report = parse_project_review_cycle_report(&format!(
                r#"{{"outcome":"review_submitted","review_event":"{raw_event}","pr":42,"summary":"Submitted review.","error":null}}"#
            ))
            .expect("parse submitted review");

            assert_eq!(report.outcome, ProjectReviewOutcome::ReviewSubmitted);
            assert_eq!(report.review_event, Some(expected_event));
            assert_eq!(report.pr, Some(42));
        }
    }

    #[test]
    fn review_submitted_requires_review_event() {
        let err = parse_project_review_cycle_report(
            r#"{"outcome":"review_submitted","pr":42,"summary":"Submitted review.","error":null}"#,
        )
        .expect_err("submitted review without event should be rejected");

        assert!(err.to_string().contains("review_event"));
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
