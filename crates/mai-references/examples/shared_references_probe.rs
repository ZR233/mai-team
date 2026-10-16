//! 项目共享只读参考源码的隔离验收驱动（只编写，不在此驱动内自测）。
//!
//! 这个可执行文件在**独立 Docker daemon + 独立验收库 + 独立验收根**上驱动真实的
//! `MaiStore` + `ReferenceCatalog` + `ReferenceApplications` + `DockerClient` 状态机，
//! 用 JSON 证据记录 T14–T17、T21–T23、T26–T27、T30–T33 与 T39 中属于领域/Docker
//! 的部分。它在产品运行时不安装、不链接，只作为验收工具。
//!
//! 明确边界（报告里也逐条写出）：
//! - `ReferenceEnvironmentHost::activate` 不启动模型 turn/PL Thread：真实 Runtime 的
//!   工具读取（read_file/list_files）、MCP 工具通道、Review Thread 与 Web UI 由主代理
//!   在真实 Runtime 上另测，本驱动不宣称覆盖。
//! - 崩溃注入只按用户指定的 phase 在“收据已 fsync”之后硬退出一次，恢复由 root 反复
//!   启动驱动完成；驱动自己不做重试循环，也不把 failed 当成成功。
//!
//! 用法（root 在验收机上执行，参数见 `ProbeConfig`）：
//!
//! ```text
//! DOCKER_HOST=unix:///run/mai-reference-acceptance-docker/docker.sock \
//!   cargo run -p mai-references --example shared_references_probe -- \
//!   [--config <probe-config.json>] <seed|apply|status|recover|clear|delete>
//!   [--scenario bind|metadata_only|clear|reuse|concurrent]
//!   [--inject-phase draining|snapshotting|creating|verifying|rebinding|publishing|cleanup_pending]
//! ```

#[path = "shared_references_probe/checks/mod.rs"]
mod checks;
#[path = "shared_references_probe/commands/mod.rs"]
mod commands;
#[path = "shared_references_probe/config/mod.rs"]
mod config;
#[path = "shared_references_probe/environment/mod.rs"]
mod environment;
#[path = "shared_references_probe/error.rs"]
mod error;
#[path = "shared_references_probe/evidence.rs"]
mod evidence;
#[path = "shared_references_probe/fixtures.rs"]
mod fixtures;
#[path = "shared_references_probe/guard.rs"]
mod guard;
#[path = "shared_references_probe/host.rs"]
mod host;
#[path = "shared_references_probe/inject.rs"]
mod inject;
#[path = "shared_references_probe/report.rs"]
mod report;

use std::process::ExitCode;

use config::ProbeConfig;

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = match ProbeConfig::from_args(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(error) => {
            let payload = serde_json::json!({
                "probe": "mai-references-acceptance",
                "command": "parse",
                "error": { "code": error.code(), "message": error.message() },
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
            );
            eprintln!("{}", error);
            eprintln!("{}", config::USAGE);
            return ExitCode::from(1);
        }
    };

    if arguments.help {
        println!("{}", config::USAGE);
        return ExitCode::SUCCESS;
    }

    match commands::run(arguments).await {
        Ok(report) => ExitCode::from(report.finish()),
        Err(failure) => {
            let payload = serde_json::json!({
                "probe": "mai-references-acceptance",
                "command": failure.command,
                "error": { "code": failure.error.code(), "message": failure.error.message() },
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
            );
            eprintln!("{}: {}", failure.command, failure.error);
            ExitCode::from(1)
        }
    }
}
