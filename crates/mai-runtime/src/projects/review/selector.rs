use std::future::Future;

use futures::stream::{FuturesUnordered, StreamExt};
use mai_protocol::{ProjectId, ProjectReviewDiscoveryCounts, ProjectReviewSkipReason};
use tokio_util::sync::CancellationToken;

use super::eligibility::{
    EvaluatedProjectReviewPr, ProjectReviewEligibilityOps, SELECTOR_PAGE_SIZE,
    evaluate_project_review_pull_request, list_open_pull_requests,
};
pub(crate) use super::eligibility::{ProjectReviewIdentity, SelectedProjectReviewPr};
use crate::github::github_path_segment;
use crate::{ProjectReviewQueueSummary, Result, RuntimeError};

const SELECTOR_CANDIDATE_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectReviewDiscoveryAdmissionInput {
    pub(crate) eligible: Vec<SelectedProjectReviewPr>,
    pub(crate) pending_ci: Vec<SelectedProjectReviewPr>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectReviewDiscoveryAdmissionResult {
    pub(crate) queue: ProjectReviewQueueSummary,
    pub(crate) watched: Vec<u64>,
    pub(crate) suppressed: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectReviewSelectorRunResult {
    pub(crate) counts: ProjectReviewDiscoveryCounts,
    pub(crate) errors: Vec<String>,
}

/// 提供 GitHub 读取与 Review 批量准入能力。
///
/// 实现方必须把一次 discovery 的合格 Job 与 CI watch 作为一个短事务提交；
/// GitHub 读取、候选评估和排序不得在该事务内执行。
pub(crate) trait ProjectReviewSelectorOps: ProjectReviewEligibilityOps {
    fn admit_project_review_discovery(
        &self,
        project_id: ProjectId,
        input: ProjectReviewDiscoveryAdmissionInput,
    ) -> impl Future<Output = Result<ProjectReviewDiscoveryAdmissionResult>> + Send;
}

pub(crate) async fn run_project_review_selector(
    ops: &impl ProjectReviewSelectorOps,
    project_id: ProjectId,
    cancellation_token: CancellationToken,
) -> Result<ProjectReviewSelectorRunResult> {
    if cancellation_token.is_cancelled() {
        return Err(RuntimeError::TurnCancelled);
    }
    tracing::info!(project_id = %project_id, "project review discovery started");
    let summary = ops.project_summary(project_id).await?;
    let identity = ops.project_review_identity(project_id).await?;
    let owner = github_path_segment(&summary.owner);
    let repo = github_path_segment(&summary.repo);
    let mut page = 1_u64;
    let mut input = ProjectReviewDiscoveryAdmissionInput::default();
    let mut counts = ProjectReviewDiscoveryCounts::default();
    let mut errors = Vec::new();
    loop {
        if cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        let mut pull_requests =
            list_open_pull_requests(ops, project_id, &owner, &repo, page).await?;
        tracing::debug!(
            project_id = %project_id,
            page,
            count = pull_requests.len(),
            "project review discovery fetched open PR page"
        );
        if pull_requests.is_empty() {
            break;
        }
        counts.scanned = counts.scanned.saturating_add(pull_requests.len() as u64);
        pull_requests.sort_by_key(|pull_request| pull_request.number);
        evaluate_pull_request_page(
            ops,
            PullRequestPageSelection {
                project_id,
                owner: &owner,
                repo: &repo,
                reviewer_user_id: identity.user_id,
                pull_requests: &pull_requests,
                cancellation_token: cancellation_token.clone(),
            },
            &mut input,
            &mut errors,
        )
        .await?;
        if pull_requests.len() < SELECTOR_PAGE_SIZE as usize {
            break;
        }
        page += 1;
    }
    input.eligible.sort_by_key(|selection| selection.pr);
    input.pending_ci.sort_by_key(|selection| selection.pr);
    counts.eligible = input.eligible.len() as u64;
    counts.errors = errors.len() as u64;
    let admission = ops
        .admit_project_review_discovery(project_id, input)
        .await?;
    counts.queued = admission.queue.queued.len() as u64;
    counts.deduped = admission.queue.deduped.len() as u64;
    counts.watched = admission.watched.len() as u64;
    counts.suppressed = admission.suppressed.len() as u64;
    tracing::info!(
        project_id = %project_id,
        scanned = counts.scanned,
        eligible = counts.eligible,
        queued = counts.queued,
        deduped = counts.deduped,
        watched = counts.watched,
        suppressed = counts.suppressed,
        errors = counts.errors,
        "project review discovery completed"
    );
    Ok(ProjectReviewSelectorRunResult { counts, errors })
}

struct PullRequestPageSelection<'a> {
    project_id: ProjectId,
    owner: &'a str,
    repo: &'a str,
    reviewer_user_id: u64,
    pull_requests: &'a [super::eligibility::GithubPullRequest],
    cancellation_token: CancellationToken,
}

async fn evaluate_pull_request_page(
    ops: &impl ProjectReviewSelectorOps,
    page: PullRequestPageSelection<'_>,
    input: &mut ProjectReviewDiscoveryAdmissionInput,
    errors: &mut Vec<String>,
) -> Result<()> {
    let mut pending = FuturesUnordered::new();
    let mut next_index = 0usize;
    loop {
        while pending.len() < SELECTOR_CANDIDATE_CONCURRENCY
            && next_index < page.pull_requests.len()
        {
            if page.cancellation_token.is_cancelled() {
                return Err(RuntimeError::TurnCancelled);
            }
            let pull_request = &page.pull_requests[next_index];
            let pr = pull_request.number;
            pending.push(async move {
                (
                    pr,
                    evaluate_project_review_pull_request(
                        ops,
                        page.project_id,
                        page.owner,
                        page.repo,
                        page.reviewer_user_id,
                        pull_request,
                    )
                    .await,
                )
            });
            next_index += 1;
        }

        let Some((pr, result)) = pending.next().await else {
            return Ok(());
        };
        if page.cancellation_token.is_cancelled() {
            return Err(RuntimeError::TurnCancelled);
        }
        match result {
            Ok(evaluated) => collect_evaluated_pull_request(input, errors, evaluated),
            Err(error) => {
                tracing::warn!(
                    project_id = %page.project_id,
                    pr,
                    "project review discovery candidate failed: {error}"
                );
                errors.push(format!("PR #{pr}: {error}"));
            }
        }
    }
}

fn collect_evaluated_pull_request(
    input: &mut ProjectReviewDiscoveryAdmissionInput,
    errors: &mut Vec<String>,
    evaluated: EvaluatedProjectReviewPr,
) {
    let selection = SelectedProjectReviewPr {
        pr: evaluated.pr,
        head_sha: evaluated.head_sha,
    };
    match evaluated.skip_reason {
        None => input.eligible.push(selection),
        Some(ProjectReviewSkipReason::CiPending) if selection.head_sha.is_some() => {
            input.pending_ci.push(selection);
        }
        Some(ProjectReviewSkipReason::CiPending) => {
            errors.push(format!(
                "PR #{}: CI pending but the head SHA is missing",
                selection.pr
            ));
        }
        Some(
            ProjectReviewSkipReason::PullRequestClosed
            | ProjectReviewSkipReason::Draft
            | ProjectReviewSkipReason::AlreadyReviewedCurrentHead,
        ) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use mai_protocol::{
        ProjectCloneStatus, ProjectReviewOutcome, ProjectReviewStatus, ProjectStatus, now,
    };
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use super::{
        ProjectReviewDiscoveryAdmissionInput, ProjectReviewDiscoveryAdmissionResult,
        ProjectReviewIdentity, ProjectReviewSelectorOps, run_project_review_selector,
    };
    use crate::projects::review::eligibility::ProjectReviewEligibilityOps;
    use crate::{ProjectReviewQueueSummary, RuntimeError};

    enum FakeGithubResponse {
        Json(Value),
        Error(String),
    }

    #[derive(Default)]
    struct FakeSelectorOps {
        responses: Mutex<HashMap<String, FakeGithubResponse>>,
        admissions: Mutex<Vec<ProjectReviewDiscoveryAdmissionInput>>,
        admission_result: ProjectReviewDiscoveryAdmissionResult,
    }

    impl FakeSelectorOps {
        fn new(
            responses: HashMap<String, FakeGithubResponse>,
            admission_result: ProjectReviewDiscoveryAdmissionResult,
        ) -> Self {
            Self {
                responses: Mutex::new(responses),
                admissions: Mutex::new(Vec::new()),
                admission_result,
            }
        }
    }

    impl ProjectReviewEligibilityOps for FakeSelectorOps {
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
            Ok(ProjectReviewIdentity { user_id: 42 })
        }

        async fn github_api_get_json(
            &self,
            _project_id: mai_protocol::ProjectId,
            path: String,
        ) -> crate::Result<Value> {
            match self.responses.lock().await.remove(&path) {
                Some(FakeGithubResponse::Json(value)) => Ok(value),
                Some(FakeGithubResponse::Error(error)) => Err(RuntimeError::InvalidInput(error)),
                None => Err(RuntimeError::InvalidInput(format!(
                    "unexpected GitHub request: {path}"
                ))),
            }
        }
    }

    impl ProjectReviewSelectorOps for FakeSelectorOps {
        async fn admit_project_review_discovery(
            &self,
            _project_id: mai_protocol::ProjectId,
            input: ProjectReviewDiscoveryAdmissionInput,
        ) -> crate::Result<ProjectReviewDiscoveryAdmissionResult> {
            self.admissions.lock().await.push(input);
            Ok(self.admission_result.clone())
        }
    }

    #[tokio::test]
    async fn selector_reads_all_pages_and_commits_one_batch() {
        let project_id = Uuid::new_v4();
        let mut responses = base_multi_page_responses();
        add_candidate_responses(&mut responses, 1, "head-1", "completed");
        add_candidate_responses(&mut responses, 2, "head-2", "in_progress");
        responses.insert(
            pull_path(3),
            FakeGithubResponse::Error("candidate unavailable".to_string()),
        );
        add_candidate_responses(&mut responses, 21, "head-21", "completed");
        let ops = FakeSelectorOps::new(
            responses,
            ProjectReviewDiscoveryAdmissionResult {
                queue: ProjectReviewQueueSummary {
                    queued: vec![1],
                    deduped: vec![21],
                    ..Default::default()
                },
                watched: vec![2],
                suppressed: Vec::new(),
            },
        );

        let result = run_project_review_selector(&ops, project_id, CancellationToken::new())
            .await
            .expect("run discovery");

        assert_eq!(21, result.counts.scanned);
        assert_eq!(2, result.counts.eligible);
        assert_eq!(1, result.counts.queued);
        assert_eq!(1, result.counts.deduped);
        assert_eq!(1, result.counts.watched);
        assert_eq!(1, result.counts.errors);
        assert_eq!(1, result.errors.len());
        let admissions = ops.admissions.lock().await;
        assert_eq!(1, admissions.len());
        assert_eq!(vec![1, 21], pr_numbers(&admissions[0].eligible));
        assert_eq!(vec![2], pr_numbers(&admissions[0].pending_ci));
    }

    #[tokio::test]
    async fn selector_does_not_commit_when_pull_request_paging_fails() {
        let project_id = Uuid::new_v4();
        let mut responses = HashMap::new();
        responses.insert(list_path(1), FakeGithubResponse::Json(first_page()));
        responses.insert(
            list_path(2),
            FakeGithubResponse::Error("page unavailable".to_string()),
        );
        add_candidate_responses(&mut responses, 1, "head-1", "completed");
        for pr in 2..=20 {
            responses.insert(
                pull_path(pr),
                FakeGithubResponse::Json(pr_detail(pr, true, &format!("head-{pr}"))),
            );
        }
        let ops = FakeSelectorOps::new(responses, Default::default());

        let error = run_project_review_selector(&ops, project_id, CancellationToken::new())
            .await
            .expect_err("paging failure must abort discovery");

        assert_eq!("invalid input: page unavailable", error.to_string());
        assert_eq!(0, ops.admissions.lock().await.len());
    }

    fn base_multi_page_responses() -> HashMap<String, FakeGithubResponse> {
        let mut responses = HashMap::new();
        responses.insert(list_path(1), FakeGithubResponse::Json(first_page()));
        responses.insert(
            list_path(2),
            FakeGithubResponse::Json(json!([pr_detail(21, false, "head-21")])),
        );
        for pr in 4..=20 {
            responses.insert(
                pull_path(pr),
                FakeGithubResponse::Json(pr_detail(pr, true, &format!("head-{pr}"))),
            );
        }
        responses
    }

    fn first_page() -> Value {
        Value::Array(
            (1..=20)
                .map(|pr| pr_detail(pr, pr >= 4, &format!("head-{pr}")))
                .collect(),
        )
    }

    fn add_candidate_responses(
        responses: &mut HashMap<String, FakeGithubResponse>,
        pr: u64,
        head_sha: &str,
        check_status: &str,
    ) {
        let encoded_head = head_sha.replace('-', "%2D");
        responses.insert(
            pull_path(pr),
            FakeGithubResponse::Json(pr_detail(pr, false, head_sha)),
        );
        responses.insert(
            format!("/repos/owner/repo/pulls/{pr}/reviews?per_page=100&page=1"),
            FakeGithubResponse::Json(json!([])),
        );
        responses.insert(
            format!("/repos/owner/repo/commits/{encoded_head}"),
            FakeGithubResponse::Json(json!({
                "commit": {"committer": {"date": "2026-01-01T00:00:00Z"}}
            })),
        );
        responses.insert(
            format!("/repos/owner/repo/commits/{encoded_head}/check-runs?per_page=100"),
            FakeGithubResponse::Json(json!({
                "check_runs": [{"status": check_status, "conclusion": null}]
            })),
        );
        responses.insert(
            format!("/repos/owner/repo/commits/{encoded_head}/status"),
            FakeGithubResponse::Json(json!({"state": "success", "statuses": []})),
        );
    }

    fn list_path(page: u64) -> String {
        format!(
            "/repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page={page}"
        )
    }

    fn pull_path(pr: u64) -> String {
        format!("/repos/owner/repo/pulls/{pr}")
    }

    fn pr_detail(number: u64, draft: bool, head_sha: &str) -> Value {
        json!({
            "number": number,
            "state": "open",
            "draft": draft,
            "user": {"id": number + 100, "login": format!("user-{number}")},
            "head": {"sha": head_sha},
        })
    }

    fn pr_numbers(selections: &[super::SelectedProjectReviewPr]) -> Vec<u64> {
        selections.iter().map(|selection| selection.pr).collect()
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
}
