use super::*;
use crate::schema::SETTING_SCHEMA_VERSION;
use mai_protocol::{
    McpServerScope, McpServerTransport, ProjectCloneStatus, ProjectReviewDecision,
    ProjectReviewEnvironmentWarning, ProjectReviewFailure, ProjectReviewFailureCategory,
    ProjectReviewJobSource, ProjectReviewJobStatus, ProjectReviewOutcome, ProjectReviewRunStatus,
    ProjectReviewStatus, ProjectReviewSubmissionIntent, ProjectReviewSubmissionReceipt,
    ProjectStatus, ThreadContextDisposition, ThreadItem, ThreadItemState, ThreadTextChannel,
    ThreadTurnHistory, Turn, TurnState,
};
use pl_protocol::{CompletedTurnState, ThreadContentLifecycle, ThreadTextItem, TurnCompletion};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tempfile::{TempDir, tempdir};
use tokio::time::{Duration, timeout};

async fn store() -> (TempDir, MaiStore) {
    let dir = tempdir().expect("tempdir");
    let store = MaiStore::open_with_config_and_artifact_index_path(
        dir.path().join("config.sqlite3"),
        dir.path().join("config.toml"),
        dir.path().join("artifacts/index"),
    )
    .await
    .expect("open store");
    (dir, store)
}

fn test_project_summary(project_id: ProjectId, maintainer_agent_id: AgentId) -> ProjectSummary {
    let timestamp = Utc::now();
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
        docker_image: "ubuntu:latest".to_string(),
        clone_status: ProjectCloneStatus::Ready,
        maintainer_agent_id,
        created_at: timestamp,
        updated_at: timestamp,
        last_error: None,
        auto_review_enabled: true,
        reviewer_extra_prompt: None,
        review_status: ProjectReviewStatus::Waiting,
        current_reviewer_agent_id: None,
        last_review_started_at: None,
        last_review_finished_at: None,
        next_review_at: None,
        last_review_outcome: None,
        review_last_error: None,
    }
}

fn completed_turn(id: String, thread_id: String, started_at: i64, completed_at: i64) -> Turn {
    Turn {
        id,
        thread_id,
        revision: 1,
        state: TurnState::Completed(CompletedTurnState::new(
            Some(started_at),
            completed_at,
            TurnCompletion::Normal,
        )),
        updated_at: completed_at,
    }
}

fn completed_final_item(
    id: &str,
    thread_id: String,
    turn_id: String,
    text: &str,
    completed_at: i64,
) -> ThreadItem {
    ThreadItem::new(
        id.to_string(),
        thread_id,
        turn_id,
        0,
        1,
        completed_at,
        completed_at,
        ThreadItemState::Text(ThreadTextItem::new(
            ThreadTextChannel::Final,
            text.to_string(),
            Vec::new(),
            ThreadContentLifecycle::completed(completed_at),
        )),
    )
}

#[tokio::test]
async fn open_in_data_dir_uses_standard_layout() {
    let dir = tempdir().expect("tempdir");
    let data_dir = dir.path().join(".mai-team");

    let store = MaiStore::open_in_data_dir(&data_dir)
        .await
        .expect("open store");

    assert_eq!(store.path(), data_dir.join("mai-team.sqlite3"));
    assert_eq!(store.config_path(), data_dir.join("config.toml"));
    assert_eq!(
        store.artifact_index_dir(),
        data_dir.join("artifacts").join("index")
    );
}

#[tokio::test]
async fn save_project_waits_for_temporary_sqlite_write_lock() {
    let (_dir, store) = store().await;
    let project = test_project_summary(Uuid::new_v4(), Uuid::new_v4());
    let path = store.path().to_path_buf();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let holder = std::thread::spawn(move || {
        let connection = rusqlite::Connection::open(path).expect("open lock holder");
        connection
            .execute("BEGIN IMMEDIATE", [])
            .expect("hold write lock");
        ready_tx.send(()).expect("signal write lock");
        std::thread::sleep(Duration::from_secs(6));
        connection
            .execute("COMMIT", [])
            .expect("release write lock");
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("write lock is held");

    timeout(Duration::from_secs(12), store.save_project(&project))
        .await
        .expect("save project timeout")
        .expect("save project");
    holder.join().expect("lock holder");

    let projects = store.load_projects().await.expect("load projects");
    assert_eq!(
        serde_json::to_value(&projects).expect("projects json"),
        serde_json::to_value(vec![project]).expect("expected json")
    );
}

fn test_product_event(
    sequence: u64,
    agent_id: AgentId,
    timestamp: DateTime<Utc>,
) -> MaiProductEventEnvelope {
    MaiProductEventEnvelope {
        sequence,
        timestamp,
        kind: MaiProductEventKind::OperationFailed {
            scope: "test".to_string(),
            agent_id: Some(agent_id),
            message: "test failure".to_string(),
        },
    }
}

#[tokio::test]
async fn artifacts_use_configured_index_dir() {
    let dir = tempdir().expect("tempdir");
    let index_dir = dir.path().join("artifact-index");
    let store = MaiStore::open_with_config_and_artifact_index_path(
        dir.path().join("config.sqlite3"),
        dir.path().join("config.toml"),
        &index_dir,
    )
    .await
    .expect("open store");
    let task_id = Uuid::new_v4();
    let artifact = ArtifactInfo {
        id: "artifact-1".to_string(),
        agent_id: Uuid::new_v4(),
        task_id,
        name: "report.txt".to_string(),
        path: "/workspace/report.txt".to_string(),
        size_bytes: 7,
        created_at: Utc::now(),
    };

    store.save_artifact(&artifact).expect("save artifact");

    assert!(index_dir.join("artifact-1.json").exists());
    assert!(!dir.path().join("artifacts/index/artifact-1.json").exists());
    let artifacts = store.load_artifacts(&task_id).expect("load artifacts");
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].id, artifact.id);
    assert_eq!(artifacts[0].task_id, artifact.task_id);
    assert_eq!(artifacts[0].name, artifact.name);

    let all_artifacts = store.load_all_artifacts().expect("load all artifacts");
    assert_eq!(all_artifacts.len(), 1);
    assert_eq!(all_artifacts[0].id, artifact.id);
}

#[tokio::test]
async fn git_account_save_enters_verifying_and_clears_previous_error() {
    let (_dir, store) = store().await;
    let saved = store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    assert_eq!(saved.status, GitAccountStatus::Verifying);
    assert_eq!(saved.last_error, None);
    assert_eq!(saved.last_verified_at, None);

    let failed = store
        .update_git_account_verification(
            "account-1",
            None,
            GitTokenKind::Unknown,
            Vec::new(),
            GitAccountStatus::Failed,
            Some("bad token".to_string()),
        )
        .await
        .expect("mark failed");
    assert_eq!(failed.status, GitAccountStatus::Failed);
    assert!(failed.last_verified_at.is_some());

    let resaved = store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("new-secret".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("resave account");
    assert_eq!(resaved.status, GitAccountStatus::Verifying);
    assert_eq!(resaved.last_error, None);
    assert_eq!(resaved.last_verified_at, None);
}

#[tokio::test]
async fn github_app_relay_account_has_installation_metadata_without_token() {
    let (_dir, store) = store().await;
    let saved = store
        .upsert_github_app_relay_account(42, "octo-org", "relay-main", true)
        .await
        .expect("save relay account");

    assert_eq!(saved.id, "github-app-installation-42");
    assert_eq!(saved.provider, GitProvider::GithubAppRelay);
    assert_eq!(saved.status, GitAccountStatus::Verified);
    assert!(!saved.has_token);
    assert_eq!(saved.installation_id, Some(42));
    assert_eq!(saved.installation_account.as_deref(), Some("octo-org"));
    assert_eq!(saved.relay_id.as_deref(), Some("relay-main"));

    let loaded = store
        .git_account("github-app-installation-42")
        .await
        .expect("load")
        .expect("account");
    assert_eq!(loaded.provider, GitProvider::GithubAppRelay);
    assert_eq!(loaded.installation_id, Some(42));
}

#[tokio::test]
async fn relay_settings_persist_without_exposing_token() {
    let (_dir, store) = store().await;

    let saved = store
        .save_relay_settings(RelaySettingsRequest {
            enabled: true,
            url: Some(" https://relay.example/ ".to_string()),
            token: Some(" relay-token ".to_string()),
            node_id: Some(" node-a ".to_string()),
        })
        .await
        .expect("save relay settings");

    assert!(saved.enabled);
    assert_eq!(saved.url, "https://relay.example");
    assert!(saved.has_token);
    assert_eq!(saved.node_id, "node-a");

    let loaded = store.relay_settings().await.expect("load relay settings");
    assert_eq!(loaded, saved);

    let kept = store
        .save_relay_settings(RelaySettingsRequest {
            enabled: true,
            url: Some("https://relay-two.example".to_string()),
            token: None,
            node_id: Some("node-b".to_string()),
        })
        .await
        .expect("save relay settings keeping token");
    assert!(kept.has_token);
    assert_eq!(kept.url, "https://relay-two.example");
    assert_eq!(kept.node_id, "node-b");
    assert_eq!(
        store.relay_secret().await.expect("load relay secret"),
        Some((
            "https://relay-two.example".to_string(),
            "relay-token".to_string(),
            "node-b".to_string(),
        ))
    );

    let preserved = store
        .save_relay_settings(RelaySettingsRequest {
            enabled: false,
            url: None,
            token: Some("   ".to_string()),
            node_id: None,
        })
        .await
        .expect("preserve relay token for blank update");
    assert!(!preserved.enabled);
    assert!(preserved.has_token);
    assert_eq!(preserved.url, "http://127.0.0.1:8090");
    assert_eq!(preserved.node_id, "mai-server");
}

#[tokio::test]
async fn github_app_settings_persist_public_url() {
    let (_dir, store) = store().await;

    store
        .save_github_app_settings(GithubAppSettingsRequest {
            app_id: Some("123".to_string()),
            private_key: Some("pem".to_string()),
            base_url: Some("https://api.github.com/".to_string()),
            public_url: Some(" https://relay.example/ ".to_string()),
        })
        .await
        .expect("save github app");
    let saved = store
        .save_github_app_identity(GithubAppIdentity {
            github_name: "Mai".to_string(),
            app_slug: "mai".to_string(),
            app_html_url: "https://github.com/apps/mai".to_string(),
            owner_login: Some("owner".to_string()),
            owner_type: Some("User".to_string()),
            bot_login: "mai[bot]".to_string(),
            bot_user_id: 42,
        })
        .await
        .expect("save GitHub App identity");

    assert_eq!(saved.public_url.as_deref(), Some("https://relay.example"));
    assert!(saved.has_private_key);
    assert_eq!(
        saved.install_url.as_deref(),
        Some("https://github.com/apps/mai/installations/select_target")
    );

    let secret = store
        .github_app_secret()
        .await
        .expect("secret")
        .expect("configured");
    assert_eq!(secret.0, "123");
    assert_eq!(secret.1, "pem");
    assert_eq!(secret.2, "https://api.github.com");

    let updated = store
        .save_github_app_settings(GithubAppSettingsRequest {
            app_id: Some("456".to_string()),
            private_key: None,
            base_url: Some("https://github.example/api/v3".to_string()),
            public_url: Some("https://relay-two.example".to_string()),
        })
        .await
        .expect("update GitHub App without private key");
    assert!(updated.has_private_key);
    assert_eq!(
        store
            .github_app_secret()
            .await
            .expect("load updated secret"),
        Some((
            "456".to_string(),
            "pem".to_string(),
            "https://github.example/api/v3".to_string(),
        ))
    );

    let blank_private_key = store
        .save_github_app_settings(GithubAppSettingsRequest {
            app_id: Some("789".to_string()),
            private_key: Some(" \n ".to_string()),
            base_url: None,
            public_url: None,
        })
        .await
        .expect("update GitHub App preserving blank private key");
    assert!(blank_private_key.has_private_key);
    assert_eq!(
        store
            .github_app_secret()
            .await
            .expect("load secret after blank private key"),
        Some((
            "789".to_string(),
            "pem".to_string(),
            "https://github.example/api/v3".to_string(),
        ))
    );
}

#[tokio::test]
async fn git_account_delete_wins_over_late_verification_update() {
    let (_dir, store) = store().await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");

    let response = store
        .delete_git_account("account-1")
        .await
        .expect("delete account");
    assert!(response.accounts.is_empty());
    assert_eq!(response.default_account_id, None);

    let late_update = store
        .update_git_account_verification(
            "account-1",
            Some("octo".to_string()),
            GitTokenKind::Classic,
            vec!["repo".to_string()],
            GitAccountStatus::Verified,
            None,
        )
        .await;
    assert!(late_update.is_err());

    let response = store.list_git_accounts().await.expect("list accounts");
    assert!(response.accounts.is_empty());
    assert_eq!(response.default_account_id, None);
}

#[tokio::test]
async fn product_event_replay_and_snapshot_keep_recent_events() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();

    for sequence in 1..=5 {
        store
            .append_product_event(&test_product_event(sequence, agent_id, Utc::now()))
            .await
            .expect("append event");
    }

    let replay = store.product_events_after(2, 2).await.expect("replay");
    assert_eq!(
        replay
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );

    let snapshot = store.load_runtime_snapshot(2).await.expect("snapshot");
    assert_eq!(snapshot.next_sequence, 6);
    assert_eq!(
        snapshot
            .recent_events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );
}

#[tokio::test]
async fn product_event_count_pruning_keeps_newest_events() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();

    for sequence in 1..=5 {
        store
            .append_product_event(&test_product_event(sequence, agent_id, Utc::now()))
            .await
            .expect("append event");
    }

    let removed = store
        .prune_product_events_to_limit(3)
        .await
        .expect("prune by limit");
    assert_eq!(removed, 2);

    let replay = store.product_events_after(0, 10).await.expect("replay");
    assert_eq!(
        replay
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
    assert_eq!(
        store.prune_product_events_to_limit(3).await.expect("noop"),
        0
    );
    assert_eq!(
        store
            .prune_product_events_to_limit(0)
            .await
            .expect("zero limit"),
        3
    );
    assert!(
        store
            .product_events_after(0, 10)
            .await
            .expect("empty replay")
            .is_empty()
    );
}

