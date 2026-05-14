use mai_protocol::{AgentStatus, AgentSummary, ProjectReviewOutcome, ProjectReviewStatus};
use tokio::time::Duration;

use crate::{ProjectReviewCycleResult, ProjectReviewLoopDecision};

const PROJECT_REVIEW_IDLE_RETRY_SECS: u64 = 120;
const PROJECT_REVIEW_FAILURE_RETRY_SECS: u64 = 600;
const PROJECT_REVIEW_GIT_LOW_SPEED_LIMIT: u64 = 1;
const PROJECT_REVIEW_GIT_LOW_SPEED_TIME_SECS: u64 = 300;

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

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}
