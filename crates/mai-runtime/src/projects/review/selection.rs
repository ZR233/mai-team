use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewSelection {
    pub(crate) pr: u64,
    pub(crate) head_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequestCandidate {
    pub(crate) number: u64,
    pub(crate) author_login: Option<String>,
    pub(crate) draft: bool,
    pub(crate) head_sha: Option<String>,
    pub(crate) latest_commit_at: Option<DateTime<Utc>>,
    pub(crate) reviews: Vec<PullRequestReview>,
    pub(crate) check_signals: Vec<CheckSignal>,
    pub(crate) combined_status_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequestReview {
    pub(crate) author_login: Option<String>,
    pub(crate) submitted_at: Option<DateTime<Utc>>,
    pub(crate) commit_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckSignal {
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
}

pub(crate) fn select_review_pr(
    reviewer_login: &str,
    mut candidates: Vec<PullRequestCandidate>,
) -> Option<ReviewSelection> {
    candidates.sort_by_key(|candidate| candidate.number);
    candidates.into_iter().find_map(|candidate| {
        if candidate.draft
            || has_running_ci(&candidate)
            || already_reviewed_current_head(reviewer_login, &candidate)
        {
            return None;
        }
        Some(ReviewSelection {
            pr: candidate.number,
            head_sha: candidate.head_sha,
        })
    })
}

fn has_running_ci(candidate: &PullRequestCandidate) -> bool {
    candidate
        .check_signals
        .iter()
        .any(|signal| signal.status.as_deref().is_some_and(is_pending_ci_state))
}

fn is_pending_ci_state(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "queued" | "requested" | "waiting" | "pending" | "in_progress"
    )
}

fn already_reviewed_current_head(reviewer_login: &str, candidate: &PullRequestCandidate) -> bool {
    let Some(latest_review) = latest_reviewer_review(reviewer_login, &candidate.reviews) else {
        return false;
    };
    if let (Some(review_commit), Some(head_sha)) = (
        latest_review.commit_id.as_deref(),
        candidate.head_sha.as_deref(),
    ) && review_commit == head_sha
    {
        return true;
    }
    if let (Some(reviewed_at), Some(latest_commit_at)) =
        (latest_review.submitted_at, candidate.latest_commit_at)
    {
        return latest_commit_at <= reviewed_at;
    }
    false
}

fn latest_reviewer_review<'a>(
    reviewer_login: &str,
    reviews: &'a [PullRequestReview],
) -> Option<&'a PullRequestReview> {
    reviews
        .iter()
        .filter(|review| review.author_login.as_deref() == Some(reviewer_login))
        .filter(|review| review.submitted_at.is_some())
        .max_by_key(|review| review.submitted_at)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use pretty_assertions::assert_eq;

    use super::{
        CheckSignal, PullRequestCandidate, PullRequestReview, ReviewSelection, select_review_pr,
    };

    fn candidate(number: u64) -> PullRequestCandidate {
        PullRequestCandidate {
            number,
            author_login: None,
            draft: false,
            head_sha: Some(format!("head-{number}")),
            latest_commit_at: Some(Utc::now()),
            reviews: Vec::new(),
            check_signals: Vec::new(),
            combined_status_state: None,
        }
    }

    #[test]
    fn selects_first_eligible_candidate_by_number() {
        let mut first = candidate(1);
        first.draft = true;
        let mut second = candidate(2);
        second.check_signals = vec![CheckSignal {
            status: Some("completed".to_string()),
            conclusion: Some("failure".to_string()),
        }];
        let mut third = candidate(3);
        third.check_signals = vec![CheckSignal {
            status: Some("completed".to_string()),
            conclusion: Some("success".to_string()),
        }];

        let selected = select_review_pr("mai-bot", vec![third, second, first]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 2,
                head_sha: Some("head-2".to_string()),
            }),
            selected
        );
    }

    #[test]
    fn skips_pending_ci_but_not_failed_or_unknown_ci() {
        let mut pending = candidate(4);
        pending.check_signals = vec![CheckSignal {
            status: Some("in_progress".to_string()),
            conclusion: None,
        }];
        let mut failed = candidate(5);
        failed.check_signals = vec![CheckSignal {
            status: Some("completed".to_string()),
            conclusion: Some("failure".to_string()),
        }];

        let selected = select_review_pr("mai-bot", vec![pending, failed]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 5,
                head_sha: Some("head-5".to_string()),
            }),
            selected
        );
    }

    #[test]
    fn self_authored_pr_uses_the_same_rules() {
        let mut candidate = candidate(9);
        candidate.author_login = Some("mai-bot".to_string());

        let selected = select_review_pr("mai-bot", vec![candidate]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 9,
                head_sha: Some("head-9".to_string()),
            }),
            selected
        );
    }

    #[test]
    fn legacy_status_pending_context_blocks_selection() {
        let mut pending = candidate(10);
        pending.check_signals = vec![CheckSignal {
            status: Some("pending".to_string()),
            conclusion: None,
        }];
        let next = candidate(11);

        let selected = select_review_pr("mai-bot", vec![pending, next]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 11,
                head_sha: Some("head-11".to_string()),
            }),
            selected
        );
    }

    #[test]
    fn empty_legacy_combined_status_pending_does_not_block_completed_check_runs() {
        let mut candidate = candidate(14);
        candidate.combined_status_state = Some("pending".to_string());
        candidate.check_signals = vec![CheckSignal {
            status: Some("completed".to_string()),
            conclusion: Some("success".to_string()),
        }];

        let selected = select_review_pr("mai-bot", vec![candidate]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 14,
                head_sha: Some("head-14".to_string()),
            }),
            selected
        );
    }

    #[test]
    fn suppresses_pr_already_reviewed_at_current_head() {
        let mut reviewed = candidate(6);
        reviewed.reviews = vec![PullRequestReview {
            author_login: Some("mai-bot".to_string()),
            submitted_at: Some(Utc::now() - TimeDelta::minutes(5)),
            commit_id: Some("head-6".to_string()),
        }];
        let next = candidate(7);

        let selected = select_review_pr("mai-bot", vec![reviewed, next]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 7,
                head_sha: Some("head-7".to_string()),
            }),
            selected
        );
    }

    #[test]
    fn suppresses_pr_when_head_commit_is_not_newer_than_latest_review() {
        let review_time = Utc::now();
        let mut reviewed = candidate(12);
        reviewed.latest_commit_at = Some(review_time - TimeDelta::minutes(1));
        reviewed.reviews = vec![PullRequestReview {
            author_login: Some("mai-bot".to_string()),
            submitted_at: Some(review_time),
            commit_id: Some("old-head".to_string()),
        }];
        let next = candidate(13);

        let selected = select_review_pr("mai-bot", vec![reviewed, next]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 13,
                head_sha: Some("head-13".to_string()),
            }),
            selected
        );
    }

    #[test]
    fn allows_rereview_after_new_commit() {
        let review_time = Utc::now() - TimeDelta::hours(1);
        let mut candidate = candidate(8);
        candidate.latest_commit_at = Some(review_time + TimeDelta::minutes(10));
        candidate.reviews = vec![PullRequestReview {
            author_login: Some("mai-bot".to_string()),
            submitted_at: Some(review_time),
            commit_id: Some("old-head".to_string()),
        }];

        let selected = select_review_pr("mai-bot", vec![candidate]);

        assert_eq!(
            Some(ReviewSelection {
                pr: 8,
                head_sha: Some("head-8".to_string()),
            }),
            selected
        );
    }
}
