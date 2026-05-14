use std::env;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use mai_docker::DockerClient;
use mai_model::ModelClient;
use mai_runtime::RuntimeConfig;
use tracing::info;

use crate::config::cli::Cli;
use crate::handlers;
use crate::handlers::state::AppState;
use crate::http::router;

pub(crate) async fn run(cli: Cli) -> Result<()> {
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
