use std::future::Future;

use chrono::{DateTime, Utc};
use mai_protocol::{ProjectId, ProjectSummary};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::selection::{
    CheckSignal, PullRequestCandidate, PullRequestReview, ReviewSelection, select_review_pr,
};
use crate::github::github_path_segment;
use crate::{ProjectReviewQueueRequest, ProjectReviewQueueSummary, Result, RuntimeError};

const SELECTOR_PAGE_SIZE: u64 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReviewIdentity {
    pub(crate) login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedProjectReviewPr {
    pub(crate) pr: u64,
    pub(crate) head_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectReviewSelectorRunResult {
    Queued(SelectedProjectReviewPr),
    NoEligiblePr,
}

/// Provides GitHub read and review queue operations for deterministic PR selection.
///
/// Implementations must perform read-only GitHub calls for selector data gathering
/// and must only enqueue through `enqueue_project_review`; selection itself must
/// not create agents, submit reviews, or mutate repositories.
pub(crate) trait ProjectReviewSelectorOps: Send + Sync {
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

    fn enqueue_project_review(
        &self,
        request: ProjectReviewQueueRequest,
    ) -> impl Future<Output = Result<ProjectReviewQueueSummary>> + Send;
}

pub(crate) async fn run_project_review_selector(
    ops: &impl ProjectReviewSelectorOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) -> Result<ProjectReviewSelectorRunResult> {
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    tracing::info!(project_id = %project_id, "project review selector started");
    let Some(selection) = select_project_review_candidate(ops, project_id).await? else {
        tracing::info!(project_id = %project_id, "project review selector found no eligible PR");
        return Ok(ProjectReviewSelectorRunResult::NoEligiblePr);
    };
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    tracing::info!(
        project_id = %project_id,
        pr = selection.pr,
        head_sha = selection.head_sha.as_deref().unwrap_or(""),
        "project review selector queued PR"
    );
    ops.enqueue_project_review(ProjectReviewQueueRequest {
        project_id,
        pr: selection.pr,
        head_sha: selection.head_sha.clone(),
        delivery_id: None,
        reason: "selector".to_string(),
    })
    .await?;
    Ok(ProjectReviewSelectorRunResult::Queued(selection))
}

pub(crate) async fn select_project_review_candidate(
    ops: &impl ProjectReviewSelectorOps,
    project_id: ProjectId,
) -> Result<Option<SelectedProjectReviewPr>> {
    let summary = ops.project_summary(project_id).await?;
    let identity = ops.project_review_identity(project_id).await?;
    let owner = github_path_segment(&summary.owner);
    let repo = github_path_segment(&summary.repo);
    let mut page = 1_u64;
    loop {
        let pull_requests = list_open_pull_requests(ops, project_id, &owner, &repo, page).await?;
        tracing::debug!(
            project_id = %project_id,
            page,
            count = pull_requests.len(),
            "project review selector fetched open PR page"
        );
        if pull_requests.is_empty() {
            return Ok(None);
        }
        let mut pull_requests = pull_requests;
        pull_requests.sort_by_key(|pull_request| pull_request.number);
        for pull_request in pull_requests.iter() {
            let candidate =
                pull_request_candidate(ops, project_id, &owner, &repo, pull_request).await?;
            if let Some(selection) = select_review_pr(&identity.login, vec![candidate]) {
                return Ok(Some(selected_project_review_pr(selection)));
            }
        }
        if pull_requests.len() < SELECTOR_PAGE_SIZE as usize {
            return Ok(None);
        }
        page += 1;
    }
}

async fn list_open_pull_requests(
    ops: &impl ProjectReviewSelectorOps,
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

async fn pull_request_candidate(
    ops: &impl ProjectReviewSelectorOps,
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
    ops: &impl ProjectReviewSelectorOps,
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
    ops: &impl ProjectReviewSelectorOps,
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
    ops: &impl ProjectReviewSelectorOps,
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
        Err(_) => return Ok(None),
    };
    let commit: GithubCommit = decode_github_json(value, "get commit")?;
    Ok(commit
        .commit
        .and_then(|commit| commit.committer.or(commit.author))
        .and_then(|person| person.date))
}

async fn check_signals(
    ops: &impl ProjectReviewSelectorOps,
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
        Err(_) => Vec::new(),
    };
    let status_path = format!(
        "/repos/{owner}/{repo}/commits/{}/status",
        github_path_segment(head_sha)
    );
    let combined_status = match ops.github_api_get_json(project_id, status_path).await {
        Ok(value) => decode_github_json::<GithubCombinedStatus>(value, "get combined status").ok(),
        Err(_) => None,
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

fn selected_project_review_pr(selection: ReviewSelection) -> SelectedProjectReviewPr {
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
struct GithubPullRequest {
    number: u64,
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
    use std::{env, sync::Arc};

    use mai_protocol::{
        ProjectCloneStatus, ProjectReviewOutcome, ProjectReviewStatus, ProjectStatus, now,
    };
    use pretty_assertions::assert_eq;
    use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
    use serde_json::{Value, json};
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use crate::{ProjectReviewQueueRequest, ProjectReviewQueueSummary};

    use super::{ProjectReviewIdentity, ProjectReviewSelectorOps, select_project_review_candidate};

    #[derive(Default)]
    struct FakeSelectorOps {
        responses: Mutex<Vec<(String, Value)>>,
        requested_paths: Mutex<Vec<String>>,
    }

    impl FakeSelectorOps {
        fn new(responses: Vec<(String, Value)>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requested_paths: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProjectReviewSelectorOps for FakeSelectorOps {
        async fn project_summary(
            &self,
            project_id: mai_protocol::ProjectId,
        ) -> crate::Result<mai_protocol::ProjectSummary> {
            Ok(test_project_summary(project_id))
        }

        async fn project_review_identity(
            &self,
            _project_id: mai_protocol::ProjectId,
        ) -> crate::Result<ProjectReviewIdentity> {
            Ok(ProjectReviewIdentity {
                login: "mai-bot".to_string(),
            })
        }

        async fn github_api_get_json(
            &self,
            _project_id: mai_protocol::ProjectId,
            path: String,
        ) -> crate::Result<Value> {
            self.requested_paths.lock().await.push(path.clone());
            let mut responses = self.responses.lock().await;
            let Some((expected, response)) = responses.first().cloned() else {
                panic!("unexpected GitHub API request: {path}");
            };
            assert_eq!(expected, path);
            responses.remove(0);
            Ok(response)
        }

        async fn enqueue_project_review(
            &self,
            _request: ProjectReviewQueueRequest,
        ) -> crate::Result<ProjectReviewQueueSummary> {
            panic!("read-only candidate selection must not enqueue");
        }
    }

    struct LiveSelectorOps {
        client: reqwest::Client,
        token: Option<String>,
        requested_paths: Mutex<Vec<String>>,
    }

    impl LiveSelectorOps {
        fn new() -> Self {
            Self {
                client: reqwest::Client::new(),
                token: env::var("MAI_LIVE_GITHUB_TOKEN")
                    .or_else(|_| env::var("GITHUB_TOKEN"))
                    .or_else(|_| env::var("GH_TOKEN"))
                    .ok(),
                requested_paths: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProjectReviewSelectorOps for LiveSelectorOps {
        async fn project_summary(
            &self,
            project_id: mai_protocol::ProjectId,
        ) -> crate::Result<mai_protocol::ProjectSummary> {
            let mut summary = test_project_summary(project_id);
            summary.owner = "rcore-os".to_string();
            summary.repo = "tgoskits".to_string();
            summary.repository_full_name = "rcore-os/tgoskits".to_string();
            summary.name = "rcore-os/tgoskits".to_string();
            Ok(summary)
        }

        async fn project_review_identity(
            &self,
            _project_id: mai_protocol::ProjectId,
        ) -> crate::Result<ProjectReviewIdentity> {
            Ok(ProjectReviewIdentity {
                login: "mai-live-selector".to_string(),
            })
        }

        async fn github_api_get_json(
            &self,
            _project_id: mai_protocol::ProjectId,
            path: String,
        ) -> crate::Result<Value> {
            self.requested_paths.lock().await.push(path.clone());
            let url = format!("https://api.github.com{path}");
            let mut headers = HeaderMap::new();
            headers.insert(
                USER_AGENT,
                HeaderValue::from_static("mai-runtime-live-selector-test"),
            );
            headers.insert(
                ACCEPT,
                HeaderValue::from_static("application/vnd.github+json"),
            );
            headers.insert(
                "X-GitHub-Api-Version",
                HeaderValue::from_static("2022-11-28"),
            );
            let mut request = self.client.get(url).headers(headers);
            if let Some(token) = self.token.as_deref() {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(|err| {
                crate::RuntimeError::InvalidInput(format!("live GitHub request failed: {err}"))
            })?;
            let status = response.status();
            let body = response.text().await.map_err(|err| {
                crate::RuntimeError::InvalidInput(format!(
                    "live GitHub response read failed: {err}"
                ))
            })?;
            if !status.is_success() {
                return Err(crate::RuntimeError::InvalidInput(format!(
                    "live GitHub request {path} failed with {status}: {body}"
                )));
            }
            serde_json::from_str(&body).map_err(|err| {
                crate::RuntimeError::InvalidInput(format!(
                    "live GitHub response for {path} was not JSON: {err}"
                ))
            })
        }

        async fn enqueue_project_review(
            &self,
            _request: ProjectReviewQueueRequest,
        ) -> crate::Result<ProjectReviewQueueSummary> {
            panic!("live read-only selector verification must not enqueue");
        }
    }

    #[tokio::test]
    async fn candidate_selection_pages_until_first_eligible_pr() {
        let project_id = Uuid::new_v4();
        let mut page_one = vec![pr_list_item(1, false, "head-1")];
        page_one
            .extend((2..=20).map(|number| pr_list_item(number, true, &format!("head-{number}"))));
        let mut responses = vec![
            (
                "/repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page=1"
                    .to_string(),
                Value::Array(page_one),
            ),
            (
                "/repos/owner/repo/pulls/1".to_string(),
                pr_detail(1, false, "head-1"),
            ),
            (
                "/repos/owner/repo/pulls/1/reviews?per_page=100".to_string(),
                json!([]),
            ),
            (
                "/repos/owner/repo/commits/head%2D1".to_string(),
                commit("2026-01-01T00:00:00Z"),
            ),
            (
                "/repos/owner/repo/commits/head%2D1/check-runs?per_page=100".to_string(),
                json!({"check_runs": [{"status": "in_progress", "conclusion": null}]}),
            ),
            (
                "/repos/owner/repo/commits/head%2D1/status".to_string(),
                json!({"state": "success"}),
            ),
        ];
        responses.extend((2..=20).map(|number| {
            (
                format!("/repos/owner/repo/pulls/{number}"),
                pr_detail(number, true, &format!("head-{number}")),
            )
        }));
        responses.extend([
            (
                "/repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page=2"
                    .to_string(),
                json!([pr_list_item(21, false, "head-21")]),
            ),
            (
                "/repos/owner/repo/pulls/21".to_string(),
                pr_detail(21, false, "head-21"),
            ),
            (
                "/repos/owner/repo/pulls/21/reviews?per_page=100".to_string(),
                json!([]),
            ),
            (
                "/repos/owner/repo/commits/head%2D21".to_string(),
                commit("2026-01-03T00:00:00Z"),
            ),
            (
                "/repos/owner/repo/commits/head%2D21/check-runs?per_page=100".to_string(),
                json!({"check_runs": [{"status": "completed", "conclusion": "failure"}]}),
            ),
            (
                "/repos/owner/repo/commits/head%2D21/status".to_string(),
                json!({"state": "failure"}),
            ),
        ]);
        let ops = Arc::new(FakeSelectorOps::new(responses));

        let selected = select_project_review_candidate(ops.as_ref(), project_id)
            .await
            .expect("select candidate");

        assert_eq!(Some(21), selected.map(|selection| selection.pr));
        let requested_paths = ops.requested_paths.lock().await;
        assert!(requested_paths.iter().any(|path| path
            == "/repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page=2"));
        assert!(!requested_paths.iter().any(|path| path
            == "/repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page=3"));
    }

    #[tokio::test]
    async fn candidate_selection_stops_after_first_eligible_pr() {
        let project_id = Uuid::new_v4();
        let ops = Arc::new(FakeSelectorOps::new(vec![
            (
                "/repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page=1"
                    .to_string(),
                json!([
                    pr_list_item(4, false, "head-4"),
                    pr_list_item(5, false, "head-5")
                ]),
            ),
            (
                "/repos/owner/repo/pulls/4".to_string(),
                pr_detail(4, false, "head-4"),
            ),
            (
                "/repos/owner/repo/pulls/4/reviews?per_page=100".to_string(),
                json!([]),
            ),
            (
                "/repos/owner/repo/commits/head%2D4".to_string(),
                commit("2026-01-04T00:00:00Z"),
            ),
            (
                "/repos/owner/repo/commits/head%2D4/check-runs?per_page=100".to_string(),
                json!({"check_runs": []}),
            ),
            (
                "/repos/owner/repo/commits/head%2D4/status".to_string(),
                json!({"state": "failure"}),
            ),
        ]));

        let selected = select_project_review_candidate(ops.as_ref(), project_id)
            .await
            .expect("select candidate");

        assert_eq!(Some(4), selected.map(|selection| selection.pr));
        assert_eq!(0, ops.responses.lock().await.len());
    }

    #[tokio::test]
    #[ignore = "reads live GitHub data from rcore-os/tgoskits"]
    async fn live_selector_reads_rcore_os_tgoskits_without_enqueueing() {
        let ops = Arc::new(LiveSelectorOps::new());

        let selected = select_project_review_candidate(ops.as_ref(), Uuid::new_v4())
            .await
            .expect("select live candidate");

        let requested_paths = ops.requested_paths.lock().await;
        match selected.as_ref() {
            Some(selection) => {
                println!(
                    "selected rcore-os/tgoskits PR #{} at {:?} after {} GitHub requests",
                    selection.pr,
                    selection.head_sha,
                    requested_paths.len()
                );
            }
            None => {
                println!(
                    "no eligible rcore-os/tgoskits PR after {} GitHub requests",
                    requested_paths.len()
                );
            }
        }
        let owner = crate::github::github_path_segment("rcore-os");
        let expected_path = format!(
            "/repos/{owner}/tgoskits/pulls?state=open&sort=created&direction=asc&per_page=20&page=1"
        );
        assert_eq!(
            Some(expected_path.as_str()),
            requested_paths.first().map(String::as_str)
        );
        assert!(
            requested_paths
                .iter()
                .all(|path| !path.contains("/responses"))
        );
        assert!(
            selected
                .as_ref()
                .map(|selection| selection.pr > 0)
                .unwrap_or(true)
        );
    }

    fn test_project_summary(project_id: mai_protocol::ProjectId) -> mai_protocol::ProjectSummary {
        mai_protocol::ProjectSummary {
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

    fn pr_list_item(number: u64, draft: bool, head_sha: &str) -> Value {
        pr_detail(number, draft, head_sha)
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
