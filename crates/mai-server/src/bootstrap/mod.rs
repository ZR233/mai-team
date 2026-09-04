use std::env;
use std::fs;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use mai_docker::DockerClient;
use mai_runtime::{RuntimeConfig, RuntimeError};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::{Cli, RelayMode, ServerConfig, ServerPaths, StdEnv};
use crate::handlers::state::AppState;
use crate::http::router;
use crate::services::relay_manager::{DynamicGithubAppBackend, RelayManager};

const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Eq, PartialEq)]
enum ServerShutdownCause {
    Signal,
    FatalRuntime(String),
}

pub(crate) async fn run(cli: Cli) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mai_server=debug,mai_runtime=info,tower_http=info".into()),
        )
        .init();

    let config = ServerConfig::from_sources(cli, &StdEnv)?;
    let paths = ServerPaths::from_data_path(&env::current_dir()?, config.data_path.clone());
    let addr = config.bind_addr;
    let listener = bind_server_listener(addr).await?;

    let docker = DockerClient::new(config.images.agent_base_image.clone());
    let docker_version = docker.check_available().await?;
    info!("docker available: {docker_version}");

    fs::create_dir_all(&paths.cache_dir)?;
    fs::create_dir_all(&paths.projects_root)?;
    fs::create_dir_all(&paths.artifact_files_root)?;
    fs::create_dir_all(&paths.artifact_index_root)?;

    let store = Arc::new(mai_store::MaiStore::open_in_data_dir(&paths.data_dir).await?);
    mai_runtime::seed_default_provider_from_env(
        store.as_ref(),
        config.provider_seed.api_key,
        config.provider_seed.base_url,
        config.provider_seed.model,
    )
    .await?;

    let system_skills_root = paths.system_skills_root.clone();
    crate::infrastructure::system_resources::release_embedded_system_skills(&system_skills_root)?;
    info!(
        path = %system_skills_root.display(),
        "released embedded system skills"
    );
    let system_agents_root = paths.system_agents_root.clone();
    crate::infrastructure::system_resources::release_embedded_system_agents(&system_agents_root)?;
    info!(
        path = %system_agents_root.display(),
        "released embedded system agents"
    );

    let runtime_config = RuntimeConfig {
        repo_root: env::current_dir()?,
        projects_root: paths.projects_root.clone(),
        cache_root: paths.cache_dir.clone(),
        artifact_files_root: paths.artifact_files_root.clone(),
        sidecar_image: config.images.sidecar_image,
        github_api_base_url: None,
        git_binary: None,
        system_skills_root: Some(system_skills_root),
        system_agents_root: Some(system_agents_root),
    };
    info!(
        data_dir = %paths.data_dir.display(),
        cache_dir = %paths.cache_dir.display(),
        projects_root = %paths.projects_root.display(),
        artifact_files_root = %paths.artifact_files_root.display(),
        artifact_index_root = %paths.artifact_index_root.display(),
        "runtime storage paths"
    );
    seed_relay_settings_from_env(&store, config.relay).await?;
    let relay = RelayManager::new(Arc::clone(&store));
    let github_http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            mai_runtime::github::GITHUB_HTTP_TIMEOUT_SECS,
        ))
        .build()?;
    let direct_backend = Arc::new(mai_runtime::github::DirectGithubAppBackend::new(
        Arc::clone(&store),
        github_http,
        mai_runtime::github::DEFAULT_GITHUB_API_BASE_URL.to_string(),
    )) as Arc<dyn mai_runtime::github::GithubAppBackend>;
    let github_backend = Some(Arc::new(DynamicGithubAppBackend::new(
        direct_backend,
        Arc::clone(&relay),
    )) as Arc<dyn mai_runtime::github::GithubAppBackend>);
    relay.configure_from_store().await?;
    let runtime = mai_runtime::AgentRuntime::new_with_github_backend(
        docker,
        Arc::clone(&store),
        runtime_config,
        github_backend,
    )
    .await?;
    relay.set_runtime(Arc::clone(&runtime)).await;
    let cleaned = runtime.cleanup_orphaned_containers().await?;
    if !cleaned.is_empty() {
        info!(
            count = cleaned.len(),
            "removed orphaned mai-team containers"
        );
    }
    ensure_startup_chat_environment(&runtime).await?;
    let shutdown_runtime = Arc::clone(&runtime);
    let shutdown = CancellationToken::new();
    let state = Arc::new(AppState {
        runtime,
        store,
        relay: Arc::clone(&relay),
        shutdown: shutdown.clone(),
    });

    let app = router::create_router(state);

    println!("Open http://{addr}/");
    info!("mai-team listening on http://{addr}");
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .into_future();
    tokio::pin!(server);
    let shutdown_request =
        select_shutdown_cause(shutdown_signal(), shutdown_runtime.wait_for_fatal_error());
    tokio::pin!(shutdown_request);
    let cause = tokio::select! {
        cause = &mut shutdown_request => cause,
        result = &mut server => {
            shutdown.cancel();
            let cleanup_result = complete_shutdown_with_timeout(async {
                relay.shutdown().await;
                shutdown_runtime.shutdown().await?;
                Ok(())
            }, SERVER_SHUTDOWN_TIMEOUT)
            .await;
            cleanup_result?;
            result.context("HTTP server stopped unexpectedly")?;
            anyhow::bail!("HTTP server stopped without a shutdown request");
        }
    };
    match &cause {
        ServerShutdownCause::Signal => info!("server shutdown signal received"),
        ServerShutdownCause::FatalRuntime(failure) => {
            error!("fatal agent runtime failure detected; restarting server: {failure}")
        }
    }
    shutdown.cancel();
    let shutdown_result = complete_shutdown_with_timeout(
        async {
            (&mut server)
                .await
                .context("HTTP server failed while draining")?;
            info!("HTTP connections drained");
            relay.shutdown().await;
            shutdown_runtime.shutdown().await?;
            info!("runtime shutdown drained");
            Ok(())
        },
        SERVER_SHUTDOWN_TIMEOUT,
    )
    .await;
    finish_server_shutdown(cause, shutdown_result)
}