#[tokio::test]
async fn product_event_retention_never_exceeds_configured_batch() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let timestamp = Utc::now() - chrono::TimeDelta::days(30);
    for sequence in 1..=501 {
        store
            .append_product_event(&test_product_event(sequence, agent_id, timestamp))
            .await
            .expect("append event");
    }

    assert_eq!(
        500,
        store
            .prune_product_events_before_batch(Utc::now(), 500)
            .await
            .expect("first retention batch")
    );
    assert_eq!(
        1,
        store
            .prune_product_events_before_batch(Utc::now(), 500)
            .await
            .expect("second retention batch")
    );
}

#[tokio::test]
async fn project_review_runs_round_trip_and_prune() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let reviewer_agent_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4();
    let started_at = Utc::now() - chrono::TimeDelta::days(1);
    let finished_at = started_at + chrono::TimeDelta::minutes(3);
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: run_id,
                job_id: None,
                attempt_index: 1,
                project_id,
                reviewer_agent_id: Some(reviewer_agent_id),
                turn_id: Some(turn_id.clone()),
                started_at,
                finished_at: Some(finished_at),
                status: ProjectReviewRunStatus::Completed,
                outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
                review_event: Some(ProjectReviewDecision::Approve),
                pr: Some(42),
                summary: Some("approved".to_string()),
                error: None,
                failure: None,
                token_usage: TokenUsage {
                    prompt_tokens: 100,
                    cached_prompt_tokens: 60,
                    cache_write_tokens: 0,
                    completion_tokens: 20,
                    reasoning_tokens: 5,
                    total_tokens: 120,
                },
                history_status: Default::default(),
                history_archive_id: None,
                history_archived_at: None,
            },
            history: Some(ThreadTurnHistory {
                turn: completed_turn(
                    turn_id.clone(),
                    reviewer_agent_id.to_string(),
                    started_at.timestamp_millis(),
                    finished_at.timestamp_millis(),
                ),
                items: vec![completed_final_item(
                    "item-1",
                    reviewer_agent_id.to_string(),
                    turn_id.clone(),
                    "done",
                    finished_at.timestamp_millis(),
                )],
                context_disposition: ThreadContextDisposition::Active,
            }),
        })
        .await
        .expect("save run");

    let runs = store
        .load_project_review_runs(project_id, None, 0, 10)
        .await
        .expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].pr, Some(42));
    assert_eq!(runs[0].outcome, Some(ProjectReviewOutcome::ReviewSubmitted));
    assert_eq!(runs[0].review_event, Some(ProjectReviewDecision::Approve));
    assert_eq!(
        runs[0].token_usage,
        TokenUsage {
            prompt_tokens: 100,
            cached_prompt_tokens: 60,
            cache_write_tokens: 0,
            completion_tokens: 20,
            reasoning_tokens: 5,
            total_tokens: 120,
        }
    );
    let detail = store
        .load_project_review_run(project_id, run_id)
        .await
        .expect("detail")
        .expect("run exists");
    let text = detail.history.as_ref().expect("archived history").items[0]
        .text()
        .expect("final text item");
    assert_eq!(text.channel(), ThreadTextChannel::Final);
    assert_eq!(text.text(), "done");

    let connection = rusqlite::Connection::open(store.path()).expect("open sqlite");
    connection
        .execute(
            "UPDATE project_review_runs SET history_json = X'80' \
             WHERE id = ?1",
            rusqlite::params![run_id.to_string()],
        )
        .expect("replace detail payloads with non-text blobs");
    let summaries = store
        .load_project_review_runs(project_id, None, 0, 10)
        .await
        .expect("summary listing must not read detail payloads");
    assert_eq!(
        serde_json::to_value(&summaries).expect("serialize summaries"),
        serde_json::to_value(&runs).expect("serialize original summaries")
    );
    connection
        .execute(
            "UPDATE project_review_runs SET history_json = 'not-json' \
             WHERE id = ?1",
            rusqlite::params![run_id.to_string()],
        )
        .expect("replace detail payload with invalid JSON text");
    assert!(
        store
            .load_project_review_run(project_id, run_id)
            .await
            .is_err(),
        "detail loading must still read and validate its payload columns"
    );

    let removed = store
        .prune_orphan_project_review_runs_before(Utc::now() - chrono::TimeDelta::days(2))
        .await
        .expect("no prune");
    assert_eq!(removed, 0);
    let removed = store
        .prune_orphan_project_review_runs_before(Utc::now())
        .await
        .expect("prune");
    assert_eq!(removed, 1);
    assert!(
        store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load")
            .is_none()
    );
}

#[tokio::test]
async fn review_run_retention_removes_reference_to_missing_job() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let finished_at = Utc::now() - chrono::TimeDelta::days(8);
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: run_id,
                job_id: Some(Uuid::new_v4()),
                attempt_index: 1,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at: finished_at - chrono::TimeDelta::minutes(1),
                finished_at: Some(finished_at),
                status: ProjectReviewRunStatus::Failed,
                outcome: Some(ProjectReviewOutcome::Failed),
                review_event: None,
                pr: Some(42),
                summary: None,
                error: Some("missing job".to_string()),
                failure: None,
                token_usage: TokenUsage::default(),
                history_status: Default::default(),
                history_archive_id: None,
                history_archived_at: None,
            },
            history: None,
        })
        .await
        .expect("save dangling run");

    assert_eq!(
        1,
        store
            .prune_orphan_project_review_runs_before(Utc::now())
            .await
            .expect("prune dangling run")
    );
    assert!(
        store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load dangling run")
            .is_none()
    );
}

#[tokio::test]
async fn agent_logs_round_trip_filter_and_prune() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4().to_string();
    let turn_id = Uuid::new_v4().to_string();
    let old_time = Utc::now() - chrono::TimeDelta::days(6);
    let new_time = Utc::now();

    store
        .append_agent_log_entry(&AgentLogEntry {
            id: Uuid::new_v4(),
            agent_id,
            thread_id: Some(thread_id.clone()),
            turn_id: Some(turn_id.clone()),
            level: "info".to_string(),
            category: "tool".to_string(),
            message: "tool started".to_string(),
            details: json!({ "call_id": "call_1" }),
            timestamp: new_time,
        })
        .await
        .expect("save new log");
    store
        .append_agent_log_entry(&AgentLogEntry {
            id: Uuid::new_v4(),
            agent_id,
            thread_id: None,
            turn_id: None,
            level: "warn".to_string(),
            category: "model".to_string(),
            message: "old".to_string(),
            details: json!({}),
            timestamp: old_time,
        })
        .await
        .expect("save old log");

    let logs = store
        .list_agent_logs(
            agent_id,
            AgentLogFilter {
                thread_id: Some(thread_id),
                turn_id: Some(turn_id),
                level: Some("info".to_string()),
                category: Some("tool".to_string()),
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("list logs");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "tool started");
    assert_eq!(logs[0].details["call_id"], "call_1");

    let removed = store
        .prune_agent_logs_before(Utc::now() - chrono::TimeDelta::days(5))
        .await
        .expect("prune logs");
    assert_eq!(removed, 1);
    let remaining = store
        .list_agent_logs(
            agent_id,
            AgentLogFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("remaining logs");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].category, "tool");
}

#[tokio::test]
async fn agent_log_retention_preserves_rows_owned_by_a_live_thread() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4().to_string();
    let timestamp = Utc::now() - chrono::TimeDelta::days(30);
    let connection = rusqlite::Connection::open(&store.path).expect("open sqlite");
    connection
        .execute(
            "INSERT INTO thread_runtime_documents (
                thread_id, revision, document_json, snapshot_json, updated_at
             ) VALUES (?1, 0, '{}', NULL, 0)",
            rusqlite::params![thread_id],
        )
        .expect("insert live thread");
    drop(connection);
    for observed_thread_id in [Some(thread_id.clone()), None] {
        store
            .append_agent_log_entry(&AgentLogEntry {
                id: Uuid::new_v4(),
                agent_id,
                thread_id: observed_thread_id,
                turn_id: None,
                level: "info".to_string(),
                category: "retention".to_string(),
                message: "old".to_string(),
                details: json!({}),
                timestamp,
            })
            .await
            .expect("append log");
    }

    assert_eq!(
        1,
        store
            .prune_agent_logs_before_batch(Utc::now(), 500)
            .await
            .expect("prune")
    );
    let remaining = store
        .list_agent_logs(
            agent_id,
            AgentLogFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("remaining");
    assert_eq!(1, remaining.len());
    assert_eq!(Some(thread_id), remaining[0].thread_id);
}

#[tokio::test]
async fn agent_log_reads_do_not_wait_for_unrelated_busy_store_operation() {
    let (_dir, store) = store().await;
    let _busy_connection = store.db.connection().await.expect("busy connection");

    let logs = timeout(
        Duration::from_millis(200),
        store.list_agent_logs(
            Uuid::new_v4(),
            AgentLogFilter {
                limit: 10,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("log read should not wait behind an unrelated store operation")
    .expect("list logs");

    assert!(logs.is_empty());
}

#[tokio::test]
async fn tool_traces_round_trip_filter_and_prune() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4().to_string();
    let turn_id = Uuid::new_v4().to_string();
    let old_time = Utc::now() - chrono::TimeDelta::days(6);
    let new_time = Utc::now();

    let trace = ToolTraceDetail {
        agent_id,
        thread_id: Some(thread_id.clone()),
        turn_id: Some(turn_id.clone()),
        call_id: "call_1".to_string(),
        tool_name: "exec".to_string(),
        arguments: json!({ "command": "printf hi" }),
        output: r#"{"status":0,"stdout":"hi","stderr":""}"#.to_string(),
        success: true,
        duration_ms: Some(42),
        started_at: Some(new_time),
        completed_at: Some(new_time),
        output_preview: "hi".to_string(),
        output_artifacts: vec![ToolOutputArtifactInfo {
            id: "artifact-1".to_string(),
            call_id: "call_1".to_string(),
            agent_id,
            name: "stdout.txt".to_string(),
            stream: "stdout".to_string(),
            size_bytes: 2,
            created_at: new_time,
        }],
    };
    store
        .save_tool_trace_started(&trace, new_time)
        .await
        .expect("save start");
    store
        .save_tool_trace_completed(&trace, new_time, new_time)
        .await
        .expect("save completed");
    store
        .save_tool_trace_completed(
            &ToolTraceDetail {
                call_id: "call_old".to_string(),
                started_at: Some(old_time),
                completed_at: Some(old_time),
                output_preview: "old".to_string(),
                ..trace.clone()
            },
            old_time,
            old_time,
        )
        .await
        .expect("save old");

    let summaries = store
        .list_tool_traces(
            agent_id,
            ToolTraceFilter {
                thread_id: Some(thread_id.clone()),
                turn_id: Some(turn_id),
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("list traces");
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].call_id, "call_1");
    assert_eq!(summaries[0].duration_ms, Some(42));

    let loaded = store
        .load_tool_trace(agent_id, Some(thread_id), "call_1")
        .await
        .expect("load trace")
        .expect("trace");
    assert_eq!(loaded.arguments["command"], "printf hi");
    assert_eq!(loaded.output_preview, "hi");
    assert_eq!(loaded.output_artifacts.len(), 1);
    assert_eq!(loaded.output_artifacts[0].stream, "stdout");

    let removed = store
        .prune_tool_traces_before(Utc::now() - chrono::TimeDelta::days(5))
        .await
        .expect("prune traces");
    assert_eq!(removed, 1);
    let remaining = store
        .list_tool_traces(
            agent_id,
            ToolTraceFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("remaining traces");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].call_id, "call_1");
}

#[tokio::test]
async fn tool_traces_keep_same_call_id_for_different_agents() {
    let (_dir, store) = store().await;
    let first_agent_id = Uuid::new_v4();
    let second_agent_id = Uuid::new_v4();
    let first_thread_id = Uuid::new_v4().to_string();
    let second_thread_id = Uuid::new_v4().to_string();
    let timestamp = Utc::now();

    for (agent_id, thread_id, command) in [
        (first_agent_id, first_thread_id.clone(), "pwd"),
        (second_agent_id, second_thread_id.clone(), "ls"),
    ] {
        store
            .save_tool_trace_completed(
                &ToolTraceDetail {
                    agent_id,
                    thread_id: Some(thread_id),
                    turn_id: Some(Uuid::new_v4().to_string()),
                    call_id: "call_duplicate".to_string(),
                    tool_name: "exec".to_string(),
                    arguments: json!({ "command": command }),
                    output: format!("{{\"command\":\"{command}\"}}"),
                    success: true,
                    duration_ms: Some(1),
                    started_at: Some(timestamp),
                    completed_at: Some(timestamp),
                    output_preview: command.to_string(),
                    output_artifacts: Vec::new(),
                },
                timestamp,
                timestamp,
            )
            .await
            .expect("save trace");
    }

    let first = store
        .load_tool_trace(first_agent_id, Some(first_thread_id), "call_duplicate")
        .await
        .expect("load first")
        .expect("first trace");
    let second = store
        .load_tool_trace(second_agent_id, Some(second_thread_id), "call_duplicate")
        .await
        .expect("load second")
        .expect("second trace");

    assert_eq!(first.arguments["command"], "pwd");
    assert_eq!(second.arguments["command"], "ls");
}

#[tokio::test]
async fn delete_project_removes_review_runs() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let maintainer_agent_id = Uuid::new_v4();
    let timestamp = Utc::now();
    store
        .save_project(&ProjectSummary {
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
            docker_image: "ubuntu:latest".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id,
            created_at: timestamp,
            updated_at: timestamp,
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Waiting,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        })
        .await
        .expect("save project");
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: Uuid::new_v4(),
                job_id: None,
                attempt_index: 1,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at: timestamp,
                finished_at: None,
                status: ProjectReviewRunStatus::Syncing,
                outcome: None,
                review_event: None,
                pr: None,
                summary: None,
                error: None,
                failure: None,
                token_usage: TokenUsage::default(),
                history_status: Default::default(),
                history_archive_id: None,
                history_archived_at: None,
            },
            history: None,
        })
        .await
        .expect("save run");
    store
        .delete_project(project_id)
        .await
        .expect("delete project");
    assert!(
        store
            .load_project_review_runs(project_id, None, 0, 10)
            .await
            .expect("runs")
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_sqlite_file_is_rejected_without_overwrite() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config.sqlite3");
    std::fs::write(&path, b"not sqlite").expect("write invalid old db");
    let error = MaiStore::open_with_config_path(&path, dir.path().join("config.toml"))
        .await
        .err()
        .expect("invalid database must be rejected");
    assert!(error.to_string().contains("拒绝覆盖"));
    assert_eq!(
        std::fs::read(path).expect("database remains"),
        b"not sqlite"
    );
}

