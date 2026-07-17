use super::*;
use crate::schema::{SCHEMA_VERSION, SETTING_SCHEMA_VERSION};
use mai_protocol::{
    McpServerScope, McpServerTransport, MessageRole, ProjectCloneStatus, ProjectReviewDecision,
    ProjectReviewOutcome, ProjectReviewRunStatus, ProjectReviewStatus, ProjectStatus,
    ServiceEventKind, TurnStatus,
};
use serde_json::json;
use std::collections::BTreeMap;
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

fn test_service_event(
    sequence: u64,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    timestamp: DateTime<Utc>,
) -> ServiceEvent {
    ServiceEvent {
        sequence,
        timestamp,
        kind: ServiceEventKind::TurnCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            status: TurnStatus::Completed,
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
async fn service_event_replay_and_snapshot_keep_recent_events() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();

    for sequence in 1..=5 {
        store
            .append_service_event(&test_service_event(
                sequence,
                agent_id,
                session_id,
                turn_id,
                Utc::now(),
            ))
            .await
            .expect("append event");
    }

    let replay = store.service_events_after(2, 2).await.expect("replay");
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
async fn service_event_count_pruning_keeps_newest_events() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();

    for sequence in 1..=5 {
        store
            .append_service_event(&test_service_event(
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
        .prune_service_events_to_limit(3)
        .await
        .expect("prune by limit");
    assert_eq!(removed, 2);

    let replay = store.service_events_after(0, 10).await.expect("replay");
    assert_eq!(
        replay
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
    assert_eq!(
        store.prune_service_events_to_limit(3).await.expect("noop"),
        0
    );
    assert_eq!(
        store
            .prune_service_events_to_limit(0)
            .await
            .expect("zero limit"),
        3
    );
    assert!(
        store
            .service_events_after(0, 10)
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
            events: vec![ServiceEvent {
                sequence: 1,
                timestamp: finished_at,
                kind: ServiceEventKind::TurnCompleted {
                    agent_id: reviewer_agent_id,
                    session_id: None,
                    turn_id,
                    status: TurnStatus::Completed,
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
        tool_name: "container_exec".to_string(),
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
                    tool_name: "container_exec".to_string(),
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
