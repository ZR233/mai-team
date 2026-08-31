use std::future::Future;

use mai_protocol::{
    AgentId, ProjectId, ProjectReviewDecision, ProjectReviewOutcome, ProjectReviewRunDetail,
    ProjectReviewRunStatus, ProjectReviewRunSummary, ProjectReviewRunsResponse, ThreadTurnHistory,
    TokenUsage, TurnId, now,
};
use mai_store::MaiStore;
use uuid::Uuid;

use crate::{Result, RuntimeError};

/// 从 reviewer 唯一拥有的 canonical Thread 读取 Run 终态快照。
///
/// 实现必须读取已持久化的 Thread 文档，不能依赖异步产品投影或内存摘要。
pub(crate) trait ReviewRunSnapshotSource: Send + Sync {
    fn snapshot(
        &self,
        reviewer_agent_id: AgentId,
        turn_id: Option<&str>,
    ) -> impl Future<Output = Result<ReviewRunSnapshot>> + Send;
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReviewRunSnapshot {
    pub(crate) token_usage: TokenUsage,
    pub(crate) history: Option<ThreadTurnHistory>,
}

#[derive(Debug, Clone)]
pub(crate) struct FinishReviewRun {
    pub(crate) run_id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) reviewer_agent_id: Option<AgentId>,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) status: ProjectReviewRunStatus,
    pub(crate) outcome: Option<ProjectReviewOutcome>,
    pub(crate) review_event: Option<ProjectReviewDecision>,
    pub(crate) pr: Option<u64>,
    pub(crate) summary_text: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) failure: Option<mai_protocol::ProjectReviewFailure>,
}

pub(crate) async fn list_project_review_runs(
    store: &MaiStore,
    project_id: ProjectId,
    offset: usize,
    limit: usize,
) -> Result<ProjectReviewRunsResponse> {
    let runs = store
        .load_project_review_runs(project_id, None, offset, limit)
        .await?;
    Ok(ProjectReviewRunsResponse { runs })
}

pub(crate) async fn get_project_review_run(
    store: &MaiStore,
    project_id: ProjectId,
    run_id: Uuid,
) -> Result<ProjectReviewRunDetail> {
    store
        .load_project_review_run(project_id, run_id)
        .await?
        .ok_or(RuntimeError::ProjectReviewRunNotFound(run_id))
}

pub(crate) async fn record_project_review_startup_failure(
    store: &MaiStore,
    project_id: ProjectId,
    error: String,
) -> Result<()> {
    let run_id = Uuid::new_v4();
    save_project_review_run_status(
        store,
        ProjectReviewRunSummary {
            id: run_id,
            job_id: None,
            attempt_index: 1,
            project_id,
            reviewer_agent_id: None,
            turn_id: None,
            started_at: now(),
            finished_at: Some(now()),
            status: ProjectReviewRunStatus::Failed,
            outcome: Some(ProjectReviewOutcome::Failed),
            review_event: None,
            pr: None,
            summary: None,
            error: Some(error),
            failure: None,
            token_usage: TokenUsage::default(),
            history_status: Default::default(),
            history_archive_id: None,
            history_archived_at: None,
        },
        None,
    )
    .await
}

pub(crate) async fn cancel_active_project_review_runs(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    project_id: ProjectId,
    reviewer_agent_id: Option<AgentId>,
    run_list_limit: usize,
) -> Result<()> {
    let runs = store
        .load_project_review_runs(project_id, None, 0, run_list_limit)
        .await?;
    for run in runs {
        if run.finished_at.is_some()
            || !matches!(
                run.status,
                ProjectReviewRunStatus::Syncing | ProjectReviewRunStatus::Running
            )
            || reviewer_agent_id.is_some_and(|id| run.reviewer_agent_id != Some(id))
        {
            continue;
        }
        let _ = finish_project_review_run(
            store,
            snapshot_source,
            FinishReviewRun {
                run_id: run.id,
                project_id,
                reviewer_agent_id: run.reviewer_agent_id,
                turn_id: run.turn_id,
                status: ProjectReviewRunStatus::Cancelled,
                outcome: None,
                review_event: None,
                pr: run.pr,
                summary_text: run.summary,
                error: Some("review cancelled".to_string()),
                failure: None,
            },
        )
        .await;
    }
    Ok(())
}

