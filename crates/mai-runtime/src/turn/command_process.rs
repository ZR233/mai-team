//! 命令的真实进程归属 generation；关闭调用者也不能丢失清理与等待责任。

use std::sync::Arc;
use std::time::Duration;

use mai_docker::DockerClient;
use mai_protocol::ReferenceErrorCode;
use mai_references::{AgentEnvironmentLease, ReferenceError};
use pl_tool::command::{
    CommandExit, CommandIo, CommandReader, CommandSpawnRequest, CommandWriter, ManagedCommand,
};
use tokio::sync::oneshot;

use crate::{Result, RuntimeError};

pub(super) async fn spawn_generation_command(
    docker: DockerClient,
    lease: Arc<AgentEnvironmentLease>,
    request: CommandSpawnRequest,
) -> Result<ManagedCommand> {
    let (ready, receive_ready) = oneshot::channel();
    let (send_cancellation, receive_cancellation) =
        oneshot::channel::<pl_tool::command::CommandCancellation>();
    let task_lease = Arc::clone(&lease);
    // 登记 fence 在创建宿主进程之前落下，换代不可能漏掉“登记之前已启动”的命令。
    let mut task = lease.spawn_tracked(async move {
        let container_id = task_lease.container_id().to_owned();
        let spawned = task_lease
            .ensure_active()
            .map_err(RuntimeError::from)
            .and_then(|()| {
                docker
                    .spawn_managed_exec(
                        &container_id,
                        &request.process_id,
                        &request.command,
                        Some(&request.cwd),
                    )
                    .map_err(RuntimeError::from)
            });
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                let message = error.to_string();
                let _ = ready.send(Err(error));
                return Err(message);
            }
        };
        let host_pid = child.id();
        let io = CommandIo {
            stdin: child
                .stdin
                .take()
                .map(|value| Box::pin(value) as CommandWriter),
            stdout: child
                .stdout
                .take()
                .map(|value| Box::pin(value) as CommandReader),
            stderr: child
                .stderr
                .take()
                .map(|value| Box::pin(value) as CommandReader),
        };
        let delivered = ready.send(Ok((host_pid, io))).is_ok();
        let cancellation = if delivered {
            receive_cancellation.await.ok()
        } else {
            None
        };
        let revoked = task_lease.cancellation();
        let completed = match cancellation {
            Some(cancellation) => tokio::select! {
                result = child.wait() => Some(result),
                _ = cancellation.cancelled() => None,
                _ = revoked.cancelled() => None,
            },
            None => None,
        };
        let result = match completed {
            Some(result) => result,
            None => {
                if let Err(error) = docker
                    .terminate_managed_exec(&container_id, &request.process_id, host_pid)
                    .await
                {
                    task_lease
                        .lifecycle()
                        .record_cleanup_failure(ReferenceError::new(
                            ReferenceErrorCode::EnvironmentApplyFailed,
                            format!("旧 generation 的容器命令没有确认停止：{error}"),
                        ));
                }
                match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(result) => result,
                    Err(_) => {
                        let _ = child.kill().await;
                        let result = child.wait().await;
                        task_lease
                            .lifecycle()
                            .record_cleanup_failure(ReferenceError::new(
                                ReferenceErrorCode::EnvironmentApplyFailed,
                                "旧 generation 的 Docker CLI 关闭超时",
                            ));
                        result
                    }
                }
            }
        };
        result
            .map(|status| CommandExit {
                exit_code: status.code(),
                capture: None,
            })
            .map_err(|error| format!("等待容器命令退出失败：{error}"))
    })?;
    let (host_pid, io) = receive_ready.await.map_err(|error| {
        RuntimeError::InvalidInput(format!("generation 命令启动任务提前结束：{error}"))
    })??;
    Ok(ManagedCommand::new(host_pid, io, move |cancellation| {
        // 在闭包构造阶段移交取消句柄；即使 wait future 从未被轮询，Drop 也能停止真实进程。
        let _ = send_cancellation.send(cancellation);
        async move {
            task.wait()
                .await
                .map_err(|error| format!("generation 命令任务失败：{error}"))?
        }
    }))
}
