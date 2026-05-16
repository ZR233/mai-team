use std::future::Future;

use chrono::{DateTime, Utc};
use mai_protocol::{ProjectId, ProjectSummary};
use serde::Deserialize;
use serde_json::Value;

use super::selection::{
    CheckSignal, PullRequestCandidate, PullRequestReview, ReviewSelection, select_review_prs,
};
use crate::github::github_path_segment;
use crate::{Result, RuntimeError};

pub(crate) const SELECTOR_PAGE_SIZE: u64 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReviewIdentity {
    pub(crate) login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedProjectReviewPr {
    pub(crate) pr: u64,
    pub(crate) head_sha: Option<String>,
}

/// 提供判断项目 PR 是否满足自动审查条件所需的只读 GitHub 数据。
///
/// 实现方只能读取项目和 GitHub 信息，不能入队审查、创建 agent、提交 GitHub
/// review 或修改仓库。调用方在共享 selector 规则完成后自行决定如何处理合格 PR。
pub(crate) trait ProjectReviewEligibilityOps: Send + Sync {
    fn project_summary(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<ProjectSummary>> + Send;

    fn project_review_identity(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<ProjectReviewIdentity>> + Send;

    fn github_api_get_json(
        &self,
        project_id: ProjectId,
        path: String,
    ) -> impl Future<Output = Result<Value>> + Send;
}

pub(crate) async fn select_project_review_candidates(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
) -> Result<Vec<SelectedProjectReviewPr>> {
    let summary = ops.project_summary(project_id).await?;
    let identity = ops.project_review_identity(project_id).await?;
    let owner = github_path_segment(&summary.owner);
    let repo = github_path_segment(&summary.repo);
    let mut page = 1_u64;
    let mut selected = Vec::new();
    loop {
        let pull_requests = list_open_pull_requests(ops, project_id, &owner, &repo, page).await?;
        tracing::debug!(
            project_id = %project_id,
            page,
            count = pull_requests.len(),
            "project review selector fetched open PR page"
        );
        if pull_requests.is_empty() {
            return Ok(selected);
        }
        let mut pull_requests = pull_requests;
        pull_requests.sort_by_key(|pull_request| pull_request.number);
        for pull_request in pull_requests.iter() {
            let candidate =
                pull_request_candidate(ops, project_id, &owner, &repo, pull_request).await?;
            selected.extend(
                select_review_prs(&identity.login, vec![candidate])
                    .into_iter()
                    .map(selected_project_review_pr),
            );
        }
        if pull_requests.len() < SELECTOR_PAGE_SIZE as usize {
            return Ok(selected);
        }
        page += 1;
    }
}

pub(crate) async fn select_project_review_pr(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
    pr: u64,
    head_sha_hint: Option<String>,
) -> Result<Option<SelectedProjectReviewPr>> {
    if pr == 0 {
        return Ok(None);
    }
    let summary = ops.project_summary(project_id).await?;
    let identity = ops.project_review_identity(project_id).await?;
    let owner = github_path_segment(&summary.owner);
    let repo = github_path_segment(&summary.repo);
    let pull_request = GithubPullRequest {
        number: pr,
        draft: false,
        user: None,
        head: head_sha_hint.map(|sha| GithubPullRequestHead { sha: Some(sha) }),
    };
    let candidate = pull_request_candidate(ops, project_id, &owner, &repo, &pull_request).await?;
    Ok(select_review_prs(&identity.login, vec![candidate])
        .into_iter()
        .map(selected_project_review_pr)
        .next())
}

pub(super) async fn list_open_pull_requests(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
    owner: &str,
    repo: &str,
    page: u64,
) -> Result<Vec<GithubPullRequest>> {
    let path = format!(
        "/repos/{owner}/{repo}/pulls?state=open&sort=created&direction=asc&per_page={SELECTOR_PAGE_SIZE}&page={page}"
    );
    let value = ops.github_api_get_json(project_id, path).await?;
    decode_github_json(value, "list open pull requests")
}

pub(super) async fn pull_request_candidate(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
    owner: &str,
    repo: &str,
    pull_request: &GithubPullRequest,
) -> Result<PullRequestCandidate> {
    let detail = pull_request_detail(ops, project_id, owner, repo, pull_request.number).await?;
    let number = detail.number;
    let head_sha = detail.head_sha().or_else(|| pull_request.head_sha());
    if detail.draft {
        return Ok(PullRequestCandidate {
            number,
            author_login: detail.author_login(),
            draft: true,
            head_sha,
            latest_commit_at: None,
            reviews: Vec::new(),
            check_signals: Vec::new(),
            combined_status_state: None,
        });
    }
    let reviews = pull_request_reviews(ops, project_id, owner, repo, number).await?;
    let latest_commit_at = match head_sha.as_deref() {
        Some(head_sha) => commit_time(ops, project_id, owner, repo, head_sha).await?,
        None => None,
    };
    let checks = match head_sha.as_deref() {
        Some(head_sha) => check_signals(ops, project_id, owner, repo, head_sha).await,
        None => CandidateChecks::default(),
    };
    Ok(PullRequestCandidate {
        number,
        author_login: detail.author_login(),
        draft: false,
        head_sha,
        latest_commit_at,
        reviews,
        check_signals: checks.signals,
        combined_status_state: checks.combined_status_state,
    })
}

async fn pull_request_detail(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<GithubPullRequest> {
    let path = format!("/repos/{owner}/{repo}/pulls/{pr}");
    let value = ops.github_api_get_json(project_id, path).await?;
    decode_github_json(value, "get pull request")
}

async fn pull_request_reviews(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<Vec<PullRequestReview>> {
    let path = format!("/repos/{owner}/{repo}/pulls/{pr}/reviews?per_page=100");
    let value = ops.github_api_get_json(project_id, path).await?;
    let reviews: Vec<GithubPullRequestReview> =
        decode_github_json(value, "list pull request reviews")?;
    Ok(reviews
        .into_iter()
        .map(|review| PullRequestReview {
            author_login: review.user.map(|user| user.login),
            submitted_at: review.submitted_at,
            commit_id: review.commit_id,
        })
        .collect())
}

async fn commit_time(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> Result<Option<DateTime<Utc>>> {
    let path = format!(
        "/repos/{owner}/{repo}/commits/{}",
        github_path_segment(head_sha)
    );
    let value = match ops.github_api_get_json(project_id, path).await {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(project_id = %project_id, "project review eligibility could not read commit time: {err}");
            return Ok(None);
        }
    };
    let commit: GithubCommit = decode_github_json(value, "get commit")?;
    Ok(commit
        .commit
        .and_then(|commit| commit.committer.or(commit.author))
        .and_then(|person| person.date))
}

async fn check_signals(
    ops: &impl ProjectReviewEligibilityOps,
    project_id: ProjectId,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> CandidateChecks {
    let checks_path = format!(
        "/repos/{owner}/{repo}/commits/{}/check-runs?per_page=100",
        github_path_segment(head_sha)
    );
    let check_runs = match ops.github_api_get_json(project_id, checks_path).await {
        Ok(value) => decode_github_json::<GithubCheckRunResponse>(value, "list check runs")
            .map(|response| response.check_runs)
            .unwrap_or_default(),
        Err(err) => {
            tracing::debug!(project_id = %project_id, "project review eligibility could not list check runs: {err}");
            Vec::new()
        }
    };
    let status_path = format!(
        "/repos/{owner}/{repo}/commits/{}/status",
        github_path_segment(head_sha)
    );
    let combined_status = match ops.github_api_get_json(project_id, status_path).await {
        Ok(value) => decode_github_json::<GithubCombinedStatus>(value, "get combined status").ok(),
        Err(err) => {
            tracing::debug!(project_id = %project_id, "project review eligibility could not read combined status: {err}");
            None
        }
    };
    let mut signals = check_runs
        .into_iter()
        .map(|run| CheckSignal {
            status: run.status,
            conclusion: run.conclusion,
        })
        .collect::<Vec<_>>();
    if let Some(status) = combined_status.as_ref() {
        signals.extend(status.statuses.iter().map(|status| CheckSignal {
            status: status.state.clone(),
            conclusion: None,
        }));
    }
    CandidateChecks {
        signals,
        combined_status_state: combined_status.and_then(|status| status.state),
    }
}

pub(super) fn selected_project_review_pr(selection: ReviewSelection) -> SelectedProjectReviewPr {
    SelectedProjectReviewPr {
        pr: selection.pr,
        head_sha: selection.head_sha,
    }
}

fn decode_github_json<T: for<'de> Deserialize<'de>>(value: Value, action: &str) -> Result<T> {
    serde_json::from_value(value).map_err(|err| {
        RuntimeError::InvalidInput(format!("invalid GitHub response for {action}: {err}"))
    })
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct GithubPullRequest {
    pub(super) number: u64,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    user: Option<GithubUser>,
    #[serde(default)]
    head: Option<GithubPullRequestHead>,
}

impl GithubPullRequest {
    fn head_sha(&self) -> Option<String> {
        self.head.as_ref().and_then(|head| head.sha.clone())
    }

    fn author_login(&self) -> Option<String> {
        self.user.as_ref().map(|user| user.login.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GithubPullRequestHead {
    sha: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubPullRequestReview {
    #[serde(default)]
    user: Option<GithubUser>,
    #[serde(default)]
    submitted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    commit_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubUser {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCommit {
    #[serde(default)]
    commit: Option<GithubCommitDetail>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCommitDetail {
    #[serde(default)]
    committer: Option<GithubCommitPerson>,
    #[serde(default)]
    author: Option<GithubCommitPerson>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCommitPerson {
    #[serde(default)]
    date: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
struct CandidateChecks {
    signals: Vec<CheckSignal>,
    combined_status_state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCheckRunResponse {
    #[serde(default)]
    check_runs: Vec<GithubCheckRun>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCheckRun {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCombinedStatus {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    statuses: Vec<GithubStatusContext>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubStatusContext {
    #[serde(default)]
    state: Option<String>,
}

#[cfg(test)]
mod tests {
    use mai_protocol::{
        ProjectCloneStatus, ProjectReviewOutcome, ProjectReviewStatus, ProjectStatus,
        ProjectSummary, now,
    };
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;

    struct FakeEligibilityOps {
        responses: Mutex<Vec<(String, Value)>>,
    }

    impl FakeEligibilityOps {
        fn new(responses: Vec<(String, Value)>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl ProjectReviewEligibilityOps for FakeEligibilityOps {
        async fn project_summary(&self, project_id: Uuid) -> crate::Result<ProjectSummary> {
            Ok(test_project_summary(project_id))
        }

        async fn project_review_identity(
            &self,
            _project_id: Uuid,
        ) -> crate::Result<ProjectReviewIdentity> {
            Ok(ProjectReviewIdentity {
                login: "mai-bot".to_string(),
            })
        }

        async fn github_api_get_json(
            &self,
            _project_id: Uuid,
            path: String,
        ) -> crate::Result<Value> {
            let mut responses = self.responses.lock().await;
            let Some((expected, response)) = responses.first().cloned() else {
                panic!("unexpected GitHub API request: {path}");
            };
            assert_eq!(expected, path);
            responses.remove(0);
            Ok(response)
        }
    }

    #[tokio::test]
    async fn single_pr_eligibility_selects_completed_ci() {
        let project_id = Uuid::new_v4();
        let ops = FakeEligibilityOps::new(vec![
            (
                "/repos/owner/repo/pulls/42".to_string(),
                pr_detail(42, false, "head-42"),
            ),
            (
                "/repos/owner/repo/pulls/42/reviews?per_page=100".to_string(),
                json!([]),
            ),
            (
                "/repos/owner/repo/commits/head%2D42".to_string(),
                commit("2026-01-01T00:00:00Z"),
            ),
            (
                "/repos/owner/repo/commits/head%2D42/check-runs?per_page=100".to_string(),
                json!({"check_runs": [{"status": "completed", "conclusion": "success"}]}),
            ),
            (
                "/repos/owner/repo/commits/head%2D42/status".to_string(),
                json!({"state": "success"}),
            ),
        ]);

        let selected = select_project_review_pr(&ops, project_id, 42, Some("hint-sha".to_string()))
            .await
            .expect("select pr");

        assert_eq!(
            Some(SelectedProjectReviewPr {
                pr: 42,
                head_sha: Some("head-42".to_string()),
            }),
            selected
        );
    }

    #[tokio::test]
    async fn single_pr_eligibility_skips_pending_ci() {
        let project_id = Uuid::new_v4();
        let ops = FakeEligibilityOps::new(vec![
            (
                "/repos/owner/repo/pulls/7".to_string(),
                pr_detail(7, false, "head-7"),
            ),
            (
                "/repos/owner/repo/pulls/7/reviews?per_page=100".to_string(),
                json!([]),
            ),
            (
                "/repos/owner/repo/commits/head%2D7".to_string(),
                commit("2026-01-01T00:00:00Z"),
            ),
            (
                "/repos/owner/repo/commits/head%2D7/check-runs?per_page=100".to_string(),
                json!({"check_runs": [{"status": "in_progress", "conclusion": null}]}),
            ),
            (
                "/repos/owner/repo/commits/head%2D7/status".to_string(),
                json!({"state": "pending"}),
            ),
        ]);

        let selected = select_project_review_pr(&ops, project_id, 7, None)
            .await
            .expect("select pr");

        assert_eq!(None, selected);
    }

    #[tokio::test]
    async fn single_pr_eligibility_allows_rereview_when_matching_review_commit_is_older_than_head()
    {
        let project_id = Uuid::new_v4();
        let ops = FakeEligibilityOps::new(vec![
            (
                "/repos/owner/repo/pulls/616".to_string(),
                pr_detail(616, false, "head-616"),
            ),
            (
                "/repos/owner/repo/pulls/616/reviews?per_page=100".to_string(),
                json!([
                    {
                        "user": { "login": "mai-bot" },
                        "submitted_at": "2026-05-16T07:25:36Z",
                        "commit_id": "head-616"
                    }
                ]),
            ),
            (
                "/repos/owner/repo/commits/head%2D616".to_string(),
                commit("2026-05-16T08:00:39Z"),
            ),
            (
                "/repos/owner/repo/commits/head%2D616/check-runs?per_page=100".to_string(),
                json!({"check_runs": [{"status": "completed", "conclusion": "success"}]}),
            ),
            (
                "/repos/owner/repo/commits/head%2D616/status".to_string(),
                json!({"state": "success"}),
            ),
        ]);

        let selected = select_project_review_pr(&ops, project_id, 616, None)
            .await
            .expect("select pr");

        assert_eq!(
            Some(SelectedProjectReviewPr {
                pr: 616,
                head_sha: Some("head-616".to_string()),
            }),
            selected
        );
    }

    fn test_project_summary(project_id: Uuid) -> ProjectSummary {
        ProjectSummary {
            id: project_id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account-1".to_string()),
            repository_id: 42,
            installation_id: 0,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "unused".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: Uuid::new_v4(),
            created_at: now(),
            updated_at: now(),
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Idle,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: Some(ProjectReviewOutcome::NoEligiblePr),
            review_last_error: None,
        }
    }

    fn pr_detail(number: u64, draft: bool, head_sha: &str) -> Value {
        json!({
            "number": number,
            "draft": draft,
            "head": { "sha": head_sha },
        })
    }

    fn commit(date: &str) -> Value {
        json!({
            "commit": {
                "committer": {
                    "date": date,
                }
            }
        })
    }
}
