use super::*;
use crate::schema::{SCHEMA_VERSION, SETTING_SCHEMA_VERSION};
use mai_protocol::{
    AgentResourceState, AgentRole, AgentState, ErrorSeverity, McpServerScope, McpServerTransport,
    MessageRole, ProjectCloneStatus, ProjectReviewDecision, ProjectReviewJobSource,
    ProjectReviewJobStatus, ProjectReviewOutcome, ProjectReviewRunStatus, ProjectReviewStatus,
    ProjectReviewSubmissionIntent, ProjectReviewSubmissionReceipt, ProjectStatus, SessionEventKind,
    SessionEventPosition,
};
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
    _session_id: SessionId,
    _turn_id: TurnId,
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

    let cleared = store
        .save_relay_settings(RelaySettingsRequest {
            enabled: false,
            url: None,
            token: Some("   ".to_string()),
            node_id: None,
        })
        .await
        .expect("clear relay token");
    assert!(!cleared.enabled);
    assert!(!cleared.has_token);
    assert_eq!(cleared.url, "http://127.0.0.1:8090");
    assert_eq!(cleared.node_id, "mai-server");
}

#[tokio::test]
async fn github_app_settings_persist_public_url() {
    let (_dir, store) = store().await;

    let saved = store
        .save_github_app_settings(GithubAppSettingsRequest {
            app_id: Some("123".to_string()),
            private_key: Some("pem".to_string()),
            base_url: Some("https://api.github.com/".to_string()),
            public_url: Some(" https://relay.example/ ".to_string()),
            app_slug: Some("mai".to_string()),
            app_html_url: None,
            owner_login: None,
            owner_type: None,
        })
        .await
        .expect("save github app");

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
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();

    for sequence in 1..=5 {
        store
            .append_product_event(&test_product_event(
                sequence,
                agent_id,
                session_id,
                turn_id,
                Utc::now(),
            ))
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
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();

    for sequence in 1..=5 {
        store
            .append_product_event(&test_product_event(
                sequence,
                agent_id,
                session_id,
                turn_id,
                Utc::now(),
            ))
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
async fn project_review_runs_round_trip_and_prune() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let reviewer_agent_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
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
                turn_id: Some(turn_id),
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
                    input_tokens: 100,
                    cached_input_tokens: 60,
                    output_tokens: 20,
                    reasoning_output_tokens: 5,
                    total_tokens: 120,
                },
            },
            messages: vec![AgentMessage {
                role: MessageRole::Assistant,
                content: "done".to_string(),
                created_at: finished_at,
            }],
            events: vec![SessionEventEnvelope {
                event_id: "event-1".to_string(),
                session_id: Uuid::new_v4().to_string(),
                source_agent_id: Some(reviewer_agent_id.to_string()),
                turn_id: Some(turn_id.to_string()),
                emitted_at: finished_at.timestamp_millis(),
                position: SessionEventPosition::Durable { sequence: 1 },
                kind: SessionEventKind::ErrorOccurred {
                    message: "historical test event".to_string(),
                    severity: ErrorSeverity::Recoverable,
                },
            }],
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
            input_tokens: 100,
            cached_input_tokens: 60,
            output_tokens: 20,
            reasoning_output_tokens: 5,
            total_tokens: 120,
        }
    );
    let detail = store
        .load_project_review_run(project_id, run_id)
        .await
        .expect("detail")
        .expect("run exists");
    assert_eq!(detail.messages[0].content, "done");
    assert_eq!(detail.events.len(), 1);

    let removed = store
        .prune_project_review_runs_before(Utc::now() - chrono::TimeDelta::days(2))
        .await
        .expect("no prune");
    assert_eq!(removed, 0);
    let removed = store
        .prune_project_review_runs_before(Utc::now())
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
async fn agent_logs_round_trip_filter_and_prune() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let old_time = Utc::now() - chrono::TimeDelta::days(6);
    let new_time = Utc::now();

    store
        .append_agent_log_entry(&AgentLogEntry {
            id: Uuid::new_v4(),
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
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
            session_id: None,
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
                session_id: Some(session_id),
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
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let old_time = Utc::now() - chrono::TimeDelta::days(6);
    let new_time = Utc::now();

    let trace = ToolTraceDetail {
        agent_id,
        session_id: Some(session_id),
        turn_id: Some(turn_id),
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
                session_id: Some(session_id),
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
        .load_tool_trace(agent_id, Some(session_id), "call_1")
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
    let first_session_id = Uuid::new_v4();
    let second_session_id = Uuid::new_v4();
    let timestamp = Utc::now();

    for (agent_id, session_id, command) in [
        (first_agent_id, first_session_id, "pwd"),
        (second_agent_id, second_session_id, "ls"),
    ] {
        store
            .save_tool_trace_completed(
                &ToolTraceDetail {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id: Some(Uuid::new_v4()),
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
        .load_tool_trace(first_agent_id, Some(first_session_id), "call_duplicate")
        .await
        .expect("load first")
        .expect("first trace");
    let second = store
        .load_tool_trace(second_agent_id, Some(second_session_id), "call_duplicate")
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
            },
            messages: Vec::new(),
            events: Vec::new(),
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
async fn invalid_sqlite_file_is_rebuilt() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config.sqlite3");
    std::fs::write(&path, b"not sqlite").expect("write invalid old db");
    let store = MaiStore::open_with_config_path(&path, dir.path().join("config.toml"))
        .await
        .expect("rebuild");
    assert_eq!(
        store
            .get_setting(SETTING_SCHEMA_VERSION)
            .await
            .expect("schema"),
        Some(SCHEMA_VERSION.to_string())
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
        config: vec![mai_protocol::SkillConfigEntry {
            name: Some("demo".to_string()),
            path: None,
            enabled: false,
        }],
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
async fn schema_version_mismatch_rebuilds_database() {
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

    let reopened = MaiStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("reopen");
    assert_eq!(
        reopened
            .get_setting(SETTING_SCHEMA_VERSION)
            .await
            .expect("schema marker")
            .as_deref(),
        Some(SCHEMA_VERSION)
    );
    assert!(
        reopened
            .list_mcp_servers()
            .await
            .expect("servers")
            .is_empty()
    );
}

#[tokio::test]
async fn schema_22_review_runs_migrate_without_rebuilding_user_data() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = MaiStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open");
    let project_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let started_at = Utc::now() - chrono::TimeDelta::minutes(2);
    store
        .save_mcp_servers(&BTreeMap::from([(
            "preserved".to_string(),
            McpServerConfig {
                command: Some("preserved-mcp".to_string()),
                ..Default::default()
            },
        )]))
        .await
        .expect("save preserved data");
    store
        .save_agent(
            &AgentSummary {
                id: reviewer_id,
                parent_id: None,
                task_id: None,
                project_id: Some(project_id),
                role: Some(AgentRole::Reviewer),
                name: "legacy reviewer".to_string(),
                state: AgentState {
                    resource: AgentResourceState::Ready,
                    ..AgentState::default()
                },
                container_id: None,
                docker_image: "ubuntu:latest".to_string(),
                provider_id: "provider".to_string(),
                provider_name: "Provider".to_string(),
                model: "model".to_string(),
                reasoning_effort: None,
                created_at: started_at,
                updated_at: started_at,
                token_usage: TokenUsage::default(),
            },
            Some(
                "You are an autonomous project pull request reviewer. Review exactly PR #42 at head `0123456789abcdef0123456789abcdef01234567` against base `base`.",
            ),
        )
        .await
        .expect("save legacy reviewer");
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: run_id,
                job_id: None,
                attempt_index: 1,
                project_id,
                reviewer_agent_id: Some(reviewer_id),
                turn_id: Some(Uuid::new_v4()),
                started_at,
                finished_at: None,
                status: ProjectReviewRunStatus::Running,
                outcome: None,
                review_event: None,
                pr: Some(42),
                summary: None,
                error: None,
                failure: None,
                token_usage: TokenUsage::default(),
            },
            messages: Vec::new(),
            events: Vec::new(),
        })
        .await
        .expect("save legacy run");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open legacy sqlite");
    connection
        .execute_batch(
            "ALTER TABLE project_review_runs RENAME TO project_review_runs_v23;
             CREATE TABLE project_review_runs (
                id TEXT PRIMARY KEY NOT NULL,
                project_id TEXT NOT NULL,
                reviewer_agent_id TEXT,
                turn_id TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                status TEXT NOT NULL,
                outcome TEXT,
                review_event TEXT,
                pr INTEGER,
                summary TEXT,
                error TEXT,
                input_tokens INTEGER NOT NULL,
                cached_input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                reasoning_output_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                messages_json TEXT NOT NULL,
                events_json TEXT NOT NULL
             );
             INSERT INTO project_review_runs (
                id, project_id, reviewer_agent_id, turn_id, started_at, finished_at,
                status, outcome, review_event, pr, summary, error, input_tokens,
                cached_input_tokens, output_tokens, reasoning_output_tokens, total_tokens,
                messages_json, events_json
             ) SELECT id, project_id, reviewer_agent_id, turn_id, started_at, finished_at,
                status, outcome, review_event, pr, summary, error, input_tokens,
                cached_input_tokens, output_tokens, reasoning_output_tokens, total_tokens,
                messages_json, events_json FROM project_review_runs_v23;
             DROP TABLE project_review_runs_v23;
             DROP TABLE project_review_jobs;
             CREATE INDEX project_review_runs_project_id_idx ON project_review_runs(project_id);
             CREATE INDEX project_review_runs_started_at_idx ON project_review_runs(started_at);
             UPDATE settings SET value = '22' WHERE key = 'toasty_schema_version';",
        )
        .expect("downgrade fixture to schema 22");
    drop(connection);

    let reopened = MaiStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("migrate schema 22");
    assert!(
        reopened
            .list_mcp_servers()
            .await
            .expect("servers")
            .contains_key("preserved")
    );
    let run = reopened
        .load_project_review_run(project_id, run_id)
        .await
        .expect("load run")
        .expect("migrated run");
    assert_eq!(Some(run_id), run.summary.job_id);
    assert_eq!(ProjectReviewRunStatus::Interrupted, run.summary.status);
    let job = reopened
        .load_project_review_job(project_id, run_id)
        .await
        .expect("load job")
        .expect("migrated job");
    assert_eq!(ProjectReviewJobStatus::RetryWaiting, job.status);
    assert_eq!(Some(reviewer_id), job.reviewer_agent_id);
    assert_eq!(Some(run_id), job.active_run_id);
    assert_eq!(1, job.attempt_count);
    assert_eq!("0123456789abcdef0123456789abcdef01234567", job.head_sha);
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
        .load_project_review_jobs(project_id, 0, 10)
        .await
        .expect("load jobs");
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
async fn active_review_job_projection_prioritizes_execution_over_waiting() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let mut waiting = test_review_job(project_id, 41, "head-waiting", None);
    waiting.status = ProjectReviewJobStatus::RetryWaiting;
    waiting.next_attempt_at = Some(Utc::now() + chrono::TimeDelta::minutes(2));
    store
        .save_project_review_job(waiting.clone())
        .await
        .expect("save waiting job");

    let mut running = test_review_job(project_id, 42, "head-running", None);
    running.created_at += chrono::TimeDelta::seconds(1);
    running.updated_at = running.created_at;
    running.status = ProjectReviewJobStatus::Running;
    running.reviewer_agent_id = Some(Uuid::new_v4());
    store
        .save_project_review_job(running.clone())
        .await
        .expect("save running job");

    assert_eq!(
        Some(running.id),
        store
            .load_active_project_review_job(project_id)
            .await
            .expect("load active job")
            .map(|job| job.id)
    );

    running.status = ProjectReviewJobStatus::Succeeded;
    running.finished_at = Some(Utc::now());
    store
        .save_project_review_job(running)
        .await
        .expect("finish running job");
    assert_eq!(
        Some(waiting.id),
        store
            .load_active_project_review_job(project_id)
            .await
            .expect("load waiting job")
            .map(|job| job.id)
    );
}

#[tokio::test]
async fn delayed_review_job_blocks_newer_job_until_its_retry_is_due() {
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
    store
        .save_project_review_job(queued)
        .await
        .expect("save queued job");

    assert!(
        store
            .claim_due_project_review_job(
                project_id,
                "owner".to_string(),
                current_time,
                current_time + chrono::TimeDelta::seconds(60),
            )
            .await
            .expect("claim before retry")
            .is_none()
    );
    let claimed = store
        .claim_due_project_review_job(
            project_id,
            "owner".to_string(),
            current_time + chrono::TimeDelta::minutes(2),
            current_time + chrono::TimeDelta::minutes(3),
        )
        .await
        .expect("claim due retry")
        .expect("waiting job becomes due");
    assert_eq!(waiting.id, claimed.id);
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
            .load_project_review_jobs(project_id, 0, 10)
            .await
            .expect("load webhook jobs")
            .len()
    );
}

#[tokio::test]
async fn concurrent_review_job_claim_has_one_winner() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    store
        .enqueue_project_review_job(test_review_job(project_id, 7, "head", None))
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
    assert_eq!(1, winners[0].attempt_count);
    assert_eq!(ProjectReviewJobStatus::Preparing, winners[0].status);
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
    assert_eq!(None, completed.next_attempt_at);
    assert_eq!(None, completed.failure);
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
        submission_intent: None,
        submission_receipt: None,
        created_at: timestamp,
        updated_at: timestamp,
        finished_at: None,
    }
}
