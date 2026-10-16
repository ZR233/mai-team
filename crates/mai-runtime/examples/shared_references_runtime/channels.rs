//! `channels` 场景：真实文件通道、命令输出上限与后台进程 stdin。
//!
//! 覆盖三条与工具无关、但同样落到**真实租约 + 真实 Docker**的通道：
//! 1. `Runtime::upload_file` / `download_file_tar`（管理面上传/下载，走 `AgentFileOps` +
//!    `MaiContainerBackend::authorize_container_write` + Docker cp）；
//! 2. 真实注册的 `exec` 工具：无匹配退出码、超时、32 KiB 模型可见截断、64 MiB 捕获硬上限；
//! 3. 后台 `exec` → 真实 task receipt → `wait/get_tool_task/write_stdin`，以及换代/重启后旧租约必须拒绝。
//!
//! 判定证据只用三类真实事实：provider 回填的 `function_call_output`（模型实际看到的工具输出）、
//! full payload 中的 exact 捕获/输出路径与真实文件体积、以及 Docker/`/proc` 事实
//! （容器是否仍在运行、进程是否残留）。工具 trace 在本树尚未接线
//! （`save_tool_trace_*` 没有调用者），因此 trace 只作为可选交叉核对，缺失时记 boundary 而不是
//! 伪造通过。
//!
//! `stat_path` / `read_bytes` 不需要新增模型工具：它们是 `pl_tool::workspace_file` 的
//! `ToolWorkspaceFileKind` 内部入口（本 Runtime 只注册 `read_file`/`list_files`/`apply_patch`），
//! 由 `MaiWorkspaceFileBackend`（`crates/mai-runtime/src/turn/workspace_file.rs`）的
//! `stat`/`read_bytes` 实现调用，读写授权仍走同一 `authorize_container_write`/读根判定。

#[path = "channels/background.rs"]
pub mod background;
#[path = "channels/exec_limits.rs"]
pub mod exec_limits;
#[path = "channels/outputs.rs"]
pub mod outputs;
#[path = "channels/swap.rs"]
pub mod swap;
#[path = "channels/task.rs"]
pub mod task;
#[path = "channels/transfer.rs"]
pub mod transfer;

use mai_protocol::AgentResourceState;
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::{self, RuntimeHost};
use crate::probe;
use crate::runtime_config::DriverConfig;

/// `channels` 命令入口；由 entry 的 CLI 调用。
pub async fn run(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    let store = host::open_store(config).await?;
    let runtime = host::start(config, store).await?;
    evidence.fact("provider", runtime.configure_models().await?);
    let outcome = scenario(config, &runtime, evidence).await;
    // background 用例会在内部重启 Runtime；这里的 shutdown 只是兜底（重复调用只记录错误）。
    runtime.shutdown().await;
    outcome
}

async fn scenario(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
) -> Result<()> {
    let tag = probe::run_tag();
    let maintainer = fixtures::agent_id(fixtures::AGENT_MAINTAINER);
    let project_a = fixtures::project_configuration(&host.store, ProjectKind::A).await?;
    evidence.fact("project_a_configuration", json!(&project_a));
    if let Err(cause) = fixtures::ensure_project_a_contract(&project_a) {
        evidence.blocked(
            "channels.project_a_contract",
            format!(
                "project A 夹具契约不满足，通道用例无法判定：{}",
                cause.message()
            ),
        );
        return Ok(());
    }
    evidence.pass(
        "channels.project_a_contract",
        "project A 绑定 linux + private-fixture 且都启用",
    );

    let summary = host.agent(maintainer).await?;
    if summary.resource.state != AgentResourceState::Ready {
        evidence.blocked(
            "channels.parent_ready",
            format!(
                "{} 的资源状态是 {:?}，upload/download 与 exec 都会被拒绝",
                summary.name, summary.resource.state
            ),
        );
        return Ok(());
    }
    let private_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
    evidence.fact(
        "parent",
        json!({
            "agent_id": maintainer,
            "container_id": summary.container_id,
            "private_target": private_target,
        }),
    );

    if matches!(
        config.command,
        crate::runtime_config::Command::ChannelsLimits
    ) {
        return exec_limits::case(config, host, evidence, &tag, maintainer, &private_target).await;
    }
    transfer::case(config, host, evidence, &tag, maintainer, &private_target).await?;
    exec_limits::case(config, host, evidence, &tag, maintainer, &private_target).await?;
    // 本用例会关闭并重建 Runtime 实例，因此放在最后。
    background::case(config, host, evidence, &tag, maintainer, &private_target).await?;

    evidence.boundary(
        "channels.stat_path_read_bytes",
        "stat_path/read_bytes 不是模型可见工具（WorkspaceFileToolKind 只注册 read_file/list_files/apply_patch）：它们是 MaiWorkspaceFileBackend 的内部 WorkspaceFileBackend 入口，由 read_file 的 stat+read_text、list_files 的 list、apply_patch 的 write 间接调用；本用例不为验收新增产品公开 API",
    );
    Ok(())
}
