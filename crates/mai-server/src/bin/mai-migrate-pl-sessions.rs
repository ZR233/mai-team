//! 显式检查、备份并迁移会话；普通服务打开旧库时不会执行这些动作。

use anyhow::Result;
use clap::{Parser, Subcommand};
use mai_server::maintenance::{check_pl_sessions, migrate_pl_sessions};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "停服后检查、备份并迁移 Mai 的 PL 会话；不改产品库")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Check {
        #[arg(long)]
        data_path: PathBuf,
    },
    Migrate {
        #[arg(long)]
        data_path: PathBuf,
        #[arg(long)]
        backup_directory: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let report = match Cli::parse().command {
        Command::Check { data_path } => check_pl_sessions(&data_path)?,
        Command::Migrate {
            data_path,
            backup_directory,
        } => migrate_pl_sessions(&data_path, &backup_directory).await?,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
