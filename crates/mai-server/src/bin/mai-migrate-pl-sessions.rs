//! 停服并完成全量备份后，使用 pl-core 的迁移入口升级 Mai 管理的会话目录。

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use pl_core::persistence::{SqliteSessionOptions, migration};

#[derive(Parser)]
#[command(about = "离线升级 Mai 会话目录中的 pl-core SQLite 数据库")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    data_path: PathBuf,
    #[arg(long, value_name = "TAR")]
    backup_archive: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let backup = tokio::fs::metadata(&cli.backup_archive)
        .await
        .with_context(|| format!("找不到会话迁移前备份：{}", cli.backup_archive.display()))?;
    ensure!(backup.is_file() && backup.len() > 0, "会话迁移前备份为空");

    let sessions_root = cli.data_path.join("sessions");
    let mut entries = match tokio::fs::read_dir(&sessions_root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("没有现存的 pl-core 会话数据库");
            return Ok(());
        }
        Err(error) => return Err(error).context("读取会话目录失败"),
    };
    let mut databases = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let path = entry.path().join("history.sqlite");
        if tokio::fs::try_exists(&path).await? {
            databases.push(path);
        }
    }
    databases.sort();

    for (index, path) in databases.iter().enumerate() {
        println!(
            "升级会话 {}/{}：{}",
            index + 1,
            databases.len(),
            path.display()
        );
        migration::migrate_to_current(SqliteSessionOptions { path: path.clone() }, |_| Ok(()))
            .await
            .with_context(|| format!("迁移 pl-core 会话失败：{}", path.display()))?;
    }
    println!("已升级 {} 个 pl-core 会话数据库", databases.len());
    Ok(())
}
