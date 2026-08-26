use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "离线归档 PL v1 并迁移 mai-team 数据库到 schema 32")]
struct Args {
    /// 待迁移的 SQLite 数据库；执行写入前必须停止 mai-server。
    #[arg(long)]
    database: PathBuf,
    /// 不可自动清理的框架归档根目录；默认位于数据库同级 framework-archives。
    #[arg(long)]
    archive_root: Option<PathBuf>,
    /// 当前部署二进制对应的 mai-team 提交。
    #[arg(long)]
    source_commit: Option<String>,
    /// 即将部署的 mai-team 提交。
    #[arg(long)]
    target_commit: Option<String>,
    /// 只校验 schema 和迁移前后不变量，不执行归档或写入。
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = if args.check {
        mai_migrate::validate_path(&args.database)?
    } else {
        let parent = args.database.parent().context("数据库路径缺少父目录")?;
        let options = mai_migrate::MigrationOptions {
            archive_root: args
                .archive_root
                .unwrap_or_else(|| parent.join("framework-archives")),
            source_commit: args
                .source_commit
                .context("执行迁移必须提供 --source-commit")?,
            target_commit: args
                .target_commit
                .context("执行迁移必须提供 --target-commit")?,
        };
        mai_migrate::migrate_path(&args.database, &options)?
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