pub(crate) async fn save_project_review_run_status(
    store: &MaiStore,
    summary: ProjectReviewRunSummary,
    history: Option<ThreadTurnHistory>,
) -> Result<()> {
    store
        .save_project_review_run(&ProjectReviewRunDetail { summary, history })
        .await?;
    Ok(())
}

pub(crate) async fn update_project_review_run_turn(
    store: &MaiStore,
    project_id: ProjectId,
    run_id: Uuid,
    reviewer_agent_id: AgentId,
    turn_id: TurnId,
) -> Result<()> {
    store
        .update_active_project_review_run_turn(project_id, run_id, reviewer_agent_id, turn_id)
        .await?;
    Ok(())
}

pub(crate) async fn finish_project_review_run(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    request: FinishReviewRun,
) -> Result<()> {
    finish_project_review_run_at(
        store,
        snapshot_source,
        request,
        now(),
        ReviewRunFinalizationMode::ActiveAttempt,
    )
    .await
}

pub(crate) async fn finish_expired_project_review_run(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    request: FinishReviewRun,
) -> Result<()> {
    finish_project_review_run_at(
        store,
        snapshot_source,
        request,
        now(),
        ReviewRunFinalizationMode::ExpiredRecovery,
    )
    .await
}

#[derive(Clone, Copy)]
enum ReviewRunFinalizationMode {
    ActiveAttempt,
    ExpiredRecovery,
}

async fn finish_project_review_run_at(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    request: FinishReviewRun,
    finished_at: chrono::DateTime<chrono::Utc>,
    mode: ReviewRunFinalizationMode,
) -> Result<()> {
    let Some(existing) = store
        .load_project_review_run(request.project_id, request.run_id)
        .await?
    else {
        return Err(RuntimeError::ProjectReviewRunNotFound(request.run_id));
    };
    let reviewer_agent_id = request
        .reviewer_agent_id
        .or(existing.summary.reviewer_agent_id);
    let turn_id = request.turn_id.or(existing.summary.turn_id);
    let snapshot = if let Some(reviewer_agent_id) = reviewer_agent_id {
        match snapshot_source
            .snapshot(reviewer_agent_id, turn_id.as_deref())
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) if matches!(mode, ReviewRunFinalizationMode::ExpiredRecovery) => {
                tracing::warn!(
                    %reviewer_agent_id,
                    ?turn_id,
                    %error,
                    "expired Review Run recovery is continuing without canonical Thread snapshot"
                );
                ReviewRunSnapshot::default()
            }
            Err(error) => return Err(error),
        }
    } else {
        ReviewRunSnapshot::default()
    };
    if reviewer_agent_id.is_some() && turn_id.is_some() && snapshot.history.is_none() {
        match mode {
            ReviewRunFinalizationMode::ActiveAttempt => {
                return Err(RuntimeError::InvalidInput(format!(
                    "review run {} cannot finish without canonical Thread history",
                    request.run_id
                )));
            }
            ReviewRunFinalizationMode::ExpiredRecovery => {
                tracing::warn!(
                    run_id = %request.run_id,
                    ?reviewer_agent_id,
                    ?turn_id,
                    "expired Review Run recovery has no canonical Thread history"
                );
            }
        }
    }
    let detail = ProjectReviewRunDetail {
        summary: ProjectReviewRunSummary {
            id: request.run_id,
            job_id: existing.summary.job_id,
            attempt_index: existing.summary.attempt_index,
            project_id: request.project_id,
            reviewer_agent_id,
            turn_id,
            started_at: existing.summary.started_at,
            finished_at: Some(finished_at),
            status: request.status,
            outcome: request.outcome,
            review_event: request.review_event,
            pr: request.pr.or(existing.summary.pr),
            summary: request.summary_text,
            error: request.error,
            failure: request.failure,
            token_usage: snapshot.token_usage,
            history_status: existing.summary.history_status,
            history_archive_id: existing.summary.history_archive_id,
            history_archived_at: existing.summary.history_archived_at,
        },
        history: snapshot.history,
    };
    match mode {
        ReviewRunFinalizationMode::ActiveAttempt => {
            store.finish_project_review_run(&detail).await?;
        }
        ReviewRunFinalizationMode::ExpiredRecovery => {
            store.finish_expired_project_review_run(&detail).await?;
        }
    }
    Ok(())
}