#[tokio::test]
async fn sqlite_store_uses_wal_journal_mode() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let store = MaiStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
        .await
        .expect("open");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("journal_mode");
    assert_eq!("wal", journal_mode.to_ascii_lowercase());
}

#[tokio::test]
async fn skills_config_persists_in_settings() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = MaiStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open");
    let config = SkillsConfigRequest {
        disabled: vec!["demo".to_string()],
    };
    store
        .save_skills_config(&config)
        .await
        .expect("save skills config");
    drop(store);

    let reopened = MaiStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("reopen");
    assert_eq!(
        reopened
            .load_skills_config()
            .await
            .expect("load skills config"),
        config
    );
    assert!(
        !config_path.exists(),
        "provider config file should be untouched"
    );
}

#[tokio::test]
async fn schema_version_mismatch_is_rejected_without_data_loss() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = MaiStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open");
    store
        .save_mcp_servers(&BTreeMap::from([(
            "demo".to_string(),
            McpServerConfig {
                command: Some("demo-mcp".to_string()),
                ..Default::default()
            },
        )]))
        .await
        .expect("save server");
    store
        .set_setting(SETTING_SCHEMA_VERSION, "4")
        .await
        .expect("mark old schema");
    drop(store);

    let error = MaiStore::open_with_config_path(&db_path, &config_path)
        .await
        .err()
        .expect("old schema must be rejected");
    assert!(error.to_string().contains("mai-migrate"));
    let connection = rusqlite::Connection::open(db_path).expect("inspect preserved database");
    let preserved: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM mcp_servers WHERE name = 'demo'",
            [],
            |row| row.get(0),
        )
        .expect("preserved row");
    assert_eq!(preserved, 1);
}

#[tokio::test]
async fn mcp_servers_round_trip_json_config() {
    let (_dir, store) = store().await;
    let servers = BTreeMap::from([
        (
            "stdio".to_string(),
            McpServerConfig {
                scope: McpServerScope::Project,
                command: Some("demo-mcp".to_string()),
                args: vec!["--stdio".to_string()],
                env: BTreeMap::from([("A".to_string(), "B".to_string())]),
                cwd: Some("/workspace".to_string()),
                enabled_tools: Some(vec!["echo".to_string()]),
                disabled_tools: vec!["danger".to_string()],
                startup_timeout_secs: Some(3),
                tool_timeout_secs: Some(7),
                ..Default::default()
            },
        ),
        (
            "http".to_string(),
            McpServerConfig {
                transport: McpServerTransport::StreamableHttp,
                url: Some("https://example.com/mcp".to_string()),
                headers: BTreeMap::from([("X-Test".to_string(), "yes".to_string())]),
                bearer_token_env: Some("MCP_TOKEN".to_string()),
                enabled: false,
                required: true,
                ..Default::default()
            },
        ),
    ]);

    store.save_mcp_servers(&servers).await.expect("save");
    let loaded = store.list_mcp_servers().await.expect("load");

    assert_eq!(loaded, servers);
    assert_eq!(
        loaded.get("stdio").map(|config| config.scope),
        Some(McpServerScope::Project)
    );
}

#[tokio::test]
async fn review_jobs_dedupe_same_head_and_supersede_old_head() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let first = test_review_job(project_id, 42, "head-a", None);

    let queued = store
        .enqueue_project_review_job(first.clone())
        .await
        .expect("enqueue first");
    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Queued,
        queued.disposition
    );
    let deduped = store
        .enqueue_project_review_job(test_review_job(project_id, 42, "head-a", None))
        .await
        .expect("dedupe same head");
    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Deduped,
        deduped.disposition
    );
    let claimed_at = Utc::now();
    let lease_expires_at = claimed_at + chrono::TimeDelta::seconds(60);
    store
        .claim_due_project_review_job(
            project_id,
            "old-head-owner".to_string(),
            claimed_at,
            lease_expires_at,
        )
        .await
        .expect("claim old head")
        .expect("old head is due");
    let replacement = store
        .enqueue_project_review_job(test_review_job(project_id, 42, "head-b", None))
        .await
        .expect("enqueue new head");
    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Queued,
        replacement.disposition
    );

    let jobs = store
        .load_project_pull_request_review_history(project_id, 42, 1, 10)
        .await
        .expect("load jobs")
        .items
        .into_iter()
        .map(|item| item.job)
        .collect::<Vec<_>>();
    assert_eq!(2, jobs.len());
    assert_eq!(ProjectReviewJobStatus::Queued, jobs[0].status);
    assert_eq!("head-b", jobs[0].head_sha);
    assert_eq!(ProjectReviewJobStatus::Superseded, jobs[1].status);
    assert_eq!(Some("old-head-owner"), jobs[1].lease_owner.as_deref());
    assert!(
        store
            .claim_due_project_review_job(
                project_id,
                "new-head-owner".to_string(),
                claimed_at + chrono::TimeDelta::seconds(1),
                claimed_at + chrono::TimeDelta::seconds(61),
            )
            .await
            .expect("claim while old reviewer stops")
            .is_none()
    );
    let new_head = store
        .claim_due_project_review_job(
            project_id,
            "new-head-owner".to_string(),
            lease_expires_at + chrono::TimeDelta::milliseconds(1),
            lease_expires_at + chrono::TimeDelta::seconds(60),
        )
        .await
        .expect("claim after old lease expires")
        .expect("new head is due");
    assert_eq!("head-b", new_head.head_sha);
}

#[tokio::test]
async fn review_discovery_admission_rolls_back_the_whole_batch() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut failed = test_review_job(project_id, 40, "head-40", None);
    failed.status = ProjectReviewJobStatus::Failed;
    failed.attempt_count = 1;
    failed.first_retryable_failure_at = Some(Utc::now() - chrono::TimeDelta::hours(1));
    failed.next_attempt_at = None;
    failed.finished_at = Some(Utc::now());
    let failed_id = failed.id;
    store
        .save_project_review_job(failed)
        .await
        .expect("save recoverable failed job");
    let retry = test_review_job(project_id, 40, "head-40", None);
    let valid = test_review_job(project_id, 41, "head-41", None);
    let invalid = test_review_job(project_id, 0, "", None);

    let error = store
        .admit_project_review_discovery(vec![retry, valid, invalid], Vec::new())
        .await
        .expect_err("invalid candidate must roll back recovery and insertion");

    assert!(
        error
            .to_string()
            .contains("requires a PR number and head SHA")
    );
    assert_eq!(
        None,
        store
            .load_active_project_review_job_for_pr(project_id, 41)
            .await
            .expect("load rolled back job")
    );
    let failed = store
        .load_project_review_job(project_id, failed_id)
        .await
        .expect("load rolled back failed job")
        .expect("failed job remains durable");
    assert_eq!(ProjectReviewJobStatus::Failed, failed.status);
    assert_eq!(1, failed.attempt_count);
}

#[tokio::test]
async fn review_discovery_admission_batches_jobs_and_ci_watches() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let now = Utc::now();
    let watch = ProjectReviewCiWatch {
        project_id,
        pr: 43,
        head_sha: "head-43".to_string(),
        delivery_id: None,
        reason: "discovery_ci_pending".to_string(),
        next_check_at: now + chrono::TimeDelta::minutes(1),
        created_at: now,
        updated_at: now,
    };

    let initial = store
        .admit_project_review_discovery(
            vec![test_review_job(project_id, 42, "head-a", None)],
            vec![watch.clone()],
        )
        .await
        .expect("admit initial discovery batch");
    assert_eq!(
        vec![42],
        initial.queued.iter().map(|job| job.pr).collect::<Vec<_>>()
    );
    assert_eq!(vec![43], initial.watched);
    assert_eq!(
        Some(watch),
        store
            .load_project_review_ci_watch(project_id, 43)
            .await
            .expect("load discovery watch")
    );

    let repeated = store
        .admit_project_review_discovery(
            vec![test_review_job(project_id, 42, "head-a", None)],
            Vec::new(),
        )
        .await
        .expect("dedupe repeated discovery batch");
    assert_eq!(Vec::<ProjectReviewJobSummary>::new(), repeated.queued);
    assert_eq!(
        vec![42],
        repeated
            .deduped
            .iter()
            .map(|job| job.pr)
            .collect::<Vec<_>>()
    );

    let new_head = store
        .admit_project_review_discovery(
            vec![test_review_job(project_id, 42, "head-b", None)],
            Vec::new(),
        )
        .await
        .expect("admit new head");
    assert_eq!(
        vec!["head-b"],
        new_head
            .queued
            .iter()
            .map(|job| job.head_sha.as_str())
            .collect::<Vec<_>>()
    );
    let history = store
        .load_project_pull_request_review_history(project_id, 42, 1, 10)
        .await
        .expect("load discovery history");
    assert_eq!(ProjectReviewJobStatus::Queued, history.items[0].job.status);
    assert_eq!(
        ProjectReviewJobStatus::Superseded,
        history.items[1].job.status
    );
}

#[tokio::test]
async fn review_discovery_requeues_same_logical_failed_job_until_attempts_exhausted() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut failed = test_review_job(project_id, 44, "failed-head", None);
    failed.status = ProjectReviewJobStatus::Failed;
    failed.attempt_count = 1;
    failed.next_attempt_at = None;
    failed.reviewer_agent_id = Some(reviewer_id);
    failed.failure = Some(ProjectReviewFailure {
        category: ProjectReviewFailureCategory::Provider,
        code: None,
        http_status: Some(401),
        message: "provider credentials were rejected".to_string(),
        retry: pl_protocol::RetryDisposition::Permanent,
    });
    failed.finished_at = Some(Utc::now());
    let failed_id = failed.id;
    let mut expected = failed.clone();
    store
        .save_project_review_job(failed)
        .await
        .expect("save failed job with attempts remaining");
    let candidate = test_review_job(project_id, 44, "failed-head", None);
    expected.status = ProjectReviewJobStatus::Queued;
    expected.delivery_id = candidate.delivery_id.clone().or(expected.delivery_id);
    expected.reason = candidate.reason.clone();
    expected.first_retryable_failure_at = None;
    expected.next_attempt_at = Some(candidate.created_at);
    expected.reviewer_agent_id = None;
    expected.active_run_id = None;
    expected.lease_owner = None;
    expected.lease_expires_at = None;
    expected.failure = None;
    expected.environment_warning = None;
    expected.skip_reason = None;
    expected.updated_at = candidate.updated_at;
    expected.finished_at = None;

    let admission = store
        .admit_project_review_discovery(vec![candidate], Vec::new())
        .await
        .expect("requeue failed job with attempts remaining");

    assert_eq!(Vec::<u64>::new(), admission.suppressed);
    assert_eq!(Vec::<ProjectReviewJobSummary>::new(), admission.deduped);
    assert_eq!(
        vec![failed_id],
        admission
            .queued
            .iter()
            .map(|job| job.id)
            .collect::<Vec<_>>()
    );
    let requeued = admission.queued.into_iter().next().expect("requeued job");
    assert_eq!(expected, requeued);
    let history = store
        .load_project_pull_request_review_history(project_id, 44, 1, 10)
        .await
        .expect("load requeued job history");
    assert_eq!(1, history.total_items);
    assert_eq!(failed_id, history.items[0].job.id);

    let claimed_at = requeued.next_attempt_at.expect("requeued due time");
    let owner = "discovery-retry-worker".to_string();
    let claimed = store
        .claim_due_project_review_job(
            project_id,
            owner.clone(),
            claimed_at,
            claimed_at + chrono::TimeDelta::minutes(1),
        )
        .await
        .expect("claim requeued job")
        .expect("requeued job is due");
    assert_eq!(failed_id, claimed.id);
    let started = store
        .begin_claimed_project_review_attempt(failed_id, owner, Uuid::new_v4(), claimed_at)
        .await
        .expect("begin next attempt on the same logical job");
    assert_eq!(2, started.attempt_count);
}

#[tokio::test]
async fn review_discovery_suppresses_exhausted_failed_head_only() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut failed = test_review_job(project_id, 44, "failed-head", None);
    failed.status = ProjectReviewJobStatus::Failed;
    failed.attempt_count = failed.max_attempts;
    failed.next_attempt_at = None;
    failed.finished_at = Some(Utc::now());
    store
        .save_project_review_job(failed)
        .await
        .expect("save exhausted failed job");

    let same_head = store
        .admit_project_review_discovery(
            vec![test_review_job(project_id, 44, "failed-head", None)],
            Vec::new(),
        )
        .await
        .expect("suppress failed head");
    assert_eq!(vec![44], same_head.suppressed);
    assert_eq!(Vec::<ProjectReviewJobSummary>::new(), same_head.queued);

    let new_head = store
        .admit_project_review_discovery(
            vec![test_review_job(project_id, 44, "new-head", None)],
            Vec::new(),
        )
        .await
        .expect("allow new head");
    assert_eq!(
        vec!["new-head"],
        new_head
            .queued
            .iter()
            .map(|job| job.head_sha.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn review_discovery_does_not_requeue_ambiguous_submission() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut failed = test_review_job(project_id, 45, "ambiguous-head", None);
    failed.status = ProjectReviewJobStatus::Failed;
    failed.attempt_count = 1;
    failed.next_attempt_at = None;
    failed.submission_intent = Some(ProjectReviewSubmissionIntent {
        job_id: failed.id,
        head_sha: failed.head_sha.clone(),
        event: ProjectReviewDecision::RequestChanges,
        body_hash: "sha256:ambiguous-review".to_string(),
        comment_count: 1,
        created_at: Utc::now(),
    });
    failed.finished_at = Some(Utc::now());
    let failed_id = failed.id;
    store
        .save_project_review_job(failed)
        .await
        .expect("save ambiguous failed job");

    let admission = store
        .admit_project_review_discovery(
            vec![test_review_job(project_id, 45, "ambiguous-head", None)],
            Vec::new(),
        )
        .await
        .expect("admit ambiguous failed head");

    assert_eq!(vec![45], admission.suppressed);
    assert_eq!(Vec::<ProjectReviewJobSummary>::new(), admission.queued);
    let persisted = store
        .load_project_review_job(project_id, failed_id)
        .await
        .expect("load ambiguous failed job")
        .expect("ambiguous failed job remains durable");
    assert_eq!(ProjectReviewJobStatus::Failed, persisted.status);
    assert!(persisted.submission_intent.is_some());
}

#[tokio::test]
async fn review_job_environment_warning_round_trips_independently_of_failure() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut job = test_review_job(project_id, 1520, "head-1520", None);
    job.environment_warning = Some(ProjectReviewEnvironmentWarning {
        code: "latest_image_refresh_failed".to_string(),
        image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
        cached_image_id: "sha256:cached".to_string(),
        message: "registry unavailable; using cached image".to_string(),
        observed_at: Utc::now(),
    });
    let job_id = job.id;

    store
        .save_project_review_job(job.clone())
        .await
        .expect("save warning");

    assert_eq!(
        Some(job),
        store
            .load_project_review_job(project_id, job_id)
            .await
            .expect("load warning")
    );
}