async fn select_shutdown_cause(
    signal: impl Future<Output = ()>,
    runtime_failure: impl Future<Output = RuntimeError>,
) -> ServerShutdownCause {
    tokio::select! {
        biased;
        failure = runtime_failure => ServerShutdownCause::FatalRuntime(failure.to_string()),
        _ = signal => ServerShutdownCause::Signal,
    }
}

async fn complete_shutdown_with_timeout(
    shutdown: impl Future<Output = Result<()>>,
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, shutdown)
        .await
        .with_context(|| {
            format!(
                "server shutdown timed out after {} seconds",
                timeout.as_secs()
            )
        })?
}

fn finish_server_shutdown(cause: ServerShutdownCause, shutdown_result: Result<()>) -> Result<()> {
    match cause {
        ServerShutdownCause::Signal => shutdown_result,
        ServerShutdownCause::FatalRuntime(failure) => {
            if let Err(shutdown_error) = shutdown_result {
                warn!("cleanup after fatal failure did not drain cleanly: {shutdown_error}");
            }
            anyhow::bail!("fatal agent runtime failure requires restart: {failure}");
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!("failed to listen for shutdown signal: {error}");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to listen for shutdown signal: {error}");
    }
}

async fn bind_server_listener(addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!("failed to bind MAI_BIND_ADDR {addr}; set MAI_BIND_ADDR to a free address or stop the process using it")
    })
}

async fn seed_relay_settings_from_env(
    store: &Arc<mai_store::MaiStore>,
    mode: RelayMode,
) -> mai_store::Result<()> {
    if store.relay_settings().await?.has_token {
        return Ok(());
    }
    let RelayMode::Enabled(config) = mode else {
        return Ok(());
    };
    store
        .save_relay_settings(mai_protocol::RelaySettingsRequest {
            enabled: true,
            url: Some(config.url),
            token: Some(config.token),
            node_id: Some(config.node_id),
        })
        .await?;
    Ok(())
}