pub(crate) async fn recover_expired_terminal_project_review_runs(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    recovered_at: chrono::DateTime<chrono::Utc>,
) -> Result<usize> {
    let candidates = store
        .load_expired_unfinished_terminal_project_review_attempts(recovered_at)
        .await?;
    let recovered = candidates.len();
    for (job, run) in candidates {
        finish_terminal_project_review_run(
            store,
            snapshot_source,
            &job,
            run,
            job.finished_at.unwrap_or(recovered_at),
        )
        .await?;
    }
    Ok(recovered)
}

pub(crate) async fn finish_owned_terminal_project_review_run(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    job: mai_protocol::ProjectReviewJobSummary,
    owner: String,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    if !job.status.is_terminal()
        || job.lease_owner.as_deref() != Some(owner.as_str())
        || job
            .lease_expires_at
            .is_none_or(|lease_expires_at| lease_expires_at <= now)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "review job {} is not terminal with an unexpired lease owned by {owner}",
            job.id
        )));
    }
    let run_id = job.active_run_id.ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "terminal review job {} has no active Run to finish",
            job.id
        ))
    })?;
    let run = store
        .load_project_review_run(job.project_id, run_id)
        .await?
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "terminal review job {} references missing active Run {run_id}",
                job.id
            ))
        })?
        .summary;
    if run.job_id != Some(job.id) {
        return Err(RuntimeError::InvalidInput(format!(
            "terminal review job {} does not own active Run {run_id}",
            job.id
        )));
    }
    finish_terminal_project_review_run(
        store,
        snapshot_source,
        &job,
        run,
        job.finished_at.unwrap_or(now),
    )
    .await
}

async fn finish_terminal_project_review_run(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    job: &mai_protocol::ProjectReviewJobSummary,
    run: ProjectReviewRunSummary,
    finished_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let receipt = job.submission_receipt.as_ref();
    let submitted =
        job.status == mai_protocol::ProjectReviewJobStatus::Succeeded && receipt.is_some();
    finish_project_review_run_at(
        store,
        snapshot_source,
        FinishReviewRun {
            run_id: run.id,
            project_id: run.project_id,
            reviewer_agent_id: run.reviewer_agent_id,
            turn_id: run.turn_id,
            status: if submitted {
                ProjectReviewRunStatus::Succeeded
            } else {
                ProjectReviewRunStatus::Interrupted
            },
            outcome: submitted.then_some(ProjectReviewOutcome::ReviewSubmitted),
            review_event: receipt.map(|receipt| receipt.event.clone()),
            pr: run.pr,
            summary_text: run
                .summary
                .or_else(|| submitted.then(|| "GitHub review submission recorded.".to_string())),
            error: (!submitted).then(|| {
                run.error.unwrap_or_else(|| {
                    "review attempt completion was not persisted before its lease expired"
                        .to_string()
                })
            }),
            failure: if submitted { None } else { run.failure },
        },
        finished_at,
        ReviewRunFinalizationMode::ActiveAttempt,
    )
    .await
}