#[tokio::test]
async fn completed_review_signals_keep_one_active_job_per_pr() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let first = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            42,
            "head-42",
            Some("delivery-1"),
        ))
        .await
        .expect("enqueue first completed signal");
    let repeated = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            42,
            "head-42",
            Some("delivery-2"),
        ))
        .await
        .expect("dedupe second completed signal");

    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Queued,
        first.disposition
    );
    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Deduped,
        repeated.disposition
    );
    assert_eq!(first.job.id, repeated.job.id);
    assert_eq!(Some("delivery-2"), repeated.job.delivery_id.as_deref());
    let jobs = store
        .load_project_pull_request_review_history(project_id, 42, 1, 10)
        .await
        .expect("load review jobs")
        .items
        .into_iter()
        .map(|item| item.job)
        .collect::<Vec<_>>();
    assert_eq!(1, jobs.len());
    assert_eq!(
        1,
        jobs.iter().filter(|job| !job.status.is_terminal()).count()
    );
}

#[tokio::test]
async fn completed_review_signal_recovers_pr_from_persisted_head() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    store
        .enqueue_project_review_job(test_review_job(
            project_id,
            1705,
            "head-1705",
            Some("synchronize-delivery"),
        ))
        .await
        .expect("persist synchronize job");
    store
        .enqueue_project_review_job(test_review_job(
            project_id,
            1706,
            "different-head",
            Some("other-delivery"),
        ))
        .await
        .expect("persist unrelated job");

    assert_eq!(
        vec![1705],
        store
            .load_project_review_prs_for_head(project_id, "head-1705".to_string())
            .await
            .expect("recover PR from head")
    );
}

#[tokio::test]
async fn review_ci_watch_persists_latest_head_and_recovers_pr_mapping() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let now = Utc::now();
    let initial = ProjectReviewCiWatch {
        project_id,
        pr: 1520,
        head_sha: "old-head".to_string(),
        delivery_id: Some("delivery-1".to_string()),
        reason: "synchronize".to_string(),
        next_check_at: now,
        created_at: now,
        updated_at: now,
    };
    store
        .upsert_project_review_ci_watch(initial)
        .await
        .expect("persist initial CI watch");
    let updated = ProjectReviewCiWatch {
        project_id,
        pr: 1520,
        head_sha: "current-head".to_string(),
        delivery_id: Some("delivery-2".to_string()),
        reason: "new synchronize".to_string(),
        next_check_at: now + chrono::TimeDelta::seconds(60),
        created_at: now + chrono::TimeDelta::seconds(1),
        updated_at: now + chrono::TimeDelta::seconds(1),
    };
    store
        .upsert_project_review_ci_watch(updated.clone())
        .await
        .expect("replace CI watch head");
    store
        .upsert_project_review_ci_watch(ProjectReviewCiWatch {
            project_id,
            pr: 1520,
            head_sha: "current-head".to_string(),
            delivery_id: Some("delayed-old-delivery".to_string()),
            reason: "delayed old synchronize".to_string(),
            next_check_at: now + chrono::TimeDelta::seconds(120),
            created_at: now,
            updated_at: now + chrono::TimeDelta::seconds(2),
        })
        .await
        .expect("merge same-head delayed signal");

    let persisted = store
        .load_project_review_ci_watch(project_id, 1520)
        .await
        .expect("load CI watch")
        .expect("CI watch exists");
    assert_eq!(updated.head_sha, persisted.head_sha);
    assert_eq!(updated.delivery_id, persisted.delivery_id);
    assert_eq!(updated.reason, persisted.reason);
    assert_eq!(now, persisted.created_at);
    assert_eq!(
        vec![1520],
        store
            .load_project_review_prs_for_head(project_id, "current-head".to_string())
            .await
            .expect("recover watched PR from head")
    );
    assert!(
        store
            .load_project_review_prs_for_head(project_id, "old-head".to_string())
            .await
            .expect("old head has no PR mapping")
            .is_empty()
    );
}

#[tokio::test]
async fn review_ci_watch_updates_use_expected_head_cas() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let now = Utc::now();
    store
        .upsert_project_review_ci_watch(ProjectReviewCiWatch {
            project_id,
            pr: 42,
            head_sha: "current-head".to_string(),
            delivery_id: None,
            reason: "synchronize".to_string(),
            next_check_at: now,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("persist CI watch");

    assert!(
        !store
            .reschedule_project_review_ci_watch(
                project_id,
                42,
                "stale-head".to_string(),
                now + chrono::TimeDelta::seconds(60),
                now,
            )
            .await
            .expect("stale reschedule")
    );
    assert!(
        !store
            .replace_project_review_ci_watch_head(
                project_id,
                42,
                "stale-head".to_string(),
                "replacement-head".to_string(),
                now + chrono::TimeDelta::seconds(60),
                now,
            )
            .await
            .expect("stale head replacement")
    );
    assert!(
        !store
            .delete_project_review_ci_watch(project_id, 42, "stale-head".to_string())
            .await
            .expect("stale delete")
    );
    assert!(
        store
            .delete_project_review_ci_watch(project_id, 42, "current-head".to_string())
            .await
            .expect("current delete")
    );
}

#[tokio::test]
async fn stale_ci_watch_cannot_supersede_newer_head_job() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let now = Utc::now();
    let stale_watch = ProjectReviewCiWatch {
        project_id,
        pr: 1520,
        head_sha: "old-head".to_string(),
        delivery_id: Some("old-delivery".to_string()),
        reason: "old synchronize".to_string(),
        next_check_at: now,
        created_at: now,
        updated_at: now,
    };
    store
        .upsert_project_review_ci_watch(stale_watch.clone())
        .await
        .expect("persist old watch");
    store
        .upsert_project_review_ci_watch(ProjectReviewCiWatch {
            project_id,
            pr: 1520,
            head_sha: "new-head".to_string(),
            delivery_id: Some("new-delivery".to_string()),
            reason: "new synchronize".to_string(),
            next_check_at: now,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("replace watch with new head");
    let new_job = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            1520,
            "new-head",
            Some("new-completed-delivery"),
        ))
        .await
        .expect("enqueue new head");

    let result = store
        .enqueue_project_review_job_from_ci_watch(
            stale_watch,
            test_review_job(project_id, 1520, "old-head", Some("old-delivery")),
        )
        .await
        .expect("reject stale watch enqueue");
    assert!(matches!(
        result,
        ProjectReviewCiWatchEnqueueResult::SignalChanged
    ));
    assert_eq!(
        Some(new_job.job),
        store
            .load_active_project_review_job_for_pr(project_id, 1520)
            .await
            .expect("load active new head")
    );
}

#[tokio::test]
async fn watched_signal_does_not_replace_newer_delivery_on_same_head() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let now = Utc::now();
    let watch = ProjectReviewCiWatch {
        project_id,
        pr: 1520,
        head_sha: "current-head".to_string(),
        delivery_id: Some("old-synchronize".to_string()),
        reason: "old synchronize".to_string(),
        next_check_at: now,
        created_at: now,
        updated_at: now,
    };
    store
        .upsert_project_review_ci_watch(watch.clone())
        .await
        .expect("persist watch");
    let active = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            1520,
            "current-head",
            Some("new-completed"),
        ))
        .await
        .expect("enqueue completed signal");

    let watched = store
        .enqueue_project_review_job_from_ci_watch(
            watch,
            test_review_job(project_id, 1520, "current-head", Some("old-synchronize")),
        )
        .await
        .expect("dedupe watched signal");
    let ProjectReviewCiWatchEnqueueResult::Enqueued(watched) = watched else {
        panic!("watch should still match");
    };
    let watched = *watched;
    assert_eq!(active.job, watched.job);
    assert_eq!(Some("new-completed"), watched.job.delivery_id.as_deref());
}

#[tokio::test]
async fn old_job_preflight_cannot_overwrite_new_head_watch() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let old_job = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            1520,
            "old-head",
            Some("old-delivery"),
        ))
        .await
        .expect("enqueue old job");
    let now = Utc::now();
    store
        .claim_due_project_review_job(
            project_id,
            "worker-1".to_string(),
            now,
            now + chrono::TimeDelta::seconds(60),
        )
        .await
        .expect("claim old job")
        .expect("old job due");
    let new_watch = ProjectReviewCiWatch {
        project_id,
        pr: 1520,
        head_sha: "new-head".to_string(),
        delivery_id: Some("new-delivery".to_string()),
        reason: "new synchronize".to_string(),
        next_check_at: now,
        created_at: now,
        updated_at: now,
    };
    store
        .upsert_project_review_ci_watch(new_watch.clone())
        .await
        .expect("persist new head watch");

    assert_eq!(
        ProjectReviewCiPendingSkipResult::Skipped,
        store
            .skip_claimed_project_review_job_for_ci_pending(
                old_job.job.id,
                "worker-1".to_string(),
                Some("old-delivery".to_string()),
                now + chrono::TimeDelta::seconds(1),
                now + chrono::TimeDelta::seconds(61),
            )
            .await
            .expect("skip old job")
    );
    assert_eq!(
        Some(new_watch),
        store
            .load_project_review_ci_watch(project_id, 1520)
            .await
            .expect("load preserved new head watch")
    );
}

#[tokio::test]
async fn ci_pending_skip_rechecks_changed_delivery_and_allows_next_generation() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let first = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            42,
            "head-42",
            Some("delivery-1"),
        ))
        .await
        .expect("enqueue first completed signal");
    let now = Utc::now();
    store
        .claim_due_project_review_job(
            project_id,
            "worker-1".to_string(),
            now,
            now + chrono::TimeDelta::seconds(60),
        )
        .await
        .expect("claim review job")
        .expect("review job is due");
    let refreshed = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            42,
            "head-42",
            Some("delivery-2"),
        ))
        .await
        .expect("refresh active job delivery");
    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Deduped,
        refreshed.disposition
    );
    assert_eq!(
        ProjectReviewCiPendingSkipResult::SignalChanged,
        store
            .skip_claimed_project_review_job_for_ci_pending(
                first.job.id,
                "worker-1".to_string(),
                Some("delivery-1".to_string()),
                now + chrono::TimeDelta::seconds(1),
                now + chrono::TimeDelta::seconds(61),
            )
            .await
            .expect("compare stale delivery")
    );
    assert_eq!(
        ProjectReviewCiPendingSkipResult::Skipped,
        store
            .skip_claimed_project_review_job_for_ci_pending(
                first.job.id,
                "worker-1".to_string(),
                Some("delivery-2".to_string()),
                now + chrono::TimeDelta::seconds(2),
                now + chrono::TimeDelta::seconds(62),
            )
            .await
            .expect("skip unchanged delivery")
    );
    let watch = store
        .load_project_review_ci_watch(project_id, 42)
        .await
        .expect("load CI watch")
        .expect("CI watch persisted atomically with skip");
    assert_eq!("head-42", watch.head_sha);
    assert_eq!(None, watch.delivery_id);

    let next = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            42,
            "head-42",
            Some("delivery-3"),
        ))
        .await
        .expect("enqueue next generation");
    assert_eq!(ProjectReviewJobEnqueueDisposition::Queued, next.disposition);
    assert_ne!(first.job.id, next.job.id);
    let jobs = store
        .load_project_pull_request_review_history(project_id, 42, 1, 10)
        .await
        .expect("load review jobs")
        .items
        .into_iter()
        .map(|item| item.job)
        .collect::<Vec<_>>();
    assert_eq!(2, jobs.len());
    assert_eq!(
        1,
        jobs.iter().filter(|job| !job.status.is_terminal()).count()
    );
}

#[tokio::test]
async fn active_review_job_projection_does_not_hide_reviewer_owner() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut waiting = test_review_job(project_id, 41, "head-waiting", None);
    waiting.status = ProjectReviewJobStatus::RetryWaiting;
    waiting.reviewer_agent_id = Some(Uuid::new_v4());
    waiting.next_attempt_at = Some(Utc::now() + chrono::TimeDelta::minutes(2));
    store
        .save_project_review_job(waiting.clone())
        .await
        .expect("save waiting job");

    let mut queued = test_review_job(project_id, 42, "head-queued", None);
    queued.created_at += chrono::TimeDelta::seconds(1);
    queued.updated_at = queued.created_at;
    store
        .save_project_review_job(queued.clone())
        .await
        .expect("save queued job");

    assert_eq!(
        Some(queued.id),
        store
            .load_active_project_review_job(project_id)
            .await
            .expect("load active job")
            .map(|job| job.id)
    );
    assert_eq!(
        Some(waiting.id),
        store
            .load_reviewer_owned_active_project_review_job(project_id)
            .await
            .expect("load reviewer owner")
            .map(|job| job.id)
    );
}

