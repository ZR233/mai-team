use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "离线迁移 mai-team 数据库到当前 schema")]
struct Args {
    /// 待迁移的 SQLite 数据库。运行前必须停止 mai-server 并完成备份。
    #[arg(long)]
    database: PathBuf,
    /// 只校验 schema 和迁移后不变量，不执行写入。
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = if args.check {
        mai_migrate::validate_path(&args.database)?
    } else {
        mai_migrate::migrate_path(&args.database)?
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
