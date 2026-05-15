mod callback_page;
mod delivery;
mod error;
mod github;
mod routes;
mod rpc;
mod session;
mod state;
mod store;
mod update;

use anyhow::{Context, Result};
use github::{DEFAULT_GITHUB_API_BASE_URL, DEFAULT_GITHUB_WEB_BASE_URL};
use state::{AppState, RelayConfig};
use std::sync::Arc;
use tokio::time::Duration;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mai_relay=info,tower_http=info".into()),
        )
        .init();

    let config = RelayConfig::from_env(DEFAULT_GITHUB_API_BASE_URL, DEFAULT_GITHUB_WEB_BASE_URL)?;
    let store = Arc::new(store::RelayStore::open(config.db_path.clone())?);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    github::app::bootstrap_github_app_config(
        &store,
        &http,
        &config.public_url,
        &config.github_api_base_url,
        &config.github_web_base_url,
    )
    .await?;

    let addr = config.bind_addr;
    let state = Arc::new(AppState::new(config, store, http)?);
    let app = routes::router(state);

    info!("mai-relay listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("binding relay listener")?;
    axum::serve(listener, app).await?;
    Ok(())
}