#[tokio::test]
async fn delayed_review_retry_reserves_the_project_reviewer_slot() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let current_time = Utc::now();
    let mut waiting = test_review_job(project_id, 41, "head-waiting", None);
    waiting.status = ProjectReviewJobStatus::RetryWaiting;
    waiting.reviewer_agent_id = Some(Uuid::new_v4());
    waiting.next_attempt_at = Some(current_time + chrono::TimeDelta::minutes(2));
    store
        .save_project_review_job(waiting.clone())
        .await
        .expect("save waiting job");

    let mut queued = test_review_job(project_id, 42, "head-queued", None);
    queued.created_at += chrono::TimeDelta::seconds(1);
    queued.updated_at = queued.created_at;
    queued.next_attempt_at = Some(current_time);
    let queued_id = queued.id;
    store
        .save_project_review_job(queued)
        .await
        .expect("save queued job");

    let claimed = store
        .claim_due_project_review_job(
            project_id,
            "queued-owner".to_string(),
            current_time,
            current_time + chrono::TimeDelta::seconds(60),
        )
        .await
        .expect("check project reviewer reservation");
    assert_eq!(None, claimed);

    let retry_time = current_time + chrono::TimeDelta::minutes(2);
    let claimed = store
        .claim_due_project_review_job(
            project_id,
            "retry-owner".to_string(),
            retry_time,
            retry_time + chrono::TimeDelta::seconds(60),
        )
        .await
        .expect("claim reserved retry")
        .expect("reserved retry must resume when due");
    assert_eq!(waiting.id, claimed.id);
    assert_eq!(
        Some(waiting.reviewer_agent_id.expect("reviewer")),
        claimed.reviewer_agent_id
    );
    assert_eq!(
        ProjectReviewJobStatus::Queued,
        store
            .load_project_review_job(project_id, queued_id)
            .await
            .expect("load queued job")
            .expect("queued job")
            .status
    );
}

#[tokio::test]
async fn webhook_delivery_is_idempotent_per_pull_request() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let delivery_id = Some("delivery-shared-by-check-suite");

    let first = store
        .enqueue_project_review_job(test_review_job(project_id, 42, "head-42", delivery_id))
        .await
        .expect("enqueue first PR");
    let second = store
        .enqueue_project_review_job(test_review_job(project_id, 43, "head-43", delivery_id))
        .await
        .expect("enqueue second PR");
    let repeated = store
        .enqueue_project_review_job(test_review_job(
            project_id,
            42,
            "different-payload-head",
            delivery_id,
        ))
        .await
        .expect("repeat first delivery");

    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Queued,
        first.disposition
    );
    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Queued,
        second.disposition
    );
    assert_eq!(
        ProjectReviewJobEnqueueDisposition::Deduped,
        repeated.disposition
    );
    assert_eq!(first.job.id, repeated.job.id);
    assert_eq!(
        2,
        store
            .load_project_pull_request_reviews(project_id, 1, 10)
            .await
            .expect("load webhook jobs")
            .reviews
            .len()
    );
}

#[tokio::test]
async fn concurrent_review_job_claim_has_one_winner() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let job = test_review_job(project_id, 7, "head", None);
    let job_id = job.id;
    store
        .enqueue_project_review_job(job)
        .await
        .expect("enqueue");
    let store = Arc::new(store);
    let current_time = Utc::now();
    let lease = current_time + chrono::TimeDelta::seconds(60);
    let (left, right) = tokio::join!(
        store.claim_due_project_review_job(project_id, "owner-a".to_string(), current_time, lease),
        store.claim_due_project_review_job(project_id, "owner-b".to_string(), current_time, lease)
    );
    let winners = [left.expect("left claim"), right.expect("right claim")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    assert_eq!(1, winners.len());
    assert_eq!(0, winners[0].attempt_count);
    assert_eq!(ProjectReviewJobStatus::Preparing, winners[0].status);
    let mut stale_claim = winners[0].clone();
    let owner = stale_claim.lease_owner.clone().expect("winner owner");
    let heartbeat_at = current_time + chrono::TimeDelta::seconds(15);
    let extended_lease = lease + chrono::TimeDelta::minutes(1);
    assert!(
        store
            .heartbeat_project_review_job(job_id, owner.clone(), heartbeat_at, extended_lease)
            .await
            .expect("extend lease")
    );
    stale_claim.updated_at = heartbeat_at + chrono::TimeDelta::seconds(1);
    assert!(
        store
            .save_claimed_project_review_job(stale_claim, owner)
            .await
            .expect("save stale claimed snapshot")
    );
    let saved = store
        .load_project_review_job(project_id, job_id)
        .await
        .expect("load saved job")
        .expect("saved job");
    assert_eq!(Some(extended_lease), saved.lease_expires_at);
}

#[tokio::test]
async fn review_attempt_start_atomically_increments_job_and_creates_run() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let job = test_review_job(project_id, 42, "head", None);
    let job_id = job.id;
    store
        .enqueue_project_review_job(job)
        .await
        .expect("enqueue");
    let started_at = Utc::now();
    let owner = "worker".to_string();
    let claimed = store
        .claim_due_project_review_job(
            project_id,
            owner.clone(),
            started_at,
            started_at + chrono::TimeDelta::minutes(1),
        )
        .await
        .expect("claim")
        .expect("claimed job");
    assert_eq!(0, claimed.attempt_count);
    let run_id = Uuid::new_v4();

    let started = store
        .begin_claimed_project_review_attempt(job_id, owner.clone(), run_id, started_at)
        .await
        .expect("begin attempt");

    assert_eq!(1, started.attempt_count);
    assert_eq!(Some(run_id), started.active_run_id);
    assert_eq!(
        vec![ProjectReviewRunSummary {
            id: run_id,
            job_id: Some(job_id),
            attempt_index: 1,
            project_id,
            reviewer_agent_id: None,
            turn_id: None,
            started_at,
            finished_at: None,
            status: ProjectReviewRunStatus::Syncing,
            outcome: None,
            review_event: None,
            pr: Some(42),
            summary: None,
            error: None,
            failure: None,
            token_usage: TokenUsage::default(),
            history_status: Default::default(),
            history_archive_id: None,
            history_archived_at: None,
        }],
        store
            .load_project_review_job_attempts(job_id, 1)
            .await
            .expect("load atomic attempt")
    );
    assert!(
        store
            .begin_claimed_project_review_attempt(job_id, owner, Uuid::new_v4(), started_at,)
            .await
            .is_err(),
        "one claimed lease cannot create a second active attempt"
    );
    assert_eq!(
        1,
        store
            .load_project_review_job(project_id, job_id)
            .await
            .expect("load job")
            .expect("job")
            .attempt_count
    );
}

#[tokio::test]
async fn submitted_attempt_archives_run_before_releasing_job_ownership() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let job = test_review_job(project_id, 43, "head", None);
    let job_id = job.id;
    store
        .enqueue_project_review_job(job)
        .await
        .expect("enqueue job");
    let started_at = Utc::now();
    let owner = "worker".to_string();
    store
        .claim_due_project_review_job(
            project_id,
            owner.clone(),
            started_at,
            started_at + chrono::TimeDelta::minutes(1),
        )
        .await
        .expect("claim")
        .expect("claimed job");
    let run_id = Uuid::new_v4();
    store
        .begin_claimed_project_review_attempt(job_id, owner.clone(), run_id, started_at)
        .await
        .expect("begin attempt");
    let submitted_at = started_at + chrono::TimeDelta::seconds(10);
    store
        .record_project_review_submission_intent(ProjectReviewSubmissionIntent {
            job_id,
            head_sha: "head".to_string(),
            event: ProjectReviewDecision::Approve,
            body_hash: "hash-43".to_string(),
            comment_count: 0,
            created_at: started_at,
        })
        .await
        .expect("persist intent");
    store
        .record_project_review_submission_receipt(
            job_id,
            ProjectReviewSubmissionReceipt {
                github_review_id: 43,
                event: ProjectReviewDecision::Approve,
                head_sha: "head".to_string(),
                html_url: Some("https://example.test/review/43".to_string()),
                submitted_at,
            },
        )
        .await
        .expect("persist receipt");
    let receipted = store
        .load_project_review_job(project_id, job_id)
        .await
        .expect("load receipted job")
        .expect("receipted job");
    assert_eq!(ProjectReviewJobStatus::Succeeded, receipted.status);
    assert_eq!(Some(run_id), receipted.active_run_id);
    assert_eq!(Some(owner.clone()), receipted.lease_owner);
    assert_eq!(
        None,
        store
            .claim_due_project_review_cleanup_task(
                "cleanup-before-archive".to_string(),
                submitted_at,
                submitted_at + chrono::TimeDelta::minutes(5),
            )
            .await
            .expect("cleanup must wait for archive")
    );

    let turn_id = "turn-43".to_string();
    let finished = ProjectReviewRunDetail {
        summary: ProjectReviewRunSummary {
            id: run_id,
            job_id: Some(job_id),
            attempt_index: 1,
            project_id,
            reviewer_agent_id: Some(Uuid::new_v4()),
            turn_id: Some(turn_id.clone()),
            started_at,
            finished_at: Some(submitted_at),
            status: ProjectReviewRunStatus::Succeeded,
            outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
            review_event: Some(ProjectReviewDecision::Approve),
            pr: Some(43),
            summary: Some("approved".to_string()),
            error: None,
            failure: None,
            token_usage: TokenUsage::default(),
            history_status: Default::default(),
            history_archive_id: None,
            history_archived_at: None,
        },
        history: Some(ThreadTurnHistory {
            turn: completed_turn(
                turn_id,
                "reviewer-43".to_string(),
                started_at.timestamp_millis(),
                submitted_at.timestamp_millis(),
            ),
            items: Vec::new(),
            context_disposition: ThreadContextDisposition::Active,
        }),
    };
    store
        .finish_project_review_run(&finished)
        .await
        .expect("archive attempt and release ownership");
    let mut stale_completion = finished.clone();
    stale_completion.summary.status = ProjectReviewRunStatus::Interrupted;
    stale_completion.summary.outcome = None;
    stale_completion.summary.error = Some("late worker".to_string());
    stale_completion.history = None;
    store
        .finish_project_review_run(&stale_completion)
        .await
        .expect("late completion is idempotently ignored");
    let error = store
        .update_active_project_review_run_turn(
            project_id,
            run_id,
            Uuid::new_v4(),
            "late-turn".to_string(),
        )
        .await
        .expect_err("late turn callback must not reopen archived Run");
    assert!(
        error
            .to_string()
            .contains("no longer the active unfinished attempt")
    );

    let completed = store
        .load_project_review_job(project_id, job_id)
        .await
        .expect("load completed job")
        .expect("completed job");
    assert_eq!(None, completed.active_run_id);
    assert_eq!(None, completed.lease_owner);
    assert_eq!(None, completed.lease_expires_at);
    assert!(
        store
            .claim_due_project_review_cleanup_task(
                "cleanup-after-archive".to_string(),
                submitted_at,
                submitted_at + chrono::TimeDelta::minutes(5),
            )
            .await
            .expect("cleanup may start after archive")
            .is_some()
    );
    assert_eq!(
        finished,
        store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load archived run")
            .expect("archived run")
    );
}

#[tokio::test]
async fn expired_recovery_archives_run_detached_by_failed_job_transition() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let job = test_review_job(project_id, 44, "head", None);
    let job_id = job.id;
    store
        .enqueue_project_review_job(job)
        .await
        .expect("enqueue job");
    let started_at = Utc::now();
    let owner = "worker".to_string();
    store
        .claim_due_project_review_job(
            project_id,
            owner.clone(),
            started_at,
            started_at + chrono::TimeDelta::minutes(1),
        )
        .await
        .expect("claim")
        .expect("claimed job");
    let run_id = Uuid::new_v4();
    let mut detached = store
        .begin_claimed_project_review_attempt(job_id, owner.clone(), run_id, started_at)
        .await
        .expect("begin attempt");
    detached.status = ProjectReviewJobStatus::RetryWaiting;
    detached.next_attempt_at = Some(Utc::now() + chrono::TimeDelta::minutes(1));
    detached.active_run_id = None;
    assert!(
        store
            .save_claimed_project_review_job(detached, owner.clone())
            .await
            .expect("persist detached ownership")
    );

    let mut run = store
        .load_project_review_run(project_id, run_id)
        .await
        .expect("load run")
        .expect("run");
    run.summary.finished_at = Some(Utc::now());
    run.summary.status = ProjectReviewRunStatus::Interrupted;
    run.summary.error = Some("review interrupted after persistence failure".to_string());
    assert!(
        store.finish_project_review_run(&run).await.is_err(),
        "ordinary finalization must retain strict active ownership"
    );
    assert!(
        store.finish_expired_project_review_run(&run).await.is_err(),
        "recovery must not archive a detached Run while its lease is valid"
    );
    let mut expired = store
        .load_project_review_job(project_id, job_id)
        .await
        .expect("load detached job")
        .expect("detached job");
    expired.lease_owner = None;
    expired.lease_expires_at = None;
    assert!(
        store
            .save_claimed_project_review_job(expired, owner)
            .await
            .expect("expire detached ownership")
    );
    store
        .finish_expired_project_review_run(&run)
        .await
        .expect("expired recovery archives detached Run");

    let archived = store
        .load_project_review_run(project_id, run_id)
        .await
        .expect("reload run")
        .expect("archived run");
    assert_eq!(ProjectReviewRunStatus::Interrupted, archived.summary.status);
    assert_eq!(Some(job_id), archived.summary.job_id);
    assert!(archived.summary.finished_at.is_some());
    let recovered_job = store
        .load_project_review_job(project_id, job_id)
        .await
        .expect("reload job")
        .expect("job");
    assert_eq!(ProjectReviewJobStatus::RetryWaiting, recovered_job.status);
    assert_eq!(None, recovered_job.active_run_id);
}

