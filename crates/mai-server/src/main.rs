mod handlers;

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{get, post};
use clap::Parser;
use mai_docker::DockerClient;
use mai_model::ModelClient;
use mai_runtime::RuntimeConfig;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use handlers::state::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mai_server=info,mai_runtime=info,tower_http=info".into()),
        )
        .init();

    let api_key = env::var("OPENAI_API_KEY").ok();
    let base_url =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.5".to_string());
    let data_dir = handlers::helpers::data_dir_path(cli.data_path)?;
    let cache_dir = handlers::helpers::cache_dir_path(&data_dir);
    let projects_root = data_dir.join("projects");
    let artifact_files_root = handlers::helpers::artifact_files_root(&data_dir);
    let artifact_index_root = handlers::helpers::artifact_index_root(&data_dir);
    let image = env::var("MAI_AGENT_BASE_IMAGE")
        .unwrap_or_else(|_| "ghcr.io/zr233/mai-team-agent:latest".to_string());
    let sidecar_image = env::var("MAI_SIDECAR_IMAGE")
        .unwrap_or_else(|_| "ghcr.io/zr233/mai-team-sidecar:latest".to_string());
    let bind = env::var("MAI_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let addr: SocketAddr = bind.parse().context("invalid MAI_BIND_ADDR")?;

    let docker = DockerClient::new(image);
    let docker_version = docker.check_available().await?;
    info!("docker available: {docker_version}");

    fs::create_dir_all(&cache_dir)?;
    fs::create_dir_all(&projects_root)?;
    fs::create_dir_all(&artifact_files_root)?;
    fs::create_dir_all(&artifact_index_root)?;

    let store = Arc::new(mai_store::ConfigStore::open_in_data_dir(&data_dir).await?);
    store
        .seed_default_provider_from_env(api_key, base_url, model)
        .await?;

    let system_skills_root = handlers::assets::system_skills_path(&data_dir);
    handlers::assets::release_embedded_system_skills(&system_skills_root)?;
    info!(
        path = %system_skills_root.display(),
        "released embedded system skills"
    );
    let system_agents_root = handlers::assets::system_agents_path(&data_dir);
    handlers::assets::release_embedded_system_agents(&system_agents_root)?;
    info!(
        path = %system_agents_root.display(),
        "released embedded system agents"
    );

    let model_client = ModelClient::new();
    let runtime_config = RuntimeConfig {
        repo_root: env::current_dir()?,
        projects_root: projects_root.clone(),
        cache_root: cache_dir.clone(),
        artifact_files_root: artifact_files_root.clone(),
        sidecar_image,
        github_api_base_url: None,
        git_binary: None,
        system_skills_root: Some(system_skills_root),
        system_agents_root: Some(system_agents_root),
    };
    info!(
        data_dir = %data_dir.display(),
        cache_dir = %cache_dir.display(),
        projects_root = %projects_root.display(),
        artifact_files_root = %artifact_files_root.display(),
        artifact_index_root = %artifact_index_root.display(),
        "runtime storage paths"
    );
    let relay = handlers::helpers::relay_config_from_env()
        .map(|config| Arc::new(mai_relay_client::RelayClient::new(config)));
    let github_backend = relay
        .as_ref()
        .map(|client| Arc::clone(client) as Arc<dyn mai_runtime::github::GithubAppBackend>);
    let runtime = mai_runtime::AgentRuntime::new_with_github_backend(
        docker,
        model_client,
        Arc::clone(&store),
        runtime_config,
        github_backend,
    )
    .await?;
    if let Some(relay) = &relay {
        relay.set_runtime(Arc::clone(&runtime)).await;
        Arc::clone(relay).start();
    }
    let cleaned = runtime.cleanup_orphaned_containers().await?;
    if !cleaned.is_empty() {
        info!(
            count = cleaned.len(),
            "removed orphaned mai-team containers"
        );
    }
    let state = Arc::new(handlers::state::AppState {
        runtime,
        store,
        relay,
    });

    let app = Router::new()
        .route("/", get(handlers::assets::index))
        .route("/health", get(handlers::assets::health))
        .route(
            "/providers",
            get(handlers::providers::get_providers).put(handlers::providers::save_providers),
        )
        .route(
            "/providers/{id}/test",
            post(handlers::providers::test_provider),
        )
        .route(
            "/mcp-servers",
            get(handlers::providers::get_mcp_servers).put(handlers::providers::save_mcp_servers),
        )
        .route(
            "/git/accounts",
            get(handlers::git_accounts::list_git_accounts)
                .post(handlers::git_accounts::save_git_account),
        )
        .route(
            "/git/accounts/default",
            axum::routing::put(handlers::git_accounts::set_default_git_account),
        )
        .route(
            "/git/accounts/{id}",
            axum::routing::put(handlers::git_accounts::save_git_account_by_id)
                .delete(handlers::git_accounts::delete_git_account),
        )
        .route(
            "/git/accounts/{id}/verify",
            post(handlers::git_accounts::verify_git_account),
        )
        .route(
            "/git/accounts/{id}/repositories",
            get(handlers::git_accounts::list_git_account_repositories),
        )
        .route(
            "/git/accounts/{id}/repositories/{owner}/{repo}/packages",
            get(handlers::git_accounts::list_git_account_repository_packages),
        )
        .route(
            "/runtime/defaults",
            get(handlers::config::get_runtime_defaults),
        )
        .route(
            "/settings/github",
            get(handlers::github_app::get_github_settings)
                .put(handlers::github_app::save_github_settings),
        )
        .route(
            "/settings/github-app",
            get(handlers::github_app::get_github_app_settings)
                .put(handlers::github_app::save_github_app_settings),
        )
        .route(
            "/github/app-manifest/start",
            post(handlers::github_app::start_github_app_manifest),
        )
        .route(
            "/github/app-manifest/callback",
            get(handlers::github_app::complete_github_app_manifest),
        )
        .route(
            "/github/app-installation/callback",
            get(handlers::github_app::github_app_installation_callback),
        )
        .route(
            "/github/app-installation/start",
            post(handlers::github_app::start_github_app_installation),
        )
        .route("/relay/status", get(handlers::github_app::get_relay_status))
        .route(
            "/github/installations",
            get(handlers::github_app::list_github_installations),
        )
        .route(
            "/github/installations:refresh",
            post(handlers::github_app::refresh_github_installations),
        )
        .route(
            "/github/installations/{id}/repositories",
            get(handlers::github_app::list_github_repositories),
        )
        .route(
            "/github/installations/{id}/repositories/{owner}/{repo}/packages",
            get(handlers::github_app::list_github_repository_packages),
        )
        .route(
            "/provider-presets",
            get(handlers::config::get_provider_presets),
        )
        .route("/skills", get(handlers::config::list_skills))
        .route(
            "/skills/config",
            axum::routing::put(handlers::config::save_skills_config),
        )
        .route(
            "/agent-profiles",
            get(handlers::config::list_agent_profiles),
        )
        .route(
            "/agent-profiles:reload",
            post(handlers::config::list_agent_profiles),
        )
        .route(
            "/agent-config",
            get(handlers::config::get_agent_config).put(handlers::config::save_agent_config),
        )
        .route("/events", get(handlers::events::events))
        .route(
            "/tasks",
            get(handlers::tasks::list_tasks).post(handlers::tasks::create_task),
        )
        .route(
            "/tasks:ensure-default",
            post(handlers::tasks::ensure_default_task),
        )
        .route(
            "/tasks/{id}",
            get(handlers::tasks::get_task).delete(handlers::tasks::delete_task),
        )
        .route(
            "/tasks/{id}/messages",
            post(handlers::tasks::send_task_message),
        )
        .route(
            "/tasks/{id}/plan:approve",
            post(handlers::tasks::approve_task_plan),
        )
        .route(
            "/tasks/{id}/plan:request-revision",
            post(handlers::tasks::request_plan_revision),
        )
        .route("/tasks/{id}/cancel", post(handlers::tasks::cancel_task))
        .route(
            "/projects",
            get(handlers::projects::list_projects).post(handlers::projects::create_project),
        )
        .route(
            "/projects/{id}",
            get(handlers::projects::get_project)
                .patch(handlers::projects::update_project)
                .delete(handlers::projects::delete_project),
        )
        .route(
            "/projects/{id}/messages",
            post(handlers::projects::send_project_message),
        )
        .route(
            "/projects/{id}/review-runs",
            get(handlers::projects::list_project_review_runs),
        )
        .route(
            "/projects/{id}/review-runs/{run_id}",
            get(handlers::projects::get_project_review_run),
        )
        .route(
            "/projects/{id}/skills",
            get(handlers::projects::list_project_skills),
        )
        .route(
            "/projects/{id}/skills/detect",
            post(handlers::projects::detect_project_skills),
        )
        .route(
            "/projects/{id}/cancel",
            post(handlers::projects::cancel_project),
        )
        .route(
            "/agents",
            get(handlers::agents::list_agents).post(handlers::agents::create_agent),
        )
        .route(
            "/agents/{id}",
            get(handlers::agents::get_agent)
                .delete(handlers::agents::delete_agent)
                .patch(handlers::agents::update_agent)
                .post(handlers::agents::cancel_agent_colon),
        )
        .route(
            "/agents/{id}/messages",
            post(handlers::agents::send_message),
        )
        .route(
            "/agents/{id}/sessions",
            post(handlers::agents::create_session),
        )
        .route(
            "/agents/{id}/sessions/{session_id}/messages",
            post(handlers::agents::send_session_message),
        )
        .route(
            "/agents/{id}/sessions/{session_id}/tool-calls/{call_id}",
            get(handlers::agents::get_session_tool_trace),
        )
        .route("/agents/{id}/logs", get(handlers::agents::list_agent_logs))
        .route(
            "/agents/{id}/tool-calls",
            get(handlers::agents::list_tool_traces),
        )
        .route(
            "/agents/{id}/tool-calls/{call_id}",
            get(handlers::agents::get_tool_trace),
        )
        .route(
            "/agents/{id}/turns/{turn_id}/cancel",
            post(handlers::agents::cancel_agent_turn),
        )
        .route(
            "/agents/{id}/files:upload",
            post(handlers::agents::upload_file),
        )
        .route(
            "/agents/{id}/files:download",
            get(handlers::agents::download_file),
        )
        .route(
            "/tasks/{id}/artifacts",
            get(handlers::tasks::list_artifacts),
        )
        .route(
            "/artifacts/{id}/download",
            get(handlers::tasks::download_artifact),
        )
        .route("/agents/{id}/cancel", post(handlers::agents::cancel_agent))
        .fallback(get(handlers::assets::static_fallback))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    println!("Open http://{addr}/");
    info!("mai-team listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use handlers::assets::{
        embedded_system_agent_relative_path, embedded_system_skill_relative_path,
        safe_embedded_relative_path,
    };
    use handlers::assets::{
        release_embedded_system_agents, release_embedded_system_skills, safe_system_resource_target,
    };
    use handlers::events::event_name;
    use handlers::helpers::{
        artifact_files_root, artifact_index_root, cache_dir_path, data_dir_path_with,
        relay_url_from_env_values,
    };
    use handlers::providers::{provider_config, provider_test_store, run_provider_test};
    use mai_protocol::{
        AgentId, ProviderKind, ProviderTestRequest, ServiceEvent, ServiceEventKind, SessionId,
        SkillActivationInfo, SkillScope, TurnId,
    };
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::path::{Path as FsPath, PathBuf};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex as TokioMutex;

    #[test]
    fn embedded_system_skills_release_to_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-skills");

        release_embedded_system_skills(&target).expect("release skills");

        let skill_path = target.join("reviewer-agent-review-pr").join("SKILL.md");
        let contents = fs::read_to_string(skill_path).expect("skill contents");
        assert!(contents.contains("name: reviewer-agent-review-pr"));
    }

    #[test]
    fn embedded_system_agents_release_to_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-agents");

        release_embedded_system_agents(&target).expect("release agents");

        let maintainer_path = target.join("project-maintainer").join("AGENT.md");
        let reviewer_path = target.join("project-reviewer").join("AGENT.md");
        let contents = fs::read_to_string(maintainer_path).expect("agent contents");
        assert!(contents.contains("id: project-maintainer"));
        assert!(reviewer_path.exists());
    }

    #[test]
    fn embedded_system_skills_release_overwrites_target_dir() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("system-skills");
        fs::create_dir_all(&target).expect("mkdir");
        fs::write(target.join("stale.txt"), "old").expect("write stale");

        release_embedded_system_skills(&target).expect("release skills");

        assert!(!target.join("stale.txt").exists());
        let expected = target.join("reviewer-agent-review-pr").join("SKILL.md");
        assert!(
            expected.exists(),
            "expected {}, found {:?}",
            expected.display(),
            list_relative_files(&target)
        );
    }

    fn list_relative_files(root: &FsPath) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                collect_relative_files(root, &entry.path(), &mut files);
            }
        }
        files.sort();
        files
    }

    fn collect_relative_files(root: &FsPath, path: &FsPath, files: &mut Vec<PathBuf>) {
        if path.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    collect_relative_files(root, &entry.path(), files);
                }
            }
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_path_buf());
        }
    }

    #[test]
    fn safe_embedded_relative_path_rejects_parent_components() {
        assert_eq!(
            safe_embedded_relative_path("reviewer-agent-review-pr/SKILL.md"),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(
            embedded_system_skill_relative_path("system-skills/reviewer-agent-review-pr/SKILL.md"),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(safe_embedded_relative_path("../SKILL.md"), None);
        assert_eq!(safe_embedded_relative_path("/tmp/SKILL.md"), None);
        assert_eq!(
            embedded_system_skill_relative_path(
                &FsPath::new(env!("OUT_DIR"))
                    .join("system-skills")
                    .join("reviewer-agent-review-pr")
                    .join("SKILL.md")
                    .to_string_lossy()
            ),
            Some(PathBuf::from("reviewer-agent-review-pr").join("SKILL.md"))
        );
        assert_eq!(
            embedded_system_agent_relative_path(
                &FsPath::new(env!("OUT_DIR"))
                    .join("system-agents")
                    .join("project-maintainer")
                    .join("AGENT.md")
                    .to_string_lossy()
            ),
            Some(PathBuf::from("project-maintainer").join("AGENT.md"))
        );
    }

    #[test]
    fn system_skills_release_rejects_root_target() {
        assert!(!safe_system_resource_target(std::path::Path::new("")));
        assert!(!safe_system_resource_target(std::path::Path::new("/")));
        assert!(safe_system_resource_target(std::path::Path::new(
            "/tmp/system-skills"
        )));
    }

    #[test]
    fn runtime_storage_paths_use_default_data_layout() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join(".mai-team");

        assert_eq!(data_dir_path_with(dir.path(), None), data_dir);
        assert_eq!(cache_dir_path(&data_dir), data_dir.join("cache"));
        assert_eq!(
            artifact_files_root(&data_dir),
            data_dir.join("artifacts").join("files")
        );
        assert_eq!(
            artifact_index_root(&data_dir),
            data_dir.join("artifacts").join("index")
        );
        assert_eq!(
            handlers::assets::system_skills_path(&data_dir),
            data_dir.join("system-skills")
        );
        assert_eq!(
            handlers::assets::system_agents_path(&data_dir),
            data_dir.join("system-agents")
        );
    }

    #[test]
    fn runtime_storage_paths_use_cli_data_path() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join("data-root");

        assert_eq!(
            data_dir_path_with(dir.path(), Some(data_dir.clone())),
            data_dir
        );
        assert_eq!(cache_dir_path(&data_dir), data_dir.join("cache"));
    }

    #[test]
    fn cli_parses_data_path() {
        let cli =
            Cli::try_parse_from(["mai-server", "--data-path", "/tmp/mai-data"]).expect("parse cli");
        assert_eq!(cli.data_path, Some(PathBuf::from("/tmp/mai-data")));

        let cli =
            Cli::try_parse_from(["mai-server", "--data-path=/tmp/mai-data"]).expect("parse cli");
        assert_eq!(cli.data_path, Some(PathBuf::from("/tmp/mai-data")));
    }

    #[test]
    fn cli_rejects_invalid_data_path_usage() {
        assert!(Cli::try_parse_from(["mai-server", "--data-path"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--unknown"]).is_err());
        assert!(Cli::try_parse_from(["mai-server", "--help"]).is_err());
    }

    #[test]
    fn relay_url_prefers_public_url_and_trims_trailing_slash() {
        assert_eq!(
            relay_url_from_env_values(
                Some("https://relay.example.com/"),
                Some("http://legacy.example.com")
            ),
            "https://relay.example.com"
        );
        assert_eq!(
            relay_url_from_env_values(None, Some("http://legacy.example.com/")),
            "http://legacy.example.com"
        );
        assert_eq!(
            relay_url_from_env_values(Some("  "), None),
            "http://127.0.0.1:8090"
        );
    }

    #[test]
    fn skills_activated_event_has_sse_name() {
        let event = ServiceEvent {
            sequence: 1,
            timestamp: mai_protocol::now(),
            kind: ServiceEventKind::SkillsActivated {
                agent_id: AgentId::new_v4(),
                session_id: Some(SessionId::new_v4()),
                turn_id: TurnId::new_v4(),
                skills: vec![SkillActivationInfo {
                    name: "demo".to_string(),
                    display_name: Some("Demo".to_string()),
                    path: std::path::PathBuf::from("/tmp/demo/SKILL.md"),
                    scope: SkillScope::Project,
                }],
            },
        };

        assert_eq!(event_name(&event), "skills_activated");
    }

    #[test]
    fn plan_updated_event_has_sse_name() {
        let event = ServiceEvent {
            sequence: 1,
            timestamp: mai_protocol::now(),
            kind: ServiceEventKind::PlanUpdated {
                task_id: TurnId::new_v4(),
                plan: mai_protocol::TaskPlan::default(),
            },
        };

        assert_eq!(event_name(&event), "plan_updated");
    }

    #[tokio::test]
    async fn service_event_replay_returns_events_after_sequence() {
        let dir = tempdir().expect("tempdir");
        let store = mai_store::ConfigStore::open_with_config_path(
            dir.path().join("server.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("open store");
        for sequence in 1..=3 {
            store
                .append_service_event(&ServiceEvent {
                    sequence,
                    timestamp: mai_protocol::now(),
                    kind: ServiceEventKind::Error {
                        agent_id: None,
                        session_id: None,
                        turn_id: None,
                        message: format!("event {sequence}"),
                    },
                })
                .await
                .expect("append event");
        }

        let replay = store.service_events_after(1, 10).await.expect("replay");
        assert_eq!(
            replay
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[tokio::test]
    async fn provider_test_succeeds_against_mock_responses_server() {
        let (base_url, requests) = start_provider_mock(vec![
            json!({
                "id": "resp_test_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 3, "output_tokens": 2, "total_tokens": 5 }
            }),
            json!({
                "id": "resp_test_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 4, "output_tokens": 2, "total_tokens": 6 }
            }),
        ])
        .await;
        let (_dir, store) = provider_test_store(provider_config(&base_url, Some("secret"))).await;

        let response = run_provider_test(
            &store,
            "openai",
            ProviderTestRequest {
                model: None,
                reasoning_effort: Some("minimal".to_string()),
                deep: true,
            },
        )
        .await;

        assert_eq!(response.status, axum::http::StatusCode::OK);
        let response = response.response;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.provider_name, "OpenAI");
        assert_eq!(response.provider_kind, ProviderKind::Openai);
        assert_eq!(response.model, "gpt-5.5");
        assert_eq!(response.base_url, base_url);
        assert_eq!(response.output_preview, "ok");
        assert_eq!(response.usage.expect("usage").total_tokens, 6);
        assert_eq!(response.error, None);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["path"], "/responses");
        assert_eq!(requests[0]["authorization"], "Bearer secret");
        assert_eq!(requests[0]["body"]["model"], "gpt-5.5");
        assert_eq!(requests[0]["body"]["store"], true);
        assert_eq!(
            requests[0]["body"].pointer("/reasoning/effort"),
            Some(&json!("minimal"))
        );
        assert_eq!(requests[1]["body"]["previous_response_id"], "resp_test_1");
        assert_eq!(
            requests[1]["body"].pointer("/reasoning/effort"),
            Some(&json!("minimal"))
        );
    }

    #[tokio::test]
    async fn provider_test_deep_mode_covers_continuation_fallback() {
        let (base_url, requests) = start_provider_mock(vec![
            json!({
                "id": "resp_test_1",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 3, "output_tokens": 2, "total_tokens": 5 }
            }),
            json!({
                "__status": 400,
                "error": {
                    "message": "previous_response_id is only supported on Responses WebSocket v2",
                    "type": "invalid_request_error"
                }
            }),
            json!({
                "id": "resp_test_2",
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "ok" }]
                    }
                ],
                "usage": { "input_tokens": 6, "output_tokens": 2, "total_tokens": 8 }
            }),
        ])
        .await;
        let (_dir, store) = provider_test_store(provider_config(&base_url, Some("secret"))).await;

        let response = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(response.status, axum::http::StatusCode::OK);
        let response = response.response;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.output_preview, "ok");
        assert_eq!(response.usage.expect("usage").total_tokens, 8);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0]["body"].get("previous_response_id").is_none());
        assert_eq!(requests[1]["body"]["previous_response_id"], "resp_test_1");
        assert!(requests[2]["body"].get("previous_response_id").is_none());
        assert_eq!(requests[2]["body"]["store"], false);
        assert_eq!(
            requests[2]["body"]["input"]
                .as_array()
                .expect("input")
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn provider_test_reports_missing_provider() {
        let (_dir, store) =
            provider_test_store(provider_config("http://127.0.0.1:1", Some("secret"))).await;

        let response = run_provider_test(&store, "missing", ProviderTestRequest::default()).await;

        assert_eq!(response.status, axum::http::StatusCode::BAD_REQUEST);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "missing");
        assert!(
            response
                .error
                .unwrap()
                .contains("provider `missing` not found")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_missing_api_key_with_provider_context() {
        let (_dir, store) = provider_test_store(provider_config("http://127.0.0.1:1", None)).await;

        let response = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(response.status, axum::http::StatusCode::BAD_REQUEST);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.provider_name, "OpenAI");
        assert_eq!(response.model, "gpt-5.5");
        assert_eq!(response.base_url, "http://127.0.0.1:1");
        assert!(
            response
                .error
                .unwrap()
                .contains("provider `openai` has no API key")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_unknown_model_with_provider_context() {
        let (_dir, store) =
            provider_test_store(provider_config("http://127.0.0.1:1", Some("secret"))).await;

        let response = run_provider_test(
            &store,
            "openai",
            ProviderTestRequest {
                model: Some("missing-model".to_string()),
                reasoning_effort: None,
                deep: true,
            },
        )
        .await;

        assert_eq!(response.status, axum::http::StatusCode::BAD_REQUEST);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.provider_id, "openai");
        assert_eq!(response.model, "missing-model");
        assert!(
            response
                .error
                .unwrap()
                .contains("model `missing-model` is not configured for provider `openai`")
        );
    }

    #[tokio::test]
    async fn provider_test_reports_upstream_error_without_leaking_key() {
        let (base_url, _requests) = start_provider_mock(vec![json!({
            "__status": 401,
            "error": {
                "message": "bad token secret-token",
                "type": "invalid_request_error"
            }
        })])
        .await;
        let (_dir, store) =
            provider_test_store(provider_config(&base_url, Some("secret-token"))).await;

        let response = run_provider_test(&store, "openai", ProviderTestRequest::default()).await;

        assert_eq!(response.status, axum::http::StatusCode::OK);
        let response = response.response;
        assert!(!response.ok);
        assert_eq!(response.base_url, base_url);
        let error = response.error.expect("error");
        assert!(error.contains("returned 401 Unauthorized"));
        assert!(error.contains("[redacted]"));
        assert!(
            !error.contains("secret-token"),
            "provider test leaked api key: {error}"
        );
    }

    async fn start_provider_mock(responses: Vec<Value>) -> (String, Arc<TokioMutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock addr");
        let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
        let requests = Arc::new(TokioMutex::new(Vec::new()));
        let server_responses = Arc::clone(&responses);
        let server_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let responses = Arc::clone(&server_responses);
                let requests = Arc::clone(&server_requests);
                tokio::spawn(async move {
                    let request = read_provider_mock_request(&mut stream).await;
                    requests.lock().await.push(request);
                    let response = responses.lock().await.pop_front().unwrap_or_else(|| {
                        json!({
                            "id": "resp_empty",
                            "output": [],
                            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                        })
                    });
                    write_provider_mock_response(&mut stream, response).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    async fn read_provider_mock_request(stream: &mut TcpStream) -> Value {
        let mut buffer = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = stream.read(&mut chunk).await.expect("read request");
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&buffer);
                let header_end = text.find("\r\n\r\n").expect("header end");
                let headers = &text[..header_end];
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if buffer.len() >= body_start + content_length {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&buffer);
        let header_end = text.find("\r\n\r\n").expect("header end");
        let headers = &text[..header_end];
        let body = &buffer[header_end + 4..];
        let path = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default();
        let authorization = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim().to_string())
            })
            .unwrap_or_default();
        json!({
            "path": path,
            "authorization": authorization,
            "body": serde_json::from_slice::<Value>(body).unwrap_or(Value::Null),
        })
    }

    async fn write_provider_mock_response(stream: &mut TcpStream, mut response: Value) {
        let status = response
            .as_object_mut()
            .and_then(|object| object.remove("__status"))
            .and_then(|value| value.as_u64())
            .unwrap_or(200);
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let body = if status == 200 {
            provider_mock_sse_body(&response)
        } else {
            serde_json::to_string(&response).expect("response json")
        };
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        let raw = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(raw.as_bytes())
            .await
            .expect("write response");
    }

    fn provider_mock_sse_body(response: &Value) -> String {
        let response_id = response
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resp_mock");
        let mut events = vec![json!({
            "type": "response.created",
            "response": { "id": response_id }
        })];
        for (index, item) in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            }));
        }
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": response.get("usage").cloned().unwrap_or(Value::Null),
            }
        }));
        events
            .into_iter()
            .map(|event| {
                let kind = event
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                format!("event: {kind}\ndata: {event}\n\n")
            })
            .collect()
    }
}
