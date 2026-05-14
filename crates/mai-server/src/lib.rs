pub(crate) mod bootstrap;
pub(crate) mod config;
pub(crate) mod handlers;
pub(crate) mod http;

use anyhow::Result;
use clap::Parser;

pub async fn run() -> Result<()> {
    let cli = config::cli::Cli::parse();
    bootstrap::run(cli).await
}
