//! 隔离参考源的**真实 Runtime** 隔离验收驱动。
//!
//! 与 `mai-references` 领域驱动（`shared_references_probe`）的分工：
//! - 领域驱动只驱动 `ReferenceApplications` + Docker 状态机，不启动模型 turn；
//! - 本驱动启动真实 `AgentRuntime`，装配真实 PL 工具与真实容器，用**真实工具执行**
//!   （read_file/list_files/apply_patch/exec/spawn_agent…）和真实 turn/lease 行为
//!   判定 W8 的 Runtime 侧验收点。判定证据来自：
//!   1. canonical Thread 的真实工具调用（`AgentRuntime::thread_turns`）；
//!   2. 真实 turn 状态与物理容器事实（`get_agent` / Docker inspect）；
//!   3. fixture provider 记录的 provider 请求（只读，用于核对模型收到的上下文与工具回填）。
//!
//! 模型侧使用确定性 fixture provider（`127.0.0.1:18081/v1`，见
//! `scripts/references/fixture-provider.py`）：它只在每个 `probe_id` 的**第一次**请求里发一批
//! 工具调用，随后正常文本收尾。因此一个 probe 只能表达**一轮**工具调用；需要“先发现
//! 工具再调用”的两段式场景要 provider 支持多轮，属于交接给 root 的 fixture 增强点。
//!
//! 用法（root 在验收机上执行）：
//!
//! ```text
//! DOCKER_HOST=unix:///run/mai-reference-acceptance-docker/docker.sock \
//!   cargo run -p mai-runtime --example shared_references_runtime -- \
//!   [--root <path>] [--gate-mode idle|stop|full] [--phase spawn|resume|full] \
//!   [--digest fixture|all] [--turn-timeout-secs <n>] [--operation-timeout-secs <n>] \
//!   <config|tools|gate|lifecycle|channels|projects|injection>
//! ```

#[path = "shared_references_runtime/applied.rs"]
mod applied;
#[path = "shared_references_runtime/channels.rs"]
mod channels;
#[path = "shared_references_runtime/collaboration.rs"]
mod collaboration;
#[path = "shared_references_runtime/error.rs"]
mod error;
#[path = "shared_references_runtime/evidence.rs"]
mod evidence;
#[path = "shared_references_runtime/fixture_setup.rs"]
mod fixture_setup;
#[path = "shared_references_runtime/fixtures.rs"]
mod fixtures;
#[path = "shared_references_runtime/gate.rs"]
mod gate;
#[path = "shared_references_runtime/guard.rs"]
mod guard;
#[path = "shared_references_runtime/host.rs"]
mod host;
#[path = "shared_references_runtime/injection.rs"]
mod injection;
#[path = "shared_references_runtime/lifecycle.rs"]
mod lifecycle;
#[path = "shared_references_runtime/probe.rs"]
mod probe;
#[path = "shared_references_runtime/project_journeys.rs"]
mod project_journeys;
#[path = "shared_references_runtime/runtime_config.rs"]
mod runtime_config;
#[path = "shared_references_runtime/tool_inventory.rs"]
mod tool_inventory;
#[path = "shared_references_runtime/tools.rs"]
mod tools;

use std::process::ExitCode;

use runtime_config::{Command, DriverConfig};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match DriverConfig::from_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            let payload = serde_json::json!({
                "driver": "shared-references-runtime",
                "stage": "parse",
                "error": { "code": error.code(), "message": error.message() },
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
            );
            eprintln!("{}", error);
            eprintln!("{}", runtime_config::USAGE);
            return ExitCode::from(2);
        }
    };

    if config.help {
        println!("{}", runtime_config::USAGE);
        return ExitCode::SUCCESS;
    }

    // 隔离边界先判：不是固定验收根 / 不是独立验收 daemon 一律不继续。
    if let Err(error) = guard::ensure_boundaries(&config) {
        return report_failure(&config, error);
    }

    let mut evidence = match evidence::Evidence::new(&config) {
        Ok(evidence) => evidence,
        Err(error) => return report_failure(&config, error),
    };

    let outcome = match config.command {
        Command::Config => host::configure_only(&config, &mut evidence).await,
        Command::Tools => tools::run(&config, &mut evidence).await,
        Command::Channels | Command::ChannelsLimits => channels::run(&config, &mut evidence).await,
        Command::Gate => gate::run(&config, &mut evidence).await,
        Command::Lifecycle => lifecycle::run(&config, &mut evidence).await,
        Command::Projects => project_journeys::run(&config, &mut evidence).await,
        Command::Injection => injection::run(&config, &mut evidence).await,
    };

    if let Err(error) = outcome {
        evidence.fail("driver", format!("{}: {}", error.code(), error.message()));
    }
    evidence.finish_and_write()
}

fn report_failure(config: &DriverConfig, error: error::DriverError) -> ExitCode {
    let payload = serde_json::json!({
        "driver": "shared-references-runtime",
        "command": config.command.as_str(),
        "root": config.root.display().to_string(),
        "error": { "code": error.code(), "message": error.message() },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
    );
    eprintln!("{}: {}", config.command.as_str(), error);
    ExitCode::from(1)
}
