use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "离线检查与迁移 mai-team 产品库；不会访问 PL 会话")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Check {
        #[arg(long)]
        database: PathBuf,
    },
    Migrate {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        backup: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let report = match Cli::parse().command {
        Command::Check { database } => mai_store::check_product_database(&database)?,
        Command::Migrate { database, backup } => {
            mai_store::migrate_product_database(&database, &backup)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
