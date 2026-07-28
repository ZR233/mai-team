use std::future::Future;

use chrono::{DateTime, TimeDelta, Utc};
use mai_protocol::{ProjectId, ProjectReviewSkipReason};
use mai_store::ProjectReviewCiWatch;
use tokio::time::{Duration, sleep};

use super::eligibility::EvaluatedProjectReviewPr;
use crate::Result;

const CI_WATCH_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectReviewCiWatchAdmission {
    Enqueued,
    SignalChanged,
}

/// 提供 CI watch 后台复核所需的持久化和审查入队边界。
///
/// 实现必须对 head 条件删除和延期使用 CAS，避免旧复核覆盖新 webhook
/// 写入的 head；入队仍须走正常的 PR 单活事务。
pub(crate) trait ProjectReviewCiWatchOps: Send + Sync {
    fn load_due_project_review_ci_watches(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<ProjectReviewCiWatch>>> + Send;

    fn evaluate_project_review_pr(
        &self,
        project_id: ProjectId,
        pr: u64,
        head_sha_hint: Option<String>,
    ) -> impl Future<Output = Result<EvaluatedProjectReviewPr>> + Send;

    fn enqueue_project_review_ci_watch(
        &self,
        watch: ProjectReviewCiWatch,
        head_sha: String,
    ) -> impl Future<Output = Result<ProjectReviewCiWatchAdmission>> + Send;

    fn replace_project_review_ci_watch_head(
        &self,
        watch: ProjectReviewCiWatch,
        head_sha: String,
        next_check_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn reschedule_project_review_ci_watch(
        &self,
        watch: ProjectReviewCiWatch,
        next_check_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> impl Future<Output = Result<bool>> + Send;

    fn delete_project_review_ci_watch(
        &self,
        watch: ProjectReviewCiWatch,
    ) -> impl Future<Output = Result<bool>> + Send;
}

pub(crate) async fn run_project_review_ci_watch_loop(ops: &impl ProjectReviewCiWatchOps) {
    let interval = Duration::from_secs(super::PROJECT_REVIEW_CI_WATCH_INTERVAL_SECS);
    loop {
        if let Err(error) = reconcile_due_project_review_ci_watches(ops, Utc::now()).await {
            tracing::warn!("project review CI watch reconciliation failed: {error}");
        }
        sleep(interval).await;
    }
}

pub(crate) async fn reconcile_due_project_review_ci_watches(
    ops: &impl ProjectReviewCiWatchOps,
    now: DateTime<Utc>,
) -> Result<usize> {
    let watches = ops
        .load_due_project_review_ci_watches(now, CI_WATCH_BATCH_SIZE)
        .await?;
    let mut reconciled = 0;
    for watch in watches {
        reconcile_project_review_ci_watch(ops, watch, now).await;
        reconciled += 1;
    }
    if reconciled > 0 {
        tracing::info!(count = reconciled, "reconciled project review CI watches");
    }
    Ok(reconciled)
}

async fn reconcile_project_review_ci_watch(
    ops: &impl ProjectReviewCiWatchOps,
    watch: ProjectReviewCiWatch,
    now: DateTime<Utc>,
) {
    let evaluated = match ops
        .evaluate_project_review_pr(watch.project_id, watch.pr, Some(watch.head_sha.clone()))
        .await
    {
        Ok(evaluated) => evaluated,
        Err(error) => {
            tracing::warn!(
                project_id = %watch.project_id,
                pr = watch.pr,
                head_sha = %watch.head_sha,
                "failed to recheck project review CI watch: {error}"
            );
            reschedule_watch(ops, watch, now).await;
            return;
        }
    };

    match evaluated.skip_reason {
        Some(ProjectReviewSkipReason::CiPending) => {
            if let Some(head_sha) = evaluated.head_sha {
                if head_sha == watch.head_sha {
                    reschedule_watch(ops, watch, now).await;
                } else {
                    replace_watch_head(ops, watch, head_sha, now).await;
                }
            } else {
                reschedule_watch(ops, watch, now).await;
            }
        }
        None => {
            let head_sha = evaluated.head_sha.unwrap_or_else(|| watch.head_sha.clone());
            match ops
                .enqueue_project_review_ci_watch(watch.clone(), head_sha)
                .await
            {
                Ok(
                    ProjectReviewCiWatchAdmission::Enqueued
                    | ProjectReviewCiWatchAdmission::SignalChanged,
                ) => {}
                Err(error) => {
                    tracing::warn!(
                        project_id = %watch.project_id,
                        pr = watch.pr,
                        "failed to enqueue eligible project review CI watch: {error}"
                    );
                    reschedule_watch(ops, watch, now).await;
                }
            }
        }
        Some(
            ProjectReviewSkipReason::PullRequestClosed
            | ProjectReviewSkipReason::Draft
            | ProjectReviewSkipReason::AlreadyReviewedCurrentHead,
        ) => delete_watch(ops, watch).await,
    }
}

async fn replace_watch_head(
    ops: &impl ProjectReviewCiWatchOps,
    watch: ProjectReviewCiWatch,
    head_sha: String,
    now: DateTime<Utc>,
) {
    let project_id = watch.project_id;
    let pr = watch.pr;
    if let Err(error) = ops
        .replace_project_review_ci_watch_head(watch, head_sha, next_check_at(now), now)
        .await
    {
        tracing::warn!(
            project_id = %project_id,
            pr,
            "failed to update project review CI watch head: {error}"
        );
    }
}

async fn reschedule_watch(
    ops: &impl ProjectReviewCiWatchOps,
    watch: ProjectReviewCiWatch,
    now: DateTime<Utc>,
) {
    if let Err(error) = ops
        .reschedule_project_review_ci_watch(watch.clone(), next_check_at(now), now)
        .await
    {
        tracing::warn!(
            project_id = %watch.project_id,
            pr = watch.pr,
            "failed to reschedule project review CI watch: {error}"
        );
    }
}

async fn delete_watch(ops: &impl ProjectReviewCiWatchOps, watch: ProjectReviewCiWatch) {
    if let Err(error) = ops.delete_project_review_ci_watch(watch.clone()).await {
        tracing::warn!(
            project_id = %watch.project_id,
            pr = watch.pr,
            "failed to delete project review CI watch: {error}"
        );
    }
}

fn next_check_at(now: DateTime<Utc>) -> DateTime<Utc> {
    now + TimeDelta::seconds(super::PROJECT_REVIEW_CI_WATCH_INTERVAL_SECS as i64)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::*;
    use crate::RuntimeError;

    type ReplacedWatch = (ProjectReviewCiWatch, String, DateTime<Utc>);
    type RescheduledWatch = (ProjectReviewCiWatch, DateTime<Utc>);

    #[derive(Clone, Default)]
    struct FakeCiWatchOps {
        due: Arc<Mutex<Vec<ProjectReviewCiWatch>>>,
        evaluations: Arc<Mutex<VecDeque<Result<EvaluatedProjectReviewPr>>>>,
        enqueued: Arc<Mutex<Vec<(ProjectReviewCiWatch, String)>>>,
        replaced: Arc<Mutex<Vec<ReplacedWatch>>>,
        rescheduled: Arc<Mutex<Vec<RescheduledWatch>>>,
        deleted: Arc<Mutex<Vec<ProjectReviewCiWatch>>>,
    }

    impl ProjectReviewCiWatchOps for FakeCiWatchOps {
        async fn load_due_project_review_ci_watches(
            &self,
            _now: DateTime<Utc>,
            _limit: usize,
        ) -> Result<Vec<ProjectReviewCiWatch>> {
            Ok(self.due.lock().expect("due lock").clone())
        }

        async fn evaluate_project_review_pr(
            &self,
            _project_id: ProjectId,
            _pr: u64,
            _head_sha_hint: Option<String>,
        ) -> Result<EvaluatedProjectReviewPr> {
            self.evaluations
                .lock()
                .expect("evaluations lock")
                .pop_front()
                .expect("evaluation")
        }

        async fn enqueue_project_review_ci_watch(
            &self,
            watch: ProjectReviewCiWatch,
            head_sha: String,
        ) -> Result<ProjectReviewCiWatchAdmission> {
            self.enqueued
                .lock()
                .expect("enqueued lock")
                .push((watch, head_sha));
            Ok(ProjectReviewCiWatchAdmission::Enqueued)
        }

        async fn replace_project_review_ci_watch_head(
            &self,
            watch: ProjectReviewCiWatch,
            head_sha: String,
            next_check_at: DateTime<Utc>,
            _updated_at: DateTime<Utc>,
        ) -> Result<bool> {
            self.replaced
                .lock()
                .expect("replaced lock")
                .push((watch, head_sha, next_check_at));
            Ok(true)
        }

        async fn reschedule_project_review_ci_watch(
            &self,
            watch: ProjectReviewCiWatch,
            next_check_at: DateTime<Utc>,
            _updated_at: DateTime<Utc>,
        ) -> Result<bool> {
            self.rescheduled
                .lock()
                .expect("rescheduled lock")
                .push((watch, next_check_at));
            Ok(true)
        }

        async fn delete_project_review_ci_watch(
            &self,
            watch: ProjectReviewCiWatch,
        ) -> Result<bool> {
            self.deleted.lock().expect("deleted lock").push(watch);
            Ok(true)
        }
    }

    fn watch(now: DateTime<Utc>) -> ProjectReviewCiWatch {
        ProjectReviewCiWatch {
            project_id: Uuid::new_v4(),
            pr: 1520,
            head_sha: "head-1520".to_string(),
            delivery_id: Some("delivery-1".to_string()),
            reason: "synchronize".to_string(),
            next_check_at: now,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn pending_ci_watch_is_rescheduled_without_enqueueing() {
        let now = Utc::now();
        let watch = watch(now);
        let ops = FakeCiWatchOps::default();
        ops.due.lock().expect("due lock").push(watch.clone());
        ops.evaluations
            .lock()
            .expect("evaluations lock")
            .push_back(Ok(EvaluatedProjectReviewPr {
                pr: watch.pr,
                head_sha: Some(watch.head_sha.clone()),
                skip_reason: Some(ProjectReviewSkipReason::CiPending),
            }));

        assert_eq!(
            1,
            reconcile_due_project_review_ci_watches(&ops, now)
                .await
                .expect("reconcile")
        );
        assert!(ops.enqueued.lock().expect("enqueued lock").is_empty());
        assert!(ops.deleted.lock().expect("deleted lock").is_empty());
        assert_eq!(
            vec![(watch, next_check_at(now))],
            *ops.rescheduled.lock().expect("rescheduled lock")
        );
    }

    #[tokio::test]
    async fn eligible_ci_watch_enqueues_once_then_deletes_watch() {
        let now = Utc::now();
        let watch = watch(now);
        let ops = FakeCiWatchOps::default();
        ops.due.lock().expect("due lock").push(watch.clone());
        ops.evaluations
            .lock()
            .expect("evaluations lock")
            .push_back(Ok(EvaluatedProjectReviewPr {
                pr: watch.pr,
                head_sha: Some(watch.head_sha.clone()),
                skip_reason: None,
            }));

        reconcile_due_project_review_ci_watches(&ops, now)
            .await
            .expect("reconcile");

        let enqueued = ops.enqueued.lock().expect("enqueued lock");
        assert_eq!(1, enqueued.len());
        assert_eq!(watch, enqueued[0].0);
        assert_eq!(watch.head_sha, enqueued[0].1);
        assert!(ops.deleted.lock().expect("deleted lock").is_empty());
        assert!(ops.rescheduled.lock().expect("rescheduled lock").is_empty());
    }

    #[tokio::test]
    async fn changed_head_replaces_pending_watch_generation() {
        let now = Utc::now();
        let watch = watch(now);
        let ops = FakeCiWatchOps::default();
        ops.due.lock().expect("due lock").push(watch.clone());
        ops.evaluations
            .lock()
            .expect("evaluations lock")
            .push_back(Ok(EvaluatedProjectReviewPr {
                pr: watch.pr,
                head_sha: Some("new-head".to_string()),
                skip_reason: Some(ProjectReviewSkipReason::CiPending),
            }));

        reconcile_due_project_review_ci_watches(&ops, now)
            .await
            .expect("reconcile");

        let replaced = ops.replaced.lock().expect("replaced lock");
        assert_eq!(
            vec![(watch, "new-head".to_string(), next_check_at(now))],
            *replaced
        );
        assert!(ops.rescheduled.lock().expect("rescheduled lock").is_empty());
    }

    #[tokio::test]
    async fn lookup_failure_keeps_watch_for_retry() {
        let now = Utc::now();
        let watch = watch(now);
        let ops = FakeCiWatchOps::default();
        ops.due.lock().expect("due lock").push(watch.clone());
        ops.evaluations
            .lock()
            .expect("evaluations lock")
            .push_back(Err(RuntimeError::InvalidInput(
                "temporary GitHub failure".to_string(),
            )));

        reconcile_due_project_review_ci_watches(&ops, now)
            .await
            .expect("reconcile");

        assert_eq!(
            vec![(watch, next_check_at(now))],
            *ops.rescheduled.lock().expect("rescheduled lock")
        );
        assert!(ops.deleted.lock().expect("deleted lock").is_empty());
    }
}