#[tokio::test]
async fn expired_review_job_recovers_without_losing_reviewer() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut job = test_review_job(project_id, 9, "head", None);
    job.status = ProjectReviewJobStatus::Running;
    job.reviewer_agent_id = Some(reviewer_id);
    job.lease_owner = Some("dead-owner".to_string());
    job.lease_expires_at = Some(Utc::now() - chrono::TimeDelta::seconds(1));
    store
        .save_project_review_job(job.clone())
        .await
        .expect("save running job");

    assert_eq!(
        1,
        store
            .recover_expired_project_review_jobs(Utc::now())
            .await
            .expect("recover")
    );
    let recovered = store
        .load_project_review_job(project_id, job.id)
        .await
        .expect("load")
        .expect("job");
    assert_eq!(ProjectReviewJobStatus::RetryWaiting, recovered.status);
    assert_eq!(Some(reviewer_id), recovered.reviewer_agent_id);
    assert!(recovered.next_attempt_at.is_some());
}

#[tokio::test]
async fn failed_missing_submission_receipt_recovers_once_for_reconciliation() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let current_time = Utc::now();
    let mut job = test_review_job(project_id, 91, "head", None);
    job.status = ProjectReviewJobStatus::Failed;
    job.attempt_count = 1;
    job.next_attempt_at = None;
    job.failure = Some(ProjectReviewFailure {
        category: ProjectReviewFailureCategory::Validation,
        code: Some("missing_submission_receipt".to_string()),
        http_status: None,
        message: "reviewer reported submission without a receipt".to_string(),
        retry: pl_protocol::RetryDisposition::Permanent,
    });
    job.submission_intent = Some(ProjectReviewSubmissionIntent {
        job_id: job.id,
        head_sha: job.head_sha.clone(),
        event: ProjectReviewDecision::Approve,
        body_hash: "sha256:body".to_string(),
        comment_count: 0,
        created_at: current_time - chrono::TimeDelta::minutes(10),
    });
    job.updated_at = current_time - chrono::TimeDelta::minutes(9);
    job.finished_at = Some(job.updated_at);
    store
        .save_project_review_job(job.clone())
        .await
        .expect("save legacy failed job");

    assert_eq!(
        1,
        store
            .recover_expired_project_review_jobs(current_time)
            .await
            .expect("recover ambiguous submission")
    );
    job.status = ProjectReviewJobStatus::Reconciling;
    job.next_attempt_at = Some(current_time);
    job.failure = None;
    job.active_run_id = None;
    job.lease_owner = None;
    job.lease_expires_at = None;
    job.updated_at = current_time;
    job.finished_at = None;
    assert_eq!(
        job,
        store
            .load_project_review_job(project_id, job.id)
            .await
            .expect("load recovered job")
            .expect("recovered job")
    );

    assert_eq!(
        0,
        store
            .recover_expired_project_review_jobs(
                current_time + chrono::TimeDelta::milliseconds(500),
            )
            .await
            .expect("queued reconciliation is not recovered again")
    );
    let claimed_at = current_time + chrono::TimeDelta::seconds(1);
    let mut claimed = store
        .claim_due_project_review_job(
            project_id,
            "reconciler".to_string(),
            claimed_at,
            claimed_at + chrono::TimeDelta::minutes(5),
        )
        .await
        .expect("claim recovered reconciliation")
        .expect("recovered reconciliation");
    assert_eq!(ProjectReviewJobStatus::Reconciling, claimed.status);
    claimed.status = ProjectReviewJobStatus::Failed;
    claimed.next_attempt_at = None;
    claimed.failure = Some(ProjectReviewFailure {
        category: ProjectReviewFailureCategory::Github,
        code: None,
        http_status: None,
        message: "reconciliation deadline elapsed".to_string(),
        retry: pl_protocol::RetryDisposition::Permanent,
    });
    claimed.lease_owner = None;
    claimed.lease_expires_at = None;
    claimed.updated_at = claimed_at + chrono::TimeDelta::seconds(1);
    claimed.finished_at = Some(claimed.updated_at);
    assert!(
        store
            .save_claimed_project_review_job(claimed, "reconciler".to_string())
            .await
            .expect("persist terminal reconciliation failure")
    );
    assert_eq!(
        0,
        store
            .recover_expired_project_review_jobs(claimed_at + chrono::TimeDelta::seconds(2))
            .await
            .expect("terminal reconciliation failure is not reopened")
    );
}

#[tokio::test]
async fn claimed_job_persists_reconciliation_and_releases_lease_atomically() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut job = test_review_job(project_id, 92, "head", None);
    let current_time = job.updated_at + chrono::TimeDelta::seconds(1);
    job.submission_intent = Some(ProjectReviewSubmissionIntent {
        job_id: job.id,
        head_sha: job.head_sha.clone(),
        event: ProjectReviewDecision::Approve,
        body_hash: "sha256:body".to_string(),
        comment_count: 0,
        created_at: current_time,
    });
    store
        .save_project_review_job(job)
        .await
        .expect("save pending job");
    let owner = "review-worker".to_string();
    let mut claimed = store
        .claim_due_project_review_job(
            project_id,
            owner.clone(),
            current_time,
            current_time + chrono::TimeDelta::minutes(5),
        )
        .await
        .expect("claim pending job")
        .expect("pending job");

    claimed.status = ProjectReviewJobStatus::Reconciling;
    claimed.next_attempt_at = Some(current_time + chrono::TimeDelta::seconds(10));
    claimed.active_run_id = None;
    claimed.lease_owner = None;
    claimed.lease_expires_at = None;
    claimed.updated_at = current_time + chrono::TimeDelta::seconds(1);
    assert!(
        store
            .save_claimed_project_review_job(claimed.clone(), owner)
            .await
            .expect("persist reconciliation")
    );
    assert_eq!(
        claimed,
        store
            .load_project_review_job(project_id, claimed.id)
            .await
            .expect("load reconciliation")
            .expect("reconciliation")
    );
}

#[tokio::test]
async fn recovery_preserves_live_lease_and_reviewer_ownership() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let current_time = Utc::now();
    let mut job = test_review_job(project_id, 10, "head", None);
    job.status = ProjectReviewJobStatus::Running;
    job.reviewer_agent_id = Some(reviewer_id);
    job.lease_owner = Some("stopped-process".to_string());
    job.lease_expires_at = Some(current_time + chrono::TimeDelta::minutes(5));
    store
        .save_project_review_job(job.clone())
        .await
        .expect("save running job");

    assert_eq!(
        0,
        store
            .recover_expired_project_review_jobs(current_time)
            .await
            .expect("future lease is not expired")
    );
    let preserved = store
        .load_project_review_job(project_id, job.id)
        .await
        .expect("load preserved job")
        .expect("preserved job");
    let cleanup_tasks = store
        .load_project_review_cleanup_tasks(job.id)
        .await
        .expect("load cleanup tasks");
    assert_eq!(job, preserved);
    assert_eq!(Vec::<ProjectReviewCleanupTask>::new(), cleanup_tasks);
}

#[tokio::test]
async fn expired_active_attempt_can_begin_the_next_attempt() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut job = test_review_job(project_id, 11, "head", None);
    job.max_attempts = 2;
    let job_id = job.id;
    store
        .enqueue_project_review_job(job)
        .await
        .expect("enqueue job");
    let first_started_at = Utc::now();
    store
        .claim_due_project_review_job(
            project_id,
            "stopped-process".to_string(),
            first_started_at,
            first_started_at + chrono::TimeDelta::minutes(5),
        )
        .await
        .expect("claim first attempt")
        .expect("first attempt due");
    let first_run_id = Uuid::new_v4();
    store
        .begin_claimed_project_review_attempt(
            job_id,
            "stopped-process".to_string(),
            first_run_id,
            first_started_at,
        )
        .await
        .expect("begin first attempt");

    let recovered_at = first_started_at + chrono::TimeDelta::minutes(6);
    assert_eq!(
        1,
        store
            .recover_expired_project_review_jobs(recovered_at)
            .await
            .expect("recover expired attempt")
    );
    let recovered = store
        .load_project_review_job(project_id, job_id)
        .await
        .expect("load recovered job")
        .expect("recovered job");
    assert_eq!(ProjectReviewJobStatus::RetryWaiting, recovered.status);
    assert_eq!(None, recovered.active_run_id);

    let second_started_at = recovered_at + chrono::TimeDelta::seconds(1);
    store
        .claim_due_project_review_job(
            project_id,
            "new-process".to_string(),
            second_started_at,
            second_started_at + chrono::TimeDelta::minutes(5),
        )
        .await
        .expect("claim recovered job")
        .expect("recovered job due");
    let second_run_id = Uuid::new_v4();
    let started = store
        .begin_claimed_project_review_attempt(
            job_id,
            "new-process".to_string(),
            second_run_id,
            second_started_at,
        )
        .await
        .expect("begin next attempt after recovery");
    assert_eq!(2, started.attempt_count);
    assert_eq!(Some(second_run_id), started.active_run_id);
    assert_eq!(None, started.failure);
    assert_eq!(
        vec![
            ProjectReviewRunStatus::Interrupted,
            ProjectReviewRunStatus::Syncing,
        ],
        store
            .load_project_review_job_attempts(job_id, 2)
            .await
            .expect("load both immutable attempts")
            .into_iter()
            .map(|attempt| attempt.status)
            .collect::<Vec<_>>()
    );
    let final_recovery = second_started_at + chrono::TimeDelta::minutes(6);
    assert_eq!(
        1,
        store
            .recover_expired_project_review_jobs(final_recovery)
            .await
            .expect("recover final allowed attempt")
    );
    let final_owner = "final-worker".to_string();
    store
        .claim_due_project_review_job(
            project_id,
            final_owner.clone(),
            final_recovery,
            final_recovery + chrono::TimeDelta::minutes(1),
        )
        .await
        .expect("claim recovered job")
        .expect("recovered job due");
    let error = store
        .begin_claimed_project_review_attempt(job_id, final_owner, Uuid::new_v4(), final_recovery)
        .await
        .expect_err("attempt limit must be enforced at Run creation");
    assert!(
        error
            .to_string()
            .contains("reached its maximum of 2 attempts")
    );
    let preserved = store
        .load_project_review_job(project_id, job_id)
        .await
        .expect("load limited job")
        .expect("limited job");
    assert_eq!(2, preserved.attempt_count);
    assert_eq!(None, preserved.active_run_id);
}

#[tokio::test]
async fn expired_lease_recovery_interrupts_only_the_active_run() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let job = test_review_job(project_id, 12, "head", None);
    let job_id = job.id;
    store
        .enqueue_project_review_job(job)
        .await
        .expect("enqueue job");
    let started_at = Utc::now();
    let lease_expires_at = started_at + chrono::TimeDelta::minutes(5);
    store
        .claim_due_project_review_job(
            project_id,
            "expired-owner".to_string(),
            started_at,
            lease_expires_at,
        )
        .await
        .expect("claim attempt")
        .expect("attempt due");
    let active_run_id = Uuid::new_v4();
    store
        .begin_claimed_project_review_attempt(
            job_id,
            "expired-owner".to_string(),
            active_run_id,
            started_at,
        )
        .await
        .expect("begin active attempt");
    let active_before = store
        .load_project_review_run(project_id, active_run_id)
        .await
        .expect("load active run")
        .expect("active run");
    let stray_run_id = Uuid::new_v4();
    let mut stray_expected = active_before.clone();
    stray_expected.summary.id = stray_run_id;
    stray_expected.summary.attempt_index = 2;
    store
        .save_project_review_run(&stray_expected)
        .await
        .expect("save non-active unfinished run");

    let recovered_at = lease_expires_at + chrono::TimeDelta::seconds(1);
    assert_eq!(
        1,
        store
            .recover_expired_project_review_jobs(recovered_at)
            .await
            .expect("recover expired ownership")
    );
    let mut active_expected = active_before;
    active_expected.summary.status = ProjectReviewRunStatus::Interrupted;
    active_expected.summary.finished_at = Some(recovered_at);
    active_expected.summary.error =
        Some("review attempt lease expired before completion".to_string());
    assert_eq!(
        active_expected,
        store
            .load_project_review_run(project_id, active_run_id)
            .await
            .expect("load interrupted active run")
            .expect("interrupted active run")
    );
    assert_eq!(
        stray_expected,
        store
            .load_project_review_run(project_id, stray_run_id)
            .await
            .expect("load preserved non-active run")
            .expect("preserved non-active run")
    );
}

#[tokio::test]
async fn submission_intent_is_idempotent_and_receipt_completes_job() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let job = test_review_job(project_id, 11, "head", None);
    store
        .save_project_review_job(job.clone())
        .await
        .expect("save job");
    let created_at = Utc::now();
    let intent = ProjectReviewSubmissionIntent {
        job_id: job.id,
        head_sha: "head".to_string(),
        event: ProjectReviewDecision::RequestChanges,
        body_hash: "hash".to_string(),
        comment_count: 2,
        created_at,
    };
    store
        .record_project_review_submission_intent(intent.clone())
        .await
        .expect("record intent");
    let mut body_only_retry = intent.clone();
    body_only_retry.comment_count = 0;
    body_only_retry.created_at += chrono::TimeDelta::seconds(1);
    let pending = store
        .record_project_review_submission_intent(body_only_retry)
        .await
        .expect("same logical body-only fallback");
    assert_eq!(Some(intent), pending.submission_intent);

    let owner = "reconciliation-worker".to_string();
    let mut reconciling = pending;
    reconciling.status = ProjectReviewJobStatus::Reconciling;
    reconciling.next_attempt_at = Some(created_at);
    store
        .save_project_review_job(reconciling)
        .await
        .expect("schedule reconciliation");
    let claimed = store
        .claim_due_project_review_job(
            project_id,
            owner.clone(),
            created_at,
            created_at + chrono::TimeDelta::minutes(1),
        )
        .await
        .expect("claim reconciliation")
        .expect("due reconciliation");
    assert_eq!(Some(owner), claimed.lease_owner);

    let receipt = ProjectReviewSubmissionReceipt {
        github_review_id: 123,
        event: ProjectReviewDecision::RequestChanges,
        head_sha: "head".to_string(),
        html_url: Some("https://example.test/review/123".to_string()),
        submitted_at: Utc::now(),
    };
    let completed = store
        .record_project_review_submission_receipt(job.id, receipt.clone())
        .await
        .expect("record receipt");
    assert_eq!(ProjectReviewJobStatus::Succeeded, completed.status);
    assert_eq!(Some(receipt), completed.submission_receipt);
    assert_eq!(None, completed.active_run_id);
    assert_eq!(None, completed.lease_owner);
    assert_eq!(None, completed.lease_expires_at);
    assert_eq!(None, completed.next_attempt_at);
    assert_eq!(None, completed.failure);
}

