use std::env;
use std::fs;
use std::sync::Arc;

use anyhow::Result;
use mai_docker::DockerClient;
use mai_model::ModelClient;
use mai_runtime::RuntimeConfig;
use tracing::info;

use crate::config::{Cli, RelayMode, ServerConfig, ServerPaths, StdEnv};
use crate::handlers::state::AppState;
use crate::http::router;

pub(crate) async fn run(cli: Cli) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mai_server=info,mai_runtime=info,tower_http=info".into()),
        )
        .init();

    let config = ServerConfig::from_sources(cli, &StdEnv)?;
    let paths = ServerPaths::from_data_path(&env::current_dir()?, config.data_path.clone());
    let addr = config.bind_addr;

    let docker = DockerClient::new(config.images.agent_base_image.clone());
    let docker_version = docker.check_available().await?;
    info!("docker available: {docker_version}");

    fs::create_dir_all(&paths.cache_dir)?;
    fs::create_dir_all(&paths.projects_root)?;
    fs::create_dir_all(&paths.artifact_files_root)?;
    fs::create_dir_all(&paths.artifact_index_root)?;

    let store = Arc::new(mai_store::ConfigStore::open_in_data_dir(&paths.data_dir).await?);
    store
        .seed_default_provider_from_env(
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

    let model_client = ModelClient::new();
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
    let relay = match config.relay {
        RelayMode::Disabled => None,
        RelayMode::Enabled(config) => Some(Arc::new(mai_relay_client::RelayClient::new(config))),
    };
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
    ensure_startup_chat_environment(&runtime).await?;
    let state = Arc::new(AppState {
        runtime,
        store,
        relay,
    });

    let app = router::create_router(state);

    println!("Open http://{addr}/");
    info!("mai-team listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ensure_startup_chat_environment(
    runtime: &Arc<mai_runtime::AgentRuntime>,
) -> mai_runtime::Result<()> {
    let _ = runtime.ensure_default_environment().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_docker::DockerClient;
    use mai_model::ModelClient;
    use mai_protocol::{ModelConfig, ProviderConfig, ProviderKind, ProvidersConfigRequest};
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[tokio::test]
    async fn startup_ensures_default_chat_environment_with_default_image() {
        let dir = tempdir().expect("tempdir");
        let store = Arc::new(
            mai_store::ConfigStore::open_with_config_and_artifact_index_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
                dir.path().join("artifacts/index"),
            )
            .await
            .expect("store"),
        );
        store
            .save_providers(ProvidersConfigRequest {
                providers: vec![test_provider()],
                default_provider_id: Some("openai".to_string()),
            })
            .await
            .expect("save providers");
        let runtime = mai_runtime::AgentRuntime::new(
            DockerClient::new_with_binary("ubuntu:latest", fake_docker_path(&dir)),
            ModelClient::new(),
            Arc::clone(&store),
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
        .expect("runtime");

        assert!(runtime.list_environments().await.is_empty());

        ensure_startup_chat_environment(&runtime)
            .await
            .expect("ensure chat environment");

        let environments = runtime.list_environments().await;
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].name, "默认环境");
        assert_eq!(environments[0].docker_image, "ubuntu:latest");
        let detail = runtime
            .get_environment(environments[0].id, None)
            .await
            .expect("environment detail");
        assert_eq!(detail.root_agent.sessions[0].title, "Chat 1");

        ensure_startup_chat_environment(&runtime)
            .await
            .expect("ensure chat environment again");
        assert_eq!(runtime.list_environments().await.len(), 1);
    }

    fn test_provider() -> ProviderConfig {
        ProviderConfig {
            id: "openai".to_string(),
            kind: ProviderKind::Openai,
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: Some("secret".to_string()),
            api_key_env: None,
            models: vec![ModelConfig {
                id: "gpt-test".to_string(),
                name: Some("gpt-test".to_string()),
                context_tokens: 128_000,
                output_tokens: 16_000,
                supports_tools: true,
                reasoning: None,
                options: serde_json::Value::Null,
                headers: Default::default(),
                wire_api: Default::default(),
                capabilities: Default::default(),
                request_policy: Default::default(),
            }],
            default_model: "gpt-test".to_string(),
            enabled: true,
        }
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