async fn ensure_startup_chat_environment(
    runtime: &Arc<mai_runtime::AgentRuntime>,
) -> mai_runtime::Result<()> {
    runtime.ensure_default_environment().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_docker::DockerClient;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[tokio::test]
    async fn bind_server_listener_reports_occupied_addr() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied listener");
        let addr = occupied.local_addr().expect("occupied listener addr");

        let error = bind_server_listener(addr)
            .await
            .expect_err("occupied addr should fail");

        let message = format!("{error:#}");
        assert!(message.contains(&format!("failed to bind MAI_BIND_ADDR {addr}")));
        assert!(message.contains("set MAI_BIND_ADDR"));
    }

    #[tokio::test]
    async fn fatal_runtime_failure_requests_server_restart() {
        let cause = select_shutdown_cause(std::future::pending(), async {
            RuntimeError::InvalidInput("Thread writer is blocked".to_string())
        })
        .await;

        assert_eq!(
            ServerShutdownCause::FatalRuntime(
                "invalid input: Thread writer is blocked".to_string()
            ),
            cause
        );
        let error = finish_server_shutdown(cause, Ok(()))
            .expect_err("fatal runtime failure must return a process error");
        assert!(error.to_string().contains("requires restart"));
    }

    #[tokio::test]
    async fn server_shutdown_timeout_bounds_stuck_http_drain() {
        let result = complete_shutdown_with_timeout(
            std::future::pending::<Result<()>>(),
            Duration::from_millis(10),
        )
        .await
        .expect_err("stuck drain must be bounded");

        assert!(result.to_string().contains("server shutdown timed out"));
    }

    #[tokio::test]
    async fn startup_ensures_default_chat_environment_with_default_image() {
        let dir = tempdir().expect("tempdir");
        let database_path = dir.path().join("runtime.sqlite3");
        let config_path = dir.path().join("config.toml");
        let artifact_index_path = dir.path().join("artifacts/index");
        let store = Arc::new(
            mai_store::MaiStore::open_with_config_and_artifact_index_path(
                database_path.clone(),
                config_path.clone(),
                artifact_index_path.clone(),
            )
            .await
            .expect("store"),
        );
        let mai_config = mai_runtime::MaiConfig::default();
        store
            .config_documents()
            .save(&mai_config)
            .await
            .expect("save config");
        let runtime = test_runtime(&dir, Arc::clone(&store)).await;

        assert!(runtime.list_environments().await.is_empty());

        ensure_startup_chat_environment(&runtime)
            .await
            .expect("ensure chat environment");

        let environments = runtime.list_environments().await;
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].name, "默认环境");
        assert_eq!(environments[0].docker_image, "ubuntu:latest");
        let detail = runtime
            .get_environment(environments[0].id)
            .await
            .expect("environment detail");
        assert_eq!(detail.root_agent.thread.title, "默认环境");

        ensure_startup_chat_environment(&runtime)
            .await
            .expect("ensure chat environment again");
        assert_eq!(runtime.list_environments().await.len(), 1);

        let original_environment_id = environments[0].id;
        let original_root_agent_id = environments[0].root_agent_id;
        runtime.shutdown().await.expect("shutdown first runtime");
        drop(runtime);
        drop(store);

        let connection = rusqlite::Connection::open(&database_path).expect("open runtime database");
        connection
            .execute_batch(
                "DELETE FROM thread_submissions;
                 DELETE FROM thread_notifications;
                 DELETE FROM thread_runtime_traces;
                 DELETE FROM thread_runtime_events;
                 DELETE FROM thread_items;
                 DELETE FROM thread_turns;
                 DELETE FROM thread_runtime_documents;",
            )
            .expect("simulate schema32 framework reset");
        drop(connection);

        let store = Arc::new(
            mai_store::MaiStore::open_with_config_and_artifact_index_path(
                database_path,
                config_path,
                artifact_index_path,
            )
            .await
            .expect("reopen migrated store"),
        );
        let runtime = test_runtime(&dir, store).await;

        ensure_startup_chat_environment(&runtime)
            .await
            .expect("restore existing chat environment");
        let restored = runtime.list_environments().await;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, original_environment_id);
        assert_eq!(restored[0].root_agent_id, original_root_agent_id);
        runtime.shutdown().await.expect("shutdown restored runtime");
    }

    async fn test_runtime(
        dir: &tempfile::TempDir,
        store: Arc<mai_store::MaiStore>,
    ) -> Arc<mai_runtime::AgentRuntime> {
        mai_runtime::AgentRuntime::new(
            DockerClient::new_with_binary("ubuntu:latest", fake_docker_path(dir)),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
                projects_root: dir.path().join("projects"),
                cache_root: dir.path().join("cache"),
                artifact_files_root: dir.path().join("artifacts/files"),
                sidecar_image: "sidecar:latest".to_string(),
                github_api_base_url: None,
                git_binary: None,
                system_skills_root: None,
                system_agents_root: None,
            },
        )
        .await
        .expect("test runtime")
    }

    fn fake_docker_path(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("fake-docker.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
case "$1" in
  ps)
    exit 0
    ;;
  create)
    echo "created-container"
    exit 0
    ;;
  start)
    exit 0
    ;;
  exec)
    exit 0
    ;;
  version)
    echo "test-version"
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
"#,
        )
        .expect("write fake docker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path)
                .expect("fake docker metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod fake docker");
        }
        path.to_string_lossy().into_owned()
    }
}