#[tokio::test]
async fn stale_worker_snapshot_cannot_overwrite_terminal_submission_receipt() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut stale_worker_snapshot = test_review_job(project_id, 111, "head", None);
    stale_worker_snapshot.status = ProjectReviewJobStatus::Running;
    stale_worker_snapshot.updated_at = Utc::now();
    store
        .save_project_review_job(stale_worker_snapshot.clone())
        .await
        .expect("save running job");
    let receipt = ProjectReviewSubmissionReceipt {
        github_review_id: 456,
        event: ProjectReviewDecision::Approve,
        head_sha: "head".to_string(),
        html_url: Some("https://example.test/review/456".to_string()),
        submitted_at: stale_worker_snapshot.updated_at + chrono::TimeDelta::seconds(1),
    };
    store
        .record_project_review_submission_intent(ProjectReviewSubmissionIntent {
            job_id: stale_worker_snapshot.id,
            head_sha: receipt.head_sha.clone(),
            event: receipt.event.clone(),
            body_hash: "stale-worker-hash".to_string(),
            comment_count: 0,
            created_at: stale_worker_snapshot.updated_at,
        })
        .await
        .expect("record intent");
    let completed = store
        .record_project_review_submission_receipt(stale_worker_snapshot.id, receipt.clone())
        .await
        .expect("record receipt");

    stale_worker_snapshot.updated_at = receipt.submitted_at + chrono::TimeDelta::seconds(1);
    store
        .save_project_review_job(stale_worker_snapshot)
        .await
        .expect("stale write is ignored");

    assert_eq!(
        store
            .load_project_review_job(project_id, completed.id)
            .await
            .expect("load completed job")
            .expect("completed job"),
        completed
    );
}

#[tokio::test]
async fn submission_receipt_without_matching_intent_is_rejected() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let job = test_review_job(project_id, 112, "head", None);
    store
        .save_project_review_job(job.clone())
        .await
        .expect("save job");

    let error = store
        .record_project_review_submission_receipt(
            job.id,
            ProjectReviewSubmissionReceipt {
                github_review_id: 112,
                event: ProjectReviewDecision::Approve,
                head_sha: job.head_sha.clone(),
                html_url: None,
                submitted_at: Utc::now(),
            },
        )
        .await
        .expect_err("receipt without intent must fail");
    assert!(error.to_string().contains("without an intent"));
    assert_eq!(
        ProjectReviewJobStatus::Queued,
        store
            .load_project_review_job(project_id, job.id)
            .await
            .expect("load job")
            .expect("job")
            .status
    );
}

#[tokio::test]
async fn terminal_review_job_persists_idempotent_retryable_cleanup_tasks() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let timestamp = Utc::now();
    let mut job = test_review_job(project_id, 12, "head", None);
    job.status = ProjectReviewJobStatus::Failed;
    job.reviewer_agent_id = Some(reviewer_id);
    job.active_run_id = Some(run_id);
    job.next_attempt_at = None;
    job.finished_at = Some(timestamp);
    job.updated_at = timestamp;

    store
        .save_project_review_job(job.clone())
        .await
        .expect("save terminal job");
    let mut run = ProjectReviewRunDetail {
        summary: ProjectReviewRunSummary {
            id: run_id,
            job_id: Some(job.id),
            attempt_index: 1,
            project_id,
            reviewer_agent_id: Some(reviewer_id),
            turn_id: None,
            started_at: timestamp - chrono::TimeDelta::minutes(1),
            finished_at: None,
            status: ProjectReviewRunStatus::Running,
            outcome: None,
            review_event: None,
            pr: Some(job.pr),
            summary: None,
            error: None,
            failure: None,
            token_usage: TokenUsage::default(),
            history_status: Default::default(),
            history_archive_id: None,
            history_archived_at: None,
        },
        history: None,
    };
    store
        .save_project_review_run(&run)
        .await
        .expect("save unfinished run");
    let tasks = store
        .load_project_review_cleanup_tasks(job.id)
        .await
        .expect("load cleanup tasks");
    assert_eq!(
        vec![
            ProjectReviewCleanupResourceKind::ReviewContext,
            ProjectReviewCleanupResourceKind::ReviewerAgent,
            ProjectReviewCleanupResourceKind::ToolOutputNamespace,
        ],
        tasks
            .iter()
            .map(|task| task.resource_kind)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        vec![ProjectReviewCleanupTaskStatus::Pending; 3],
        tasks.iter().map(|task| task.status).collect::<Vec<_>>()
    );

    let owner = "cleanup-worker".to_string();
    assert_eq!(
        None,
        store
            .claim_due_project_review_cleanup_task(
                owner.clone(),
                timestamp + chrono::TimeDelta::seconds(1),
                timestamp + chrono::TimeDelta::minutes(2),
            )
            .await
            .expect("cleanup waits for Run archive")
    );
    run.summary.finished_at = Some(timestamp);
    run.summary.status = ProjectReviewRunStatus::Failed;
    run.summary.outcome = Some(ProjectReviewOutcome::Failed);
    store
        .save_project_review_run(&run)
        .await
        .expect("simulate legacy archived Run ownership");
    assert_eq!(
        1,
        store
            .release_expired_archived_terminal_project_review_ownership(
                timestamp + chrono::TimeDelta::seconds(1),
            )
            .await
            .expect("release archived ownership")
    );
    assert_eq!(
        0,
        store
            .release_expired_archived_terminal_project_review_ownership(
                timestamp + chrono::TimeDelta::seconds(1),
            )
            .await
            .expect("release is idempotent")
    );
    let claimed = store
        .claim_due_project_review_cleanup_task(
            owner.clone(),
            timestamp + chrono::TimeDelta::seconds(1),
            timestamp + chrono::TimeDelta::minutes(2),
        )
        .await
        .expect("claim cleanup task")
        .expect("due cleanup task");
    assert_eq!(ProjectReviewCleanupTaskStatus::Running, claimed.status);
    assert_eq!(1, claimed.attempt_count);

    let retry_at = timestamp + chrono::TimeDelta::seconds(30);
    assert!(
        store
            .retry_project_review_cleanup_task(
                claimed.id.clone(),
                owner.clone(),
                retry_at,
                "temporary failure".to_string(),
            )
            .await
            .expect("schedule retry")
    );
    let reclaimed = store
        .claim_due_project_review_cleanup_task(
            owner.clone(),
            retry_at,
            retry_at + chrono::TimeDelta::minutes(2),
        )
        .await
        .expect("reclaim cleanup task")
        .expect("retry is due");
    assert_eq!(claimed.id, reclaimed.id);
    assert_eq!(2, reclaimed.attempt_count);
    assert!(
        store
            .complete_project_review_cleanup_task(reclaimed.id, owner, retry_at)
            .await
            .expect("complete cleanup task")
    );

    store
        .save_project_review_job(job.clone())
        .await
        .expect("save terminal job again");
    let tasks = store
        .load_project_review_cleanup_tasks(job.id)
        .await
        .expect("reload cleanup tasks");
    assert_eq!(3, tasks.len());
    assert_eq!(
        1,
        tasks
            .iter()
            .filter(|task| task.status == ProjectReviewCleanupTaskStatus::Succeeded)
            .count()
    );
}

#[tokio::test]
async fn review_job_retention_preserves_active_and_leased_jobs() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let old = Utc::now() - chrono::TimeDelta::days(60);
    let mut removable = test_review_job(project_id, 20, "head-20", None);
    removable.status = ProjectReviewJobStatus::Succeeded;
    removable.created_at = old;
    removable.updated_at = old;
    removable.finished_at = Some(old);
    removable.next_attempt_at = None;
    let mut leased = test_review_job(project_id, 21, "head-21", None);
    leased.status = ProjectReviewJobStatus::Failed;
    leased.created_at = old;
    leased.updated_at = old;
    leased.finished_at = Some(old);
    leased.next_attempt_at = None;
    leased.lease_owner = Some("worker".to_string());
    leased.lease_expires_at = Some(Utc::now() + chrono::TimeDelta::hours(1));
    let mut malformed_lease = test_review_job(project_id, 23, "head-23", None);
    malformed_lease.status = ProjectReviewJobStatus::Failed;
    malformed_lease.created_at = old;
    malformed_lease.updated_at = old;
    malformed_lease.finished_at = Some(old);
    malformed_lease.next_attempt_at = None;
    malformed_lease.lease_owner = Some("worker-without-expiry".to_string());
    malformed_lease.lease_expires_at = None;
    let mut active = test_review_job(project_id, 22, "head-22", None);
    active.status = ProjectReviewJobStatus::Running;
    active.created_at = old;
    active.updated_at = old;
    active.next_attempt_at = None;
    for job in [&removable, &leased, &malformed_lease, &active] {
        store
            .save_project_review_job(job.clone())
            .await
            .expect("save review job");
    }
    let removable_run_id = Uuid::new_v4();
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: removable_run_id,
                job_id: Some(removable.id),
                attempt_index: 1,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at: old,
                finished_at: Some(old),
                status: ProjectReviewRunStatus::Completed,
                outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
                review_event: Some(ProjectReviewDecision::Approve),
                pr: Some(removable.pr),
                summary: Some("approved".to_string()),
                error: None,
                failure: None,
                token_usage: TokenUsage::default(),
                history_status: Default::default(),
                history_archive_id: None,
                history_archived_at: None,
            },
            history: None,
        })
        .await
        .expect("save removable review run");

    assert_eq!(
        1,
        store
            .prune_project_review_jobs_before_batch(
                Utc::now() - chrono::TimeDelta::days(30),
                Utc::now(),
                500,
            )
            .await
            .expect("prune jobs")
    );
    assert!(
        store
            .load_project_review_job(project_id, removable.id)
            .await
            .expect("load removable")
            .is_none()
    );
    assert!(
        store
            .load_project_review_run(project_id, removable_run_id)
            .await
            .expect("load removable run")
            .is_none(),
        "review run must be deleted atomically with its retained job"
    );
    assert!(
        store
            .load_project_review_job(project_id, leased.id)
            .await
            .expect("load leased")
            .is_some()
    );
    assert!(
        store
            .load_project_review_job(project_id, malformed_lease.id)
            .await
            .expect("load malformed lease")
            .is_some(),
        "an owned lease without an expiry is not proven stale and must be retained"
    );
    assert!(
        store
            .load_project_review_job(project_id, active.id)
            .await
            .expect("load active")
            .is_some()
    );
}

#[tokio::test]
async fn review_job_retention_keeps_the_exact_seven_day_boundary() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let cutoff = Utc::now() - chrono::TimeDelta::days(7);
    let mut expired = test_review_job(project_id, 70, "head-70", None);
    expired.status = ProjectReviewJobStatus::Succeeded;
    expired.finished_at = Some(cutoff - chrono::TimeDelta::milliseconds(1));
    expired.updated_at = expired.finished_at.expect("expired timestamp");
    expired.next_attempt_at = None;
    let mut boundary = test_review_job(project_id, 71, "head-71", None);
    boundary.status = ProjectReviewJobStatus::Succeeded;
    boundary.finished_at = Some(cutoff);
    boundary.updated_at = cutoff;
    boundary.next_attempt_at = None;
    for job in [&expired, &boundary] {
        store
            .save_project_review_job(job.clone())
            .await
            .expect("save terminal job");
    }

    assert_eq!(
        1,
        store
            .prune_project_review_jobs_before_batch(cutoff, Utc::now(), 100)
            .await
            .expect("prune seven-day history")
    );
    assert!(
        store
            .load_project_review_job(project_id, expired.id)
            .await
            .expect("load expired job")
            .is_none()
    );
    assert!(
        store
            .load_project_review_job(project_id, boundary.id)
            .await
            .expect("load boundary job")
            .is_some()
    );
}

#[tokio::test]
async fn review_job_attempt_loader_rejects_missing_run_history() {
    let (_dir, store) = store().await;
    let job_id = Uuid::new_v4();

    assert_eq!(
        "data integrity error: review job ".to_string()
            + &job_id.to_string()
            + " declares 1 attempts but stores 0 runs",
        store
            .load_project_review_job_attempts(job_id, 1)
            .await
            .expect_err("missing immutable attempt history must fail")
            .to_string()
    );
}