pub(crate) async fn archive_expired_project_review_runs(
    store: &MaiStore,
    snapshot_source: &impl ReviewRunSnapshotSource,
    recovered_at: chrono::DateTime<chrono::Utc>,
) -> Result<usize> {
    let attempts = store
        .load_expired_unfinished_active_project_review_attempts(recovered_at)
        .await?;
    let archived = attempts.len();
    for (_job, run) in attempts {
        finish_project_review_run_at(
            store,
            snapshot_source,
            FinishReviewRun {
                run_id: run.id,
                project_id: run.project_id,
                reviewer_agent_id: run.reviewer_agent_id,
                turn_id: run.turn_id,
                status: ProjectReviewRunStatus::Interrupted,
                outcome: None,
                review_event: None,
                pr: run.pr,
                summary_text: run.summary,
                error: Some(run.error.unwrap_or_else(|| {
                    "review attempt lease expired before completion".to_string()
                })),
                failure: run.failure,
            },
            recovered_at,
            ReviewRunFinalizationMode::ActiveAttempt,
        )
        .await?;
    }
    Ok(archived)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use mai_protocol::{
        ProjectReviewJobSource, ProjectReviewJobStatus, ProjectReviewJobSummary,
        ProjectReviewSubmissionIntent, ProjectReviewSubmissionReceipt, ThreadContextDisposition,
        Turn, TurnState,
    };
    use pl_protocol::{CompletedTurnState, TurnCompletion};
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;
    use crate::projects::review::job::{NewProjectReviewJob, new_project_review_job};

    #[derive(Clone)]
    struct FixedSnapshotSource {
        snapshot: ReviewRunSnapshot,
    }

    impl ReviewRunSnapshotSource for FixedSnapshotSource {
        async fn snapshot(
            &self,
            _reviewer_agent_id: AgentId,
            _turn_id: Option<&str>,
        ) -> Result<ReviewRunSnapshot> {
            Ok(self.snapshot.clone())
        }
    }

    async fn receipted_attempt(
        store: &MaiStore,
        pr: u64,
    ) -> (
        ProjectReviewJobSummary,
        ProjectReviewRunSummary,
        ThreadTurnHistory,
    ) {
        let project_id = Uuid::new_v4();
        let job = new_project_review_job(NewProjectReviewJob {
            project_id,
            pr,
            head_sha: format!("head-{pr}"),
            source: ProjectReviewJobSource::Manual,
            delivery_id: None,
            reason: "test".to_string(),
        });
        let job_id = job.id;
        store
            .enqueue_project_review_job(job.clone())
            .await
            .expect("enqueue job");
        let started_at = Utc::now() + TimeDelta::seconds(1);
        let owner = format!("worker-{pr}");
        let claimed = store
            .claim_due_project_review_job(
                project_id,
                owner.clone(),
                started_at,
                started_at + TimeDelta::minutes(10),
            )
            .await
            .expect("claim job")
            .expect("claimed job");
        let run_id = Uuid::new_v4();
        let active_job = store
            .begin_claimed_project_review_attempt(job_id, owner, run_id, started_at)
            .await
            .expect("begin attempt");
        let reviewer_agent_id = Uuid::new_v4();
        let turn_id = format!("turn-{pr}");
        update_project_review_run_turn(
            store,
            project_id,
            run_id,
            reviewer_agent_id,
            turn_id.clone(),
        )
        .await
        .expect("record reviewer turn");
        let submitted_at = started_at + TimeDelta::minutes(4);
        store
            .record_project_review_submission_intent(ProjectReviewSubmissionIntent {
                job_id,
                head_sha: active_job.head_sha.clone(),
                event: ProjectReviewDecision::Approve,
                body_hash: format!("hash-{pr}"),
                comment_count: 0,
                created_at: started_at,
            })
            .await
            .expect("record intent");
        let job = store
            .record_project_review_submission_receipt(
                job_id,
                ProjectReviewSubmissionReceipt {
                    github_review_id: pr,
                    event: ProjectReviewDecision::Approve,
                    head_sha: active_job.head_sha,
                    html_url: Some(format!("https://example.test/review/{pr}")),
                    submitted_at,
                },
            )
            .await
            .expect("record receipt");
        assert_eq!(claimed.id, job.id);
        let run = store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load run")
            .expect("unfinished run")
            .summary;
        let history = ThreadTurnHistory {
            turn: Turn {
                id: turn_id,
                thread_id: reviewer_agent_id.to_string(),
                revision: 0,
                state: TurnState::Completed(CompletedTurnState::new(
                    Some(started_at.timestamp_millis()),
                    submitted_at.timestamp_millis(),
                    TurnCompletion::Normal,
                )),
                updated_at: submitted_at.timestamp_millis(),
            },
            items: Vec::new(),
            context_disposition: ThreadContextDisposition::Active,
        };
        (job, run, history)
    }

    async fn test_store() -> (tempfile::TempDir, MaiStore) {
        let directory = tempdir().expect("tempdir");
        let store = MaiStore::open_with_config_and_artifact_index_path(
            directory.path().join("runtime.sqlite3"),
            directory.path().join("config.toml"),
            directory.path().join("artifacts/index"),
        )
        .await
        .expect("open store");
        (directory, store)
    }

    #[tokio::test]
    async fn owner_archives_submitted_thread_before_releasing_job_ownership() {
        let (_directory, store) = test_store().await;
        let (job, run, history) = receipted_attempt(&store, 2026).await;
        assert_eq!(ProjectReviewJobStatus::Succeeded, job.status);
        assert_eq!(Some(run.id), job.active_run_id);
        assert!(job.lease_owner.is_some());
        let source = FixedSnapshotSource {
            snapshot: ReviewRunSnapshot {
                token_usage: TokenUsage {
                    prompt_tokens: 2,
                    cached_prompt_tokens: 1,
                    cache_write_tokens: 0,
                    completion_tokens: 3,
                    reasoning_tokens: 4,
                    total_tokens: 10,
                },
                history: Some(history.clone()),
            },
        };

        finish_owned_terminal_project_review_run(
            &store,
            &source,
            job.clone(),
            job.lease_owner.clone().expect("lease owner"),
            Utc::now(),
        )
        .await
        .expect("finish owned submitted attempt");
        let archived = store
            .load_project_review_run(run.project_id, run.id)
            .await
            .expect("load archived run")
            .expect("archived run");
        assert_eq!(Some(history.clone()), archived.history);
        assert_eq!(ProjectReviewRunStatus::Succeeded, archived.summary.status);
        assert_eq!(
            Some(ProjectReviewOutcome::ReviewSubmitted),
            archived.summary.outcome
        );
        assert_eq!(
            Some(ProjectReviewDecision::Approve),
            archived.summary.review_event
        );
        assert_eq!(source.snapshot.token_usage, archived.summary.token_usage);
        let completed_job = store
            .load_project_review_job(job.project_id, job.id)
            .await
            .expect("load completed job")
            .expect("completed job");
        assert_eq!(None, completed_job.active_run_id);
        assert_eq!(None, completed_job.lease_owner);
        assert_eq!(None, completed_job.lease_expires_at);
    }

    #[tokio::test]
    async fn terminal_recovery_waits_for_expiry_and_ignores_non_active_run() {
        let (_directory, store) = test_store().await;
        let (job, run, history) = receipted_attempt(&store, 2027).await;
        let mut stray = store
            .load_project_review_run(run.project_id, run.id)
            .await
            .expect("load active run")
            .expect("active run");
        stray.summary.id = Uuid::new_v4();
        stray.summary.attempt_index += 1;
        store
            .save_project_review_run(&stray)
            .await
            .expect("save non-active unfinished run");
        let source = FixedSnapshotSource {
            snapshot: ReviewRunSnapshot {
                token_usage: TokenUsage::default(),
                history: Some(history),
            },
        };

        assert_eq!(
            0,
            recover_expired_terminal_project_review_runs(&store, &source, Utc::now())
                .await
                .expect("live terminal lease is retained")
        );
        let recovered_at = job.lease_expires_at.expect("lease expiry") + TimeDelta::seconds(1);
        assert_eq!(
            1,
            recover_expired_terminal_project_review_runs(&store, &source, recovered_at)
                .await
                .expect("recover expired terminal Run")
        );
        assert_eq!(
            stray,
            store
                .load_project_review_run(run.project_id, stray.summary.id)
                .await
                .expect("load non-active run")
                .expect("non-active run")
        );
    }

    #[tokio::test]
    async fn submitted_recovery_without_thread_history_retains_job_ownership() {
        let (_directory, store) = test_store().await;
        let (job, run, _history) = receipted_attempt(&store, 2098).await;
        let source = FixedSnapshotSource {
            snapshot: ReviewRunSnapshot::default(),
        };

        let recovered_at = job.lease_expires_at.expect("lease expiry") + TimeDelta::seconds(1);
        let error = recover_expired_terminal_project_review_runs(&store, &source, recovered_at)
            .await
            .expect_err("missing canonical history must block cleanup");
        assert!(
            error
                .to_string()
                .contains("cannot finish without canonical Thread history")
        );
        let unfinished = store
            .load_project_review_run(run.project_id, run.id)
            .await
            .expect("load unfinished run")
            .expect("unfinished run");
        assert_eq!(None, unfinished.summary.finished_at);
        assert_eq!(None, unfinished.history);
        let retained_job = store
            .load_project_review_job(job.project_id, job.id)
            .await
            .expect("load retained job")
            .expect("retained job");
        assert_eq!(Some(run.id), retained_job.active_run_id);
        assert!(retained_job.lease_owner.is_some());
    }

    #[tokio::test]
    async fn recovery_waits_for_lease_expiry_before_archiving_active_attempt() {
        let (_directory, store) = test_store().await;
        let project_id = Uuid::new_v4();
        let job = new_project_review_job(NewProjectReviewJob {
            project_id,
            pr: 2043,
            head_sha: "head-2043".to_string(),
            source: ProjectReviewJobSource::Webhook,
            delivery_id: Some("delivery-2043".to_string()),
            reason: "test".to_string(),
        });
        store
            .enqueue_project_review_job(job.clone())
            .await
            .expect("enqueue job");
        let started_at = Utc::now() + TimeDelta::seconds(1);
        let owner = "worker-2043".to_string();
        store
            .claim_due_project_review_job(
                project_id,
                owner.clone(),
                started_at,
                started_at + TimeDelta::minutes(10),
            )
            .await
            .expect("claim job")
            .expect("claimed job");
        let run_id = Uuid::new_v4();
        store
            .begin_claimed_project_review_attempt(job.id, owner, run_id, started_at)
            .await
            .expect("begin attempt");
        let reviewer_agent_id = Uuid::new_v4();
        let turn_id = "turn-2043".to_string();
        update_project_review_run_turn(
            &store,
            project_id,
            run_id,
            reviewer_agent_id,
            turn_id.clone(),
        )
        .await
        .expect("record reviewer turn");
        store
            .record_project_review_submission_intent(ProjectReviewSubmissionIntent {
                job_id: job.id,
                head_sha: job.head_sha.clone(),
                event: ProjectReviewDecision::Approve,
                body_hash: "hash-2043".to_string(),
                comment_count: 0,
                created_at: started_at,
            })
            .await
            .expect("record uncertain submission intent");
        let live_recovery_at = started_at + TimeDelta::minutes(2);
        let recovered_at = started_at + TimeDelta::minutes(11);
        let history = ThreadTurnHistory {
            turn: Turn {
                id: turn_id,
                thread_id: reviewer_agent_id.to_string(),
                revision: 0,
                state: TurnState::Completed(CompletedTurnState::new(
                    Some(started_at.timestamp_millis()),
                    recovered_at.timestamp_millis(),
                    TurnCompletion::Normal,
                )),
                updated_at: recovered_at.timestamp_millis(),
            },
            items: Vec::new(),
            context_disposition: ThreadContextDisposition::Active,
        };
        let source = FixedSnapshotSource {
            snapshot: ReviewRunSnapshot {
                token_usage: TokenUsage::default(),
                history: Some(history.clone()),
            },
        };

        assert_eq!(
            0,
            archive_expired_project_review_runs(&store, &source, live_recovery_at)
                .await
                .expect("live lease keeps attempt active")
        );
        let live = store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load live run")
            .expect("live run");
        assert_eq!(None, live.summary.finished_at);
        assert_eq!(None, live.history);

        assert_eq!(
            1,
            archive_expired_project_review_runs(&store, &source, recovered_at)
                .await
                .expect("archive interrupted attempt")
        );
        let archived = store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load archived run")
            .expect("archived run");
        assert_eq!(ProjectReviewRunStatus::Interrupted, archived.summary.status);
        assert_eq!(Some(history.clone()), archived.history);
        let pre_recovery_job = store
            .load_project_review_job(project_id, job.id)
            .await
            .expect("load owned job")
            .expect("owned job");
        assert_eq!(Some(run_id), pre_recovery_job.active_run_id);
        assert!(pre_recovery_job.lease_owner.is_some());

        assert_eq!(
            1,
            store
                .recover_expired_project_review_jobs(recovered_at)
                .await
                .expect("recover interrupted job")
        );
        let recovered_job = store
            .load_project_review_job(project_id, job.id)
            .await
            .expect("load recovered job")
            .expect("recovered job");
        assert_eq!(ProjectReviewJobStatus::Reconciling, recovered_job.status);
        assert_eq!(None, recovered_job.active_run_id);
        assert_eq!(None, recovered_job.lease_owner);
        let receipt_at = recovered_at + TimeDelta::seconds(1);
        let submitted_job = store
            .record_project_review_submission_receipt(
                job.id,
                ProjectReviewSubmissionReceipt {
                    github_review_id: 2043,
                    event: ProjectReviewDecision::Approve,
                    head_sha: job.head_sha,
                    html_url: Some("https://example.test/review/2043".to_string()),
                    submitted_at: receipt_at,
                },
            )
            .await
            .expect("reconcile persisted GitHub submission");
        assert_eq!(ProjectReviewJobStatus::Succeeded, submitted_job.status);
        let submitted_run = store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load submitted run")
            .expect("submitted run");
        assert_eq!(
            ProjectReviewRunStatus::Succeeded,
            submitted_run.summary.status
        );
        assert_eq!(
            Some(ProjectReviewOutcome::ReviewSubmitted),
            submitted_run.summary.outcome
        );
        assert_eq!(Some(history), submitted_run.history);
        assert!(
            store
                .claim_due_project_review_cleanup_task(
                    "cleanup-after-recovery".to_string(),
                    receipt_at,
                    receipt_at + TimeDelta::minutes(5),
                )
                .await
                .expect("cleanup claim")
                .is_some()
        );
    }
}
