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
    crate::handlers::assets::release_embedded_system_skills(&system_skills_root)?;
    info!(
        path = %system_skills_root.display(),
        "released embedded system skills"
    );
    let system_agents_root = paths.system_agents_root.clone();
    crate::handlers::assets::release_embedded_system_agents(&system_agents_root)?;
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