#[tokio::test]
async fn pull_request_review_pages_aggregate_latest_jobs_and_preserve_attempt_history() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let earlier = DateTime::parse_from_rfc3339("2026-08-11T10:00:00Z")
        .expect("earlier timestamp")
        .with_timezone(&Utc);
    let shared = DateTime::parse_from_rfc3339("2026-08-11T11:00:00Z")
        .expect("shared timestamp")
        .with_timezone(&Utc);
    let latest = DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
        .expect("latest timestamp")
        .with_timezone(&Utc);

    let mut succeeded = test_review_job(project_id, 42, "head-a", None);
    succeeded.id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("job id");
    succeeded.status = ProjectReviewJobStatus::Succeeded;
    succeeded.created_at = earlier;
    succeeded.updated_at = earlier;
    succeeded.finished_at = Some(earlier);
    succeeded.next_attempt_at = None;

    let mut cancelled = test_review_job(project_id, 42, "head-b", None);
    cancelled.id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").expect("job id");
    cancelled.status = ProjectReviewJobStatus::Cancelled;
    cancelled.attempt_count = 1;
    cancelled.created_at = shared;
    cancelled.updated_at = shared;
    cancelled.finished_at = Some(shared);
    cancelled.next_attempt_at = None;

    let mut skipped = test_review_job(project_id, 42, "head-c", None);
    skipped.id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").expect("job id");
    skipped.status = ProjectReviewJobStatus::Skipped;
    skipped.created_at = shared;
    skipped.updated_at = shared;
    skipped.finished_at = Some(shared);
    skipped.next_attempt_at = None;

    let mut failed = test_review_job(project_id, 43, "head-d", None);
    failed.id = Uuid::parse_str("00000000-0000-0000-0000-000000000004").expect("job id");
    failed.status = ProjectReviewJobStatus::Failed;
    failed.created_at = latest;
    failed.updated_at = latest;
    failed.finished_at = Some(latest);
    failed.next_attempt_at = None;

    let mut second_succeeded = test_review_job(project_id, 44, "head-e", None);
    second_succeeded.id = Uuid::parse_str("00000000-0000-0000-0000-000000000005").expect("job id");
    second_succeeded.status = ProjectReviewJobStatus::Succeeded;
    second_succeeded.created_at = latest;
    second_succeeded.updated_at = latest;
    second_succeeded.finished_at = Some(latest);
    second_succeeded.next_attempt_at = None;

    let mut active = test_review_job(project_id, 45, "head-f", None);
    active.id = Uuid::parse_str("00000000-0000-0000-0000-000000000006").expect("job id");
    active.created_at = latest + chrono::TimeDelta::hours(1);
    active.updated_at = active.created_at;

    for job in [
        &succeeded,
        &cancelled,
        &skipped,
        &failed,
        &second_succeeded,
        &active,
    ] {
        store
            .save_project_review_job(job.clone())
            .await
            .expect("save review job");
    }
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: Uuid::new_v4(),
                job_id: Some(cancelled.id),
                attempt_index: 1,
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at: shared,
                finished_at: Some(shared),
                status: ProjectReviewRunStatus::Cancelled,
                outcome: None,
                review_event: None,
                pr: Some(42),
                summary: None,
                error: None,
                failure: None,
                token_usage: TokenUsage::default(),
                history_status: Default::default(),
                history_archive_id: None,
                history_archived_at: None,
            },
            history: None,
        })
        .await
        .expect("save cancelled attempt");

    let first_page = store
        .load_project_pull_request_reviews(project_id, 1, 2)
        .await
        .expect("load first review page");
    assert_eq!(
        first_page,
        ProjectPullRequestReviewPage {
            reviews: vec![
                ProjectPullRequestReviewSummary {
                    pr: 45,
                    latest_job: active,
                    history_count: 1,
                    lifecycle_state: ProjectPullRequestLifecycleState::Open,
                    state_changed_at: None,
                },
                ProjectPullRequestReviewSummary {
                    pr: 44,
                    latest_job: second_succeeded,
                    history_count: 1,
                    lifecycle_state: ProjectPullRequestLifecycleState::Open,
                    state_changed_at: None,
                },
            ],
            page: 1,
            page_size: 2,
            total_items: 4,
            total_pages: 2,
            summary: ProjectPullRequestReviewStatusSummary {
                active: 1,
                succeeded: 1,
                skipped: 1,
                failed: 1,
            },
        }
    );
    let second_page = store
        .load_project_pull_request_reviews(project_id, 2, 2)
        .await
        .expect("load second review page");
    assert_eq!(
        second_page,
        ProjectPullRequestReviewPage {
            reviews: vec![
                ProjectPullRequestReviewSummary {
                    pr: 43,
                    latest_job: failed,
                    history_count: 1,
                    lifecycle_state: ProjectPullRequestLifecycleState::Open,
                    state_changed_at: None,
                },
                ProjectPullRequestReviewSummary {
                    pr: 42,
                    latest_job: skipped.clone(),
                    history_count: 3,
                    lifecycle_state: ProjectPullRequestLifecycleState::Open,
                    state_changed_at: None,
                },
            ],
            page: 2,
            page_size: 2,
            total_items: 4,
            total_pages: 2,
            summary: ProjectPullRequestReviewStatusSummary {
                active: 1,
                succeeded: 1,
                skipped: 1,
                failed: 1,
            },
        }
    );

    let history = store
        .load_project_pull_request_review_history(project_id, 42, 1, 2)
        .await
        .expect("load first history page");
    assert_eq!(
        history,
        ProjectPullRequestReviewHistoryPage {
            items: vec![
                ProjectPullRequestReviewHistoryItem { job: skipped },
                ProjectPullRequestReviewHistoryItem { job: cancelled },
            ],
            page: 1,
            page_size: 2,
            total_items: 3,
            total_pages: 2,
        }
    );
    let history_tail = store
        .load_project_pull_request_review_history(project_id, 42, 2, 2)
        .await
        .expect("load second history page");
    assert_eq!(
        history_tail,
        ProjectPullRequestReviewHistoryPage {
            items: vec![ProjectPullRequestReviewHistoryItem { job: succeeded }],
            page: 2,
            page_size: 2,
            total_items: 3,
            total_pages: 2,
        }
    );
}

#[tokio::test]
async fn pull_request_review_pages_sort_active_approved_other_merged_then_closed() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let timestamp = DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    for (index, (pr, status, event)) in [
        (
            10,
            ProjectReviewJobStatus::Succeeded,
            Some(ProjectReviewDecision::Approve),
        ),
        (
            11,
            ProjectReviewJobStatus::Succeeded,
            Some(ProjectReviewDecision::Approve),
        ),
        (
            12,
            ProjectReviewJobStatus::Succeeded,
            Some(ProjectReviewDecision::RequestChanges),
        ),
        (
            13,
            ProjectReviewJobStatus::Succeeded,
            Some(ProjectReviewDecision::Approve),
        ),
        (
            14,
            ProjectReviewJobStatus::Succeeded,
            Some(ProjectReviewDecision::Comment),
        ),
        (15, ProjectReviewJobStatus::Queued, None),
        (16, ProjectReviewJobStatus::Running, None),
    ]
    .into_iter()
    .enumerate()
    {
        let mut job = test_review_job(project_id, pr, &format!("head-{pr}"), None);
        job.id = Uuid::from_u128(u128::try_from(index + 1).expect("job id"));
        job.status = status;
        job.created_at =
            timestamp + chrono::TimeDelta::minutes(i64::try_from(index).expect("timestamp offset"));
        job.updated_at = job.created_at;
        job.finished_at = job.status.is_terminal().then_some(job.created_at);
        job.next_attempt_at = None;
        job.submission_receipt = event.map(|event| ProjectReviewSubmissionReceipt {
            github_review_id: pr,
            event,
            head_sha: job.head_sha.clone(),
            html_url: None,
            submitted_at: job.created_at,
        });
        store
            .save_project_review_job(job.clone())
            .await
            .expect("save review job");
    }
    let detected_at = timestamp + chrono::TimeDelta::hours(1);
    let inserted = store
        .save_pull_request_state_observations(
            project_id,
            vec![
                PersistedPullRequestStateObservation {
                    pr: 13,
                    state: ProjectPullRequestLifecycleState::Merged,
                    state_changed_at: Some(detected_at),
                    detected_at,
                },
                PersistedPullRequestStateObservation {
                    pr: 14,
                    state: ProjectPullRequestLifecycleState::Closed,
                    state_changed_at: Some(detected_at),
                    detected_at,
                },
            ],
        )
        .await
        .expect("save merged pull requests");
    assert_eq!(
        inserted,
        PersistedPullRequestStateSaveSummary {
            newly_merged: 1,
            newly_closed: 1,
        }
    );
    assert_eq!(
        store
            .save_pull_request_state_observations(
                project_id,
                vec![PersistedPullRequestStateObservation {
                    pr: 13,
                    state: ProjectPullRequestLifecycleState::Merged,
                    state_changed_at: Some(detected_at + chrono::TimeDelta::hours(1)),
                    detected_at: detected_at + chrono::TimeDelta::hours(1),
                }],
            )
            .await
            .expect("repeat merged save"),
        PersistedPullRequestStateSaveSummary::default()
    );

    let first_page = store
        .load_project_pull_request_reviews(project_id, 1, 4)
        .await
        .expect("first page");
    let second_page = store
        .load_project_pull_request_reviews(project_id, 2, 4)
        .await
        .expect("second page");
    assert_eq!(
        first_page
            .reviews
            .iter()
            .map(|review| (review.pr, review.lifecycle_state))
            .collect::<Vec<_>>(),
        vec![
            (16, ProjectPullRequestLifecycleState::Open),
            (15, ProjectPullRequestLifecycleState::Open),
            (11, ProjectPullRequestLifecycleState::Open),
            (10, ProjectPullRequestLifecycleState::Open),
        ]
    );
    assert_eq!(
        second_page
            .reviews
            .iter()
            .map(|review| (review.pr, review.lifecycle_state, review.state_changed_at))
            .collect::<Vec<_>>(),
        vec![
            (12, ProjectPullRequestLifecycleState::Open, None),
            (
                13,
                ProjectPullRequestLifecycleState::Merged,
                Some(detected_at)
            ),
            (
                14,
                ProjectPullRequestLifecycleState::Closed,
                Some(detected_at)
            ),
        ]
    );
    assert_eq!(
        store
            .load_refreshable_project_review_prs(project_id)
            .await
            .expect("load unmerged pull requests"),
        vec![10, 11, 12, 14, 15, 16]
    );
    assert_eq!(
        store
            .load_project_pull_request_state(project_id, 13)
            .await
            .expect("merged lookup"),
        Some(ProjectPullRequestLifecycleState::Merged)
    );
    assert_eq!(
        store
            .load_project_pull_request_state(project_id, 14)
            .await
            .expect("closed lookup"),
        Some(ProjectPullRequestLifecycleState::Closed)
    );
}

#[tokio::test]
async fn pull_request_lifecycle_state_is_isolated_by_project() {
    let (_dir, store) = store().await;
    let merged_project = Uuid::new_v4();
    let open_project = Uuid::new_v4();
    for project_id in [merged_project, open_project] {
        store
            .save_project_review_job(test_review_job(project_id, 42, "head-42", None))
            .await
            .expect("save review job");
    }
    let timestamp = Utc::now();
    store
        .save_pull_request_state_observations(
            merged_project,
            vec![PersistedPullRequestStateObservation {
                pr: 42,
                state: ProjectPullRequestLifecycleState::Merged,
                state_changed_at: Some(timestamp),
                detected_at: timestamp,
            }],
        )
        .await
        .expect("save merged state");

    assert_eq!(
        store
            .load_refreshable_project_review_prs(merged_project)
            .await
            .expect("merged project"),
        Vec::<u64>::new()
    );
    assert_eq!(
        store
            .load_refreshable_project_review_prs(open_project)
            .await
            .expect("open project"),
        vec![42]
    );
    assert_eq!(
        store
            .load_project_pull_request_state(merged_project, 42)
            .await
            .expect("merged lookup"),
        Some(ProjectPullRequestLifecycleState::Merged)
    );
    assert_eq!(
        store
            .load_project_pull_request_state(open_project, 42)
            .await
            .expect("open lookup"),
        None
    );
    assert_eq!(
        store
            .load_project_pull_request_reviews(open_project, 1, 20)
            .await
            .expect("open aggregate")
            .reviews[0]
            .lifecycle_state,
        ProjectPullRequestLifecycleState::Open
    );
}

#[tokio::test]
async fn non_open_pull_requests_cannot_be_enqueued_through_atomic_admission() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let timestamp = Utc::now();
    for (pr, state) in [
        (42, ProjectPullRequestLifecycleState::Merged),
        (43, ProjectPullRequestLifecycleState::Closed),
    ] {
        store
            .save_pull_request_state_observations(
                project_id,
                vec![PersistedPullRequestStateObservation {
                    pr,
                    state,
                    state_changed_at: Some(timestamp),
                    detected_at: timestamp,
                }],
            )
            .await
            .expect("save terminal state");

        let result = store
            .enqueue_reviewable_project_review_job(test_review_job(
                project_id,
                pr,
                "terminal-head",
                None,
            ))
            .await
            .expect("terminal admission");
        assert!(matches!(
            result,
            ProjectReviewReviewableJobEnqueueResult::NotOpen(actual) if actual == state
        ));
        assert!(
            store
                .load_project_pull_request_review_history(project_id, pr, 1, 20)
                .await
                .expect("review history")
                .items
                .is_empty()
        );
    }
}

#[tokio::test]
async fn reopened_pull_request_clears_closed_state_and_becomes_reviewable() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let timestamp = Utc::now();
    store
        .save_pull_request_state_observations(
            project_id,
            vec![PersistedPullRequestStateObservation {
                pr: 42,
                state: ProjectPullRequestLifecycleState::Closed,
                state_changed_at: Some(timestamp),
                detected_at: timestamp,
            }],
        )
        .await
        .expect("save closed state");
    assert_eq!(
        store
            .load_refreshable_project_review_prs(project_id)
            .await
            .expect("refreshable before a job exists"),
        Vec::<u64>::new()
    );
    store
        .save_project_review_job(test_review_job(project_id, 42, "reopened-head", None))
        .await
        .expect("save review history");
    assert_eq!(
        store
            .load_refreshable_project_review_prs(project_id)
            .await
            .expect("closed state remains refreshable"),
        vec![42]
    );
    store
        .save_pull_request_state_observations(
            project_id,
            vec![PersistedPullRequestStateObservation {
                pr: 42,
                state: ProjectPullRequestLifecycleState::Open,
                state_changed_at: None,
                detected_at: timestamp,
            }],
        )
        .await
        .expect("save reopened state");
    assert_eq!(
        store
            .load_project_pull_request_state(project_id, 42)
            .await
            .expect("reopened state"),
        None
    );
}

fn test_review_job(
    project_id: Uuid,
    pr: u64,
    head_sha: &str,
    delivery_id: Option<&str>,
) -> ProjectReviewJobSummary {
    let timestamp = Utc::now();
    ProjectReviewJobSummary {
        id: Uuid::new_v4(),
        project_id,
        pr,
        head_sha: head_sha.to_string(),
        source: ProjectReviewJobSource::Webhook,
        delivery_id: delivery_id.map(ToString::to_string),
        reason: "test".to_string(),
        status: ProjectReviewJobStatus::Queued,
        attempt_count: 0,
        max_attempts: 5,
        first_retryable_failure_at: None,
        next_attempt_at: Some(timestamp),
        reviewer_agent_id: None,
        active_run_id: None,
        lease_owner: None,
        lease_expires_at: None,
        failure: None,
        environment_warning: None,
        skip_reason: None,
        submission_intent: None,
        submission_receipt: None,
        created_at: timestamp,
        updated_at: timestamp,
        finished_at: None,
    }
}
