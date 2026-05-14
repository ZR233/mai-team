use mai_protocol::{
    AgentId, AgentRole, AgentStatus, AgentSummary, ProjectReviewOutcome, ProjectReviewStatus,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::Duration;

use crate::{Result, RuntimeError};

pub(crate) mod cleanup;
pub(crate) mod reviewer;
pub(crate) mod runs;
pub(crate) mod state;
pub(crate) mod workspace;

const PROJECT_REVIEW_IDLE_RETRY_SECS: u64 = 120;
const PROJECT_REVIEW_FAILURE_RETRY_SECS: u64 = 600;
const PROJECT_REVIEW_GIT_LOW_SPEED_LIMIT: u64 = 1;
const PROJECT_REVIEW_GIT_LOW_SPEED_TIME_SECS: u64 = 300;
const PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL: &str =
    "mcp__github__pull_request_review_write";
const PROJECT_GITHUB_CREATE_PULL_REQUEST_REVIEW_TOOL: &str =
    "mcp__github__create_pull_request_review";

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

#[derive(Debug, Clone, Copy)]
pub(crate) enum ReviewRepoAction {
    Ensure,
    Sync,
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewRepoCommandSpec {
    pub(crate) label: &'static str,
    pub(crate) command: String,
    pub(crate) fallback_command: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewWorkspaceCommandSpec {
    pub(crate) sidecar_name: String,
    pub(crate) command: String,
    pub(crate) cwd: &'static str,
    pub(crate) timeout_secs: u64,
}

pub(crate) fn review_repo_command_spec(
    action: ReviewRepoAction,
    repo_url: &str,
    expected_remote: &str,
    branch: &str,
) -> ReviewRepoCommandSpec {
    match action {
        ReviewRepoAction::Ensure => ReviewRepoCommandSpec {
            label: "ensure",
            command: review_repo_ensure_command(repo_url, expected_remote, branch),
            fallback_command: None,
        },
        ReviewRepoAction::Sync => ReviewRepoCommandSpec {
            label: "sync",
            command: review_repo_sync_command(repo_url, expected_remote, branch),
            fallback_command: Some(review_repo_reclone_command(
                repo_url,
                expected_remote,
                branch,
            )),
        },
    }
}

pub(crate) fn reviewer_worktree_cleanup_spec(reviewer_id: AgentId) -> ReviewWorkspaceCommandSpec {
    ReviewWorkspaceCommandSpec {
        sidecar_name: format!("mai-review-cleanup-{reviewer_id}"),
        command: format!(
            "set -eu\n\
             git -C /workspace/repo worktree prune 2>/dev/null || true\n\
             rm -rf {}",
            shell_quote(&format!("/workspace/reviews/{reviewer_id}"))
        ),
        cwd: "/workspace",
        timeout_secs: 120,
    }
}

pub(crate) fn retention_cleanup_spec(
    project_id: mai_protocol::ProjectId,
    active_reviewer: Option<AgentId>,
    cutoff_epoch: i64,
) -> ReviewWorkspaceCommandSpec {
    let active_path = active_reviewer
        .map(|id| format!("/workspace/reviews/{id}"))
        .unwrap_or_default();
    ReviewWorkspaceCommandSpec {
        sidecar_name: format!("mai-review-retention-{project_id}"),
        command: format!(
            "set -eu\n\
             git -C /workspace/repo worktree prune 2>/dev/null || true\n\
             mkdir -p /workspace/reviews\n\
             find /workspace/reviews -mindepth 1 -maxdepth 1 {active_filter} -type d ! -newermt @{cutoff_epoch} -exec rm -rf -- {{}} +\n\
             find /workspace/reviews -type f \\( -name '*.log' -o -name '*.tmp' -o -name '*.temp' -o -name 'tmp.*' \\) ! -newermt @{cutoff_epoch} -delete\n",
            active_filter = if active_path.is_empty() {
                String::new()
            } else {
                format!("! -path {}", shell_quote(&active_path))
            },
            cutoff_epoch = cutoff_epoch,
        ),
        cwd: "/workspace",
        timeout_secs: 120,
    }
}

pub(crate) fn review_repo_auth_prelude() -> &'static str {
    "tmp=$(mktemp -d)\n\
     askpass=\"$tmp/askpass.sh\"\n\
     cleanup() { rm -rf \"$tmp\"; }\n\
     trap cleanup EXIT HUP INT TERM\n\
     cat >\"$askpass\" <<'EOF'\n\
#!/bin/sh\n\
case \"$1\" in\n\
  *Username*) printf '%s\\n' x-access-token ;;\n\
  *Password*) printf '%s\\n' \"$MAI_GITHUB_REVIEW_TOKEN\" ;;\n\
  *) printf '\\n' ;;\n\
esac\n\
EOF\n\
     chmod 700 \"$askpass\"\n\
     export GIT_TERMINAL_PROMPT=0\n\
     export GIT_ASKPASS=\"$askpass\"\n\
     git config --global --add safe.directory /workspace/repo 2>/dev/null || true"
}

pub(crate) fn review_repo_ensure_command(
    repo_url: &str,
    expected_remote: &str,
    branch: &str,
) -> String {
    let branch_arg = if branch.trim().is_empty() {
        String::new()
    } else {
        format!(" --branch {}", shell_quote(branch))
    };
    format!(
        "set -eu\n\
         {prelude}\n\
         mkdir -p /workspace\n\
         if [ -d /workspace/repo/.git ]; then\n\
           remote=$(git -C /workspace/repo remote get-url origin 2>/dev/null || true)\n\
           if [ \"$remote\" != {expected_remote} ]; then\n\
             rm -rf /workspace/repo\n\
           fi\n\
         fi\n\
         if [ ! -d /workspace/repo/.git ]; then\n\
           rm -rf /workspace/repo\n\
           {git_network} clone{branch_arg} -- {repo_url} /workspace/repo\n\
         fi",
        prelude = review_repo_auth_prelude(),
        expected_remote = shell_quote(expected_remote),
        git_network = review_repo_git_network_command_prefix(),
        repo_url = shell_quote(repo_url),
    )
}

pub(crate) fn review_repo_sync_command(
    repo_url: &str,
    expected_remote: &str,
    branch: &str,
) -> String {
    let branch_refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    let origin_branch = format!("origin/{branch}");
    format!(
        "set -eu\n\
         {ensure}\n\
         cd /workspace/repo\n\
         git -c credential.helper= remote set-url origin {repo_url}\n\
         git worktree prune\n\
         git reset --hard HEAD\n\
         git clean -fdx\n\
         {git_network} fetch --prune --no-tags origin {branch_refspec}\n\
         {git_network} fetch --prune --no-tags origin '+refs/pull/*/head:refs/remotes/origin/pr/*'\n\
         git checkout -B {branch} {origin_branch}\n\
         git reset --hard {origin_branch}\n\
         git clean -fdx\n\
         git worktree prune\n\
         mkdir -p /workspace/reviews",
        ensure = review_repo_ensure_command(repo_url, expected_remote, branch),
        git_network = review_repo_git_network_command_prefix(),
        repo_url = shell_quote(repo_url),
        branch_refspec = shell_quote(&branch_refspec),
        branch = shell_quote(branch),
        origin_branch = shell_quote(&origin_branch),
    )
}

pub(crate) fn review_repo_reclone_command(
    repo_url: &str,
    expected_remote: &str,
    branch: &str,
) -> String {
    format!(
        "set -eu\n\
         rm -rf /workspace/repo\n\
         {ensure}\n\
         cd /workspace/repo\n\
         {git_network} fetch --prune --no-tags origin '+refs/pull/*/head:refs/remotes/origin/pr/*'\n\
         mkdir -p /workspace/reviews",
        ensure = review_repo_ensure_command(repo_url, expected_remote, branch),
        git_network = review_repo_git_network_command_prefix(),
    )
}

pub(crate) fn review_repo_git_network_command_prefix() -> String {
    format!(
        "git -c credential.helper= -c http.version=HTTP/1.1 -c http.lowSpeedLimit={} -c http.lowSpeedTime={}",
        PROJECT_REVIEW_GIT_LOW_SPEED_LIMIT, PROJECT_REVIEW_GIT_LOW_SPEED_TIME_SECS
    )
}

pub(crate) fn project_reviewer_system_prompt() -> &'static str {
    "You are an autonomous project pull request reviewer. Review exactly one eligible GitHub pull request for this project, using only the GitHub MCP tools visible in the current tool list for GitHub reads/writes and local shell commands for git/test work. The repository has already been cloned and synced at /workspace/repo. Use an isolated git worktree under /workspace/reviews for the selected PR and clean it up before finishing. Do not look for GitHub tokens in the environment or write credentials. Finish with only the required JSON object so the project scheduler can decide the next cycle."
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

pub(crate) fn project_review_mcp_arguments_with_model_footer(
    model_tool_name: &str,
    mut arguments: Value,
    role: Option<&AgentRole>,
    model_id: &str,
) -> Value {
    if !matches!(role, Some(AgentRole::Reviewer))
        || !is_github_pull_request_review_write_tool(model_tool_name)
    {
        return arguments;
    }
    let Some(body) = arguments.get("body").and_then(Value::as_str) else {
        return arguments;
    };
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return arguments;
    }
    let footer = format!("Powered by {model_id}");
    if body.contains(&footer) {
        return arguments;
    }
    let body = format!("{}\n\n{}", body.trim_end(), footer);
    if let Some(value) = arguments.get_mut("body") {
        *value = Value::String(body);
    }
    arguments
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
    ProjectReviewLoopDecision {
        delay: Duration::from_secs(PROJECT_REVIEW_FAILURE_RETRY_SECS),
        status: ProjectReviewStatus::Failed,
        outcome: Some(ProjectReviewOutcome::Failed),
        summary: None,
        error: Some(error),
    }
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

fn is_github_pull_request_review_write_tool(model_tool_name: &str) -> bool {
    model_tool_name == PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL
        || model_tool_name == PROJECT_GITHUB_CREATE_PULL_REQUEST_REVIEW_TOOL
}

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use mai_protocol::TokenUsage;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn review_body_gets_model_footer() {
        let arguments = json!({
            "body": "Looks good after local validation.",
            "comments": [
                {
                    "path": "src/lib.rs",
                    "line": 12,
                    "side": "RIGHT",
                    "body": "Please cover this edge case."
                }
            ]
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

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
        let arguments = json!({
            "body": "Looks good.\n\nPowered by gpt-5.4"
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Looks good.\n\nPowered by gpt-5.4")
        );
    }

    #[test]
    fn review_body_footer_supports_legacy_tool_name() {
        let arguments = json!({
            "body": "Review submitted."
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_CREATE_PULL_REQUEST_REVIEW_TOOL,
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Review submitted.\n\nPowered by gpt-5.4")
        );
    }

    #[test]
    fn model_footer_leaves_non_review_tools_unchanged() {
        let arguments = json!({
            "body": "A regular issue comment."
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            "mcp__github__add_issue_comment",
            arguments,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("A regular issue comment.")
        );
    }

    #[test]
    fn model_footer_leaves_non_reviewer_agents_unchanged() {
        let arguments = json!({
            "body": "Maintainer review body."
        });

        let updated = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            arguments,
            Some(&AgentRole::Planner),
            "gpt-5.4",
        );

        assert_eq!(
            updated.get("body").and_then(Value::as_str),
            Some("Maintainer review body.")
        );
    }

    #[test]
    fn model_footer_leaves_missing_or_non_string_body_unchanged() {
        let missing = json!({
            "event": "APPROVE"
        });
        let updated_missing = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            missing,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );
        assert_eq!(updated_missing, json!({ "event": "APPROVE" }));

        let non_string = json!({
            "body": null
        });
        let updated_non_string = project_review_mcp_arguments_with_model_footer(
            PROJECT_GITHUB_PULL_REQUEST_REVIEW_WRITE_TOOL,
            non_string,
            Some(&AgentRole::Reviewer),
            "gpt-5.4",
        );
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

    #[test]
    fn sync_repo_command_spec_includes_reclone_fallback() {
        let spec = review_repo_command_spec(
            ReviewRepoAction::Sync,
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo.git",
            "main",
        );

        assert_eq!(spec.label, "sync");
        assert!(spec.command.contains("git worktree prune"));
        assert!(
            spec.command
                .contains("'+refs/pull/*/head:refs/remotes/origin/pr/*'")
        );
        assert!(
            spec.fallback_command
                .expect("fallback")
                .contains("rm -rf /workspace/repo")
        );
    }

    #[test]
    fn retention_cleanup_spec_preserves_active_reviewer() {
        let project_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let spec = retention_cleanup_spec(project_id, Some(reviewer_id), 1_700_000_000);

        assert_eq!(
            spec.sidecar_name,
            format!("mai-review-retention-{project_id}")
        );
        assert_eq!(spec.cwd, "/workspace");
        assert_eq!(spec.timeout_secs, 120);
        assert!(
            spec.command
                .contains("git -C /workspace/repo worktree prune")
        );
        assert!(spec.command.contains("mkdir -p /workspace/reviews"));
        assert!(
            spec.command
                .contains(&format!("! -path /workspace/reviews/{reviewer_id}"))
        );
        assert!(spec.command.contains("! -newermt @1700000000"));
    }

    #[test]
    fn reviewer_worktree_cleanup_spec_removes_only_reviewer_path() {
        let reviewer_id = Uuid::new_v4();
        let spec = reviewer_worktree_cleanup_spec(reviewer_id);

        assert_eq!(
            spec.sidecar_name,
            format!("mai-review-cleanup-{reviewer_id}")
        );
        assert_eq!(spec.cwd, "/workspace");
        assert_eq!(spec.timeout_secs, 120);
        assert!(
            spec.command
                .contains("git -C /workspace/repo worktree prune")
        );
        assert!(
            spec.command
                .contains(&format!("rm -rf /workspace/reviews/{reviewer_id}"))
        );
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
