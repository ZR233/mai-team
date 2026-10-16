//! 仅供独立 daemon 的真实捕获终止验收，禁止连接生产 Docker。
use mai_docker::{ContainerTerminationOutcome, DockerClient, ExecCaptureOptions};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = std::env::var("DOCKER_HOST")?;
    if endpoint != "unix:///run/mai-reference-acceptance-docker/docker.sock" {
        return Err("仅允许共享参考源码验收专用 Docker socket".into());
    }
    let mut args = std::env::args().skip(1);
    let container = args.next().ok_or("缺少验收容器 ID")?;
    let directory = PathBuf::from(args.next().ok_or("缺少捕获证据目录")?);
    if !directory.starts_with("/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources")
    {
        return Err("捕获目录必须属于本次隔离验收".into());
    }
    std::fs::create_dir_all(&directory)?;
    let stdout = directory.join("stdout.bin");
    let stderr = directory.join("stderr.bin");
    let docker = DockerClient::new("ghcr.io/zr233/mai-team-agent:latest");
    let command = "python3 -c 'import os,time; open(\"/tmp/mai-reference-capture-probe.pid\",\"w\").write(str(os.getpid())); b=b\"x\"*65536\nwhile True: os.write(1,b); os.write(2,b)'";
    let output = docker
        .exec_shell_captured_with_cancel(
            &container,
            command,
            Some("/workspace"),
            Some(60),
            ExecCaptureOptions {
                stdout_path: &stdout,
                stderr_path: &stderr,
                output_bytes_cap: 32768,
            },
            &CancellationToken::new(),
        )
        .await?;
    let disk_bytes = std::fs::metadata(&stdout)?.len() + std::fs::metadata(&stderr)?.len();
    let failure = output
        .capture_failure
        .as_ref()
        .and_then(|failure| failure.output_limit_exceeded())
        .ok_or("未触发容量终止")?;
    if disk_bytes > mai_docker::MAX_EXEC_CAPTURE_BYTES
        || !failure.termination.docker_exec_exited
        || failure.termination.container_termination != ContainerTerminationOutcome::Issued
    {
        return Err("捕获落盘越界或未确认真实进程退出".into());
    }
    let stopped = docker.exec_shell(&container, "pid=$(cat /tmp/mai-reference-capture-probe.pid); test ! -r /proc/$pid/stat || test \"$(cut -d' ' -f3 /proc/$pid/stat)\" = Z", Some("/"), Some(5)).await?;
    if stopped.status != 0 {
        return Err("输出虽已停止接收，但容器内生产进程仍在运行".into());
    }
    println!(
        "{}",
        serde_json::json!({"disk_bytes": disk_bytes, "result": output})
    );
    Ok(())
}
