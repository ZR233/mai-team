use std::path::Path;
use std::process::{Output, Stdio};

use pl_core::{ShellCommandTimeout, shell_command_with_timeout};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use crate::args::{HOST_NETWORK, validate_image};
use crate::capture::{await_capture_task, capture_stream};
use crate::client::DockerClient;
use crate::error::{DockerError, Result};
use crate::mount::{ContainerVolumeMount, validate_additional_mounts};
use crate::naming::MANAGED_LABEL;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct ExecCaptureOptions<'a> {
    pub stdout_path: &'a Path,
    pub stderr_path: &'a Path,
    pub output_bytes_cap: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedExecOutput {
    pub output: ExecOutput,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ShellExecOptions<'a> {
    pub cwd: Option<&'a str>,
    pub timeout_secs: Option<u64>,
    pub env: &'a [(String, String)],
}

#[derive(Debug, Clone)]
pub struct SidecarParams<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub command: &'a str,
    pub args: &'a [String],
    pub cwd: Option<&'a str>,
    pub env: &'a [(String, String)],
    pub workspace_volume: Option<&'a str>,
    pub mounts: &'a [ContainerVolumeMount],
    pub timeout_secs: Option<u64>,
}

impl DockerClient {
    pub async fn exec_shell(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        self.exec_shell_env_with_cancel(
            container_id,
            command,
            &ShellExecOptions {
                cwd,
                timeout_secs,
                env: &[],
            },
            &CancellationToken::new(),
        )
        .await
    }

    pub async fn exec_shell_env(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
        env: &[(String, String)],
    ) -> Result<ExecOutput> {
        self.exec_shell_env_with_cancel(
            container_id,
            command,
            &ShellExecOptions {
                cwd,
                timeout_secs,
                env,
            },
            &CancellationToken::new(),
        )
        .await
    }

    pub async fn exec_shell_with_cancel(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecOutput> {
        self.exec_shell_env_with_cancel(
            container_id,
            command,
            &ShellExecOptions {
                cwd,
                timeout_secs,
                env: &[],
            },
            cancellation_token,
        )
        .await
    }

    pub async fn exec_shell_captured_with_cancel(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
        capture: ExecCaptureOptions<'_>,
        cancellation_token: &CancellationToken,
    ) -> Result<CapturedExecOutput> {
        self.exec_shell_env_captured_with_cancel(
            container_id,
            command,
            &ShellExecOptions {
                cwd,
                timeout_secs,
                env: &[],
            },
            capture,
            cancellation_token,
        )
        .await
    }

    pub async fn exec_shell_env_captured_with_cancel(
        &self,
        container_id: &str,
        command: &str,
        opts: &ShellExecOptions<'_>,
        capture: ExecCaptureOptions<'_>,
        cancellation_token: &CancellationToken,
    ) -> Result<CapturedExecOutput> {
        let shell_command = shell_command_with_timeout(
            command,
            ShellCommandTimeout::from_optional_seconds(opts.timeout_secs),
        );
        let mut cmd = Command::new(&self.binary);
        cmd.arg("exec");
        if let Some(cwd) = opts.cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in opts.env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.args([container_id, "/bin/sh", "-lc", &shell_command]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| {
            DockerError::CommandFailed("docker exec stdout pipe unavailable".to_string())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            DockerError::CommandFailed("docker exec stderr pipe unavailable".to_string())
        })?;
        let stdout_task = capture_stream(
            stdout,
            capture.stdout_path.to_path_buf(),
            capture.output_bytes_cap,
        );
        let stderr_task = capture_stream(
            stderr,
            capture.stderr_path.to_path_buf(),
            capture.output_bytes_cap,
        );

        let wait =
            wait_child_with_optional_timeout(&mut child, opts.timeout_secs, "docker exec command");
        let status = tokio::select! {
            status = wait => status,
            _ = cancellation_token.cancelled() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(DockerError::Cancelled);
            }
        };
        let status = match status {
            Ok(status) => status,
            Err(err) => {
                stdout_task.abort();
                stderr_task.abort();
                return Err(err);
            }
        };
        let stdout_capture = await_capture_task(stdout_task).await?;
        let stderr_capture = await_capture_task(stderr_task).await?;
        Ok(CapturedExecOutput {
            output: ExecOutput {
                status: status.code().unwrap_or(-1),
                stdout: stdout_capture.text,
                stderr: stderr_capture.text,
            },
            stdout_bytes: stdout_capture.total_bytes,
            stderr_bytes: stderr_capture.total_bytes,
            stdout_truncated: stdout_capture.truncated,
            stderr_truncated: stderr_capture.truncated,
        })
    }

    pub async fn exec_shell_env_with_cancel(
        &self,
        container_id: &str,
        command: &str,
        opts: &ShellExecOptions<'_>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecOutput> {
        let shell_command = shell_command_with_timeout(
            command,
            ShellCommandTimeout::from_optional_seconds(opts.timeout_secs),
        );
        let mut cmd = Command::new(&self.binary);
        cmd.arg("exec");
        if let Some(cwd) = opts.cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in opts.env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.args([container_id, "/bin/sh", "-lc", &shell_command]);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let output = collect_child_output_with_cancel(
            &mut child,
            opts.timeout_secs,
            "docker exec command",
            cancellation_token,
        )
        .await?;
        Ok(ExecOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }

    pub async fn run_sidecar_shell_env(&self, params: &SidecarParams<'_>) -> Result<ExecOutput> {
        let image = validate_image(params.image)?;
        validate_additional_mounts(params.mounts)?;
        let shell_command = shell_command_with_timeout(
            params.command,
            ShellCommandTimeout::from_optional_seconds(params.timeout_secs),
        );
        let mut cmd = Command::new(&self.binary);
        cmd.arg("run")
            .arg("--rm")
            .args(["--network", HOST_NETWORK])
            .args(["--name", params.name])
            .args(["--label", MANAGED_LABEL]);
        if let Some(volume) = params.workspace_volume {
            let mount = format!("{volume}:/workspace");
            cmd.args(["-v", &mount]);
        }
        for mount in params.mounts {
            cmd.args(["--mount", &mount.docker_mount_spec()]);
        }
        if let Some(cwd) = params.cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in params.env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.arg(image).args(["/bin/sh", "-lc", &shell_command]);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let output =
            collect_child_output(&mut child, params.timeout_secs, "docker sidecar command").await?;
        Ok(ExecOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }

    pub fn spawn_exec(
        &self,
        container_id: &str,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<Child> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("exec").arg("-i");
        if let Some(cwd) = cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.arg(container_id).arg(command).args(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        Ok(cmd.spawn()?)
    }

    /// 通过 `spawn_exec` 启动带容器内 PID 台账的命令，供统一 Agent 进程管理器使用。
    pub fn spawn_managed_exec(
        &self,
        container_id: &str,
        process_id: &str,
        command: &str,
        cwd: Option<&str>,
    ) -> Result<Child> {
        let pid_file = managed_exec_pid_file(process_id)?;
        let wrapped = managed_exec_shell_command(&pid_file, command);
        self.spawn_exec(
            container_id,
            "/bin/sh",
            &["-lc".to_string(), wrapped],
            cwd,
            &[],
        )
    }

    /// 同时终止容器内命令进程组和宿主 Docker CLI 进程树。
    pub async fn terminate_managed_exec(
        &self,
        container_id: &str,
        process_id: &str,
        host_pid: Option<u32>,
    ) {
        if let Ok(pid_file) = managed_exec_pid_file(process_id) {
            let _ = self
                .exec_shell(
                    container_id,
                    &managed_exec_kill_command(&pid_file),
                    Some("/"),
                    Some(5),
                )
                .await;
        }
        terminate_spawned_exec(host_pid).await;
    }

    /// 在 Drop 等同步兜底路径触发容器内清理，并终止宿主 Docker CLI 进程树。
    pub fn terminate_managed_exec_sync(
        &self,
        container_id: &str,
        process_id: &str,
        host_pid: Option<u32>,
    ) {
        if let Ok(pid_file) = managed_exec_pid_file(process_id) {
            let _ = std::process::Command::new(&self.binary)
                .arg("exec")
                .arg(container_id)
                .arg("/bin/sh")
                .args(["-lc", &managed_exec_kill_command(&pid_file)])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        terminate_spawned_exec_sync(host_pid);
    }

    /// 在容器身份不可用时至少终止宿主 Docker CLI 进程树。
    pub async fn terminate_exec_host(&self, host_pid: Option<u32>) {
        terminate_spawned_exec(host_pid).await;
    }

    /// 在同步兜底路径至少终止宿主 Docker CLI 进程树。
    pub fn terminate_exec_host_sync(&self, host_pid: Option<u32>) {
        terminate_spawned_exec_sync(host_pid);
    }

    pub fn spawn_sidecar(&self, params: &SidecarParams<'_>) -> Result<Child> {
        let image = validate_image(params.image)?;
        validate_additional_mounts(params.mounts)?;
        let mut cmd = Command::new(&self.binary);
        cmd.arg("run")
            .arg("--rm")
            .arg("-i")
            .args(["--network", HOST_NETWORK])
            .args(["--name", params.name])
            .args(["--label", MANAGED_LABEL]);
        if let Some(volume) = params.workspace_volume {
            cmd.args(["-v", &format!("{volume}:/workspace")]);
        }
        for mount in params.mounts {
            cmd.args(["--mount", &mount.docker_mount_spec()]);
        }
        if let Some(cwd) = params.cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in params.env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.arg(image).arg(params.command).args(params.args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd.spawn()?)
    }
}

fn managed_exec_shell_command(pid_file: &str, command: &str) -> String {
    format!(
        "pid_file={pid_file}; printf '%s\\n' \"$$\" > \"$pid_file\"; trap 'rm -f \"$pid_file\"' EXIT; eval {command}",
        pid_file = pl_core::shell_quote_word(pid_file),
        command = pl_core::shell_quote_word(command),
    )
}

/// 终止 `spawn_exec` 启动的宿主 Docker CLI 进程树。
async fn terminate_spawned_exec(host_pid: Option<u32>) {
    let Some(host_pid) = host_pid else { return };
    #[cfg(unix)]
    {
        let process_group = format!("-{host_pid}");
        let mut terminate = Command::new("kill");
        terminate
            .args(["-TERM", "--", &process_group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let delivered = terminate
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false);
        if delivered {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let mut kill = Command::new("kill");
        kill.args(["-KILL", "--", &process_group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let _ = kill.status().await;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &host_pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
    }
}

/// 在 Drop 等同步兜底路径终止 `spawn_exec` 的宿主 Docker CLI 进程树。
fn terminate_spawned_exec_sync(host_pid: Option<u32>) {
    let Some(host_pid) = host_pid else { return };
    #[cfg(unix)]
    {
        let process_group = format!("-{host_pid}");
        let delivered = std::process::Command::new("kill")
            .args(["-TERM", "--", &process_group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if delivered {
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &process_group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &host_pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn managed_exec_pid_file(process_id: &str) -> Result<String> {
    if process_id.is_empty()
        || !process_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(DockerError::CommandFailed(
            "invalid managed exec process id".to_string(),
        ));
    }
    Ok(format!("/tmp/mai-{process_id}.pid"))
}

fn managed_exec_kill_command(pid_file: &str) -> String {
    let pid_file = pl_core::shell_quote_word(pid_file);
    format!(
        "if test -r {pid_file}; then pid=$(cat {pid_file}); kill -TERM -- \"-$pid\" 2>/dev/null || kill -TERM \"$pid\" 2>/dev/null || true; sleep 1; kill -KILL -- \"-$pid\" 2>/dev/null || kill -KILL \"$pid\" 2>/dev/null || true; rm -f {pid_file}; fi"
    )
}

async fn wait_child_with_optional_timeout(
    child: &mut Child,
    timeout_secs: Option<u64>,
    description: &str,
) -> Result<std::process::ExitStatus> {
    match host_timeout_duration(timeout_secs) {
        Some(duration) => match timeout(duration, child.wait()).await {
            Ok(status) => Ok(status?),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Err(DockerError::CommandFailed(format!(
                    "{description} timed out after {}s",
                    timeout_secs.unwrap_or_default()
                )))
            }
        },
        None => Ok(child.wait().await?),
    }
}

async fn collect_child_output_with_cancel(
    child: &mut Child,
    timeout_secs: Option<u64>,
    description: &str,
    cancellation_token: &CancellationToken,
) -> Result<Output> {
    let stdout = child.stdout.take().ok_or_else(|| {
        DockerError::CommandFailed(format!("{description} stdout pipe unavailable"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        DockerError::CommandFailed(format!("{description} stderr pipe unavailable"))
    })?;
    let stdout_task = read_child_output_stream(stdout);
    let stderr_task = read_child_output_stream(stderr);
    let wait = wait_child_with_optional_timeout(child, timeout_secs, description);
    let status = tokio::select! {
        status = wait => status,
        _ = cancellation_token.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(DockerError::Cancelled);
        }
    };
    collect_child_output_after_status(status, stdout_task, stderr_task).await
}

async fn collect_child_output(
    child: &mut Child,
    timeout_secs: Option<u64>,
    description: &str,
) -> Result<Output> {
    let stdout = child.stdout.take().ok_or_else(|| {
        DockerError::CommandFailed(format!("{description} stdout pipe unavailable"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        DockerError::CommandFailed(format!("{description} stderr pipe unavailable"))
    })?;
    let stdout_task = read_child_output_stream(stdout);
    let stderr_task = read_child_output_stream(stderr);
    let status = wait_child_with_optional_timeout(child, timeout_secs, description).await;
    collect_child_output_after_status(status, stdout_task, stderr_task).await
}

async fn collect_child_output_after_status(
    status: Result<std::process::ExitStatus>,
    stdout_task: JoinHandle<Result<Vec<u8>>>,
    stderr_task: JoinHandle<Result<Vec<u8>>>,
) -> Result<Output> {
    let status = match status {
        Ok(status) => status,
        Err(err) => {
            stdout_task.abort();
            stderr_task.abort();
            return Err(err);
        }
    };
    let stdout = await_output_task(stdout_task).await?;
    let stderr = await_output_task(stderr_task).await?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_child_output_stream<R>(reader: R) -> JoinHandle<Result<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(bytes)
    })
}

async fn await_output_task(task: JoinHandle<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    match task.await {
        Ok(result) => result,
        Err(err) => Err(DockerError::CommandFailed(format!(
            "stream output task failed: {err}"
        ))),
    }
}

fn host_timeout_duration(timeout_secs: Option<u64>) -> Option<Duration> {
    timeout_secs
        .filter(|seconds| *seconds > 0)
        .map(|seconds| Duration::from_secs(seconds + 5))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn exec_shell_command_omits_timeout_wrapper_when_unlimited() {
        assert_eq!(
            shell_command_with_timeout(
                "sleep 1000",
                ShellCommandTimeout::from_optional_seconds(None)
            ),
            "sleep 1000"
        );
        assert_eq!(
            shell_command_with_timeout(
                "sleep 1000",
                ShellCommandTimeout::from_optional_seconds(Some(0))
            ),
            "sleep 1000"
        );
        assert!(
            shell_command_with_timeout(
                "sleep 1000",
                ShellCommandTimeout::from_optional_seconds(Some(5))
            )
            .starts_with("timeout --preserve-status 5s /bin/sh -lc ")
        );
    }

    #[test]
    fn shell_command_helpers_delegate_to_pl_core() {
        let source = include_str!("exec.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production section");

        assert!(production.contains("shell_command_with_timeout"));
        assert!(production.contains("ShellCommandTimeout"));
        assert!(!production.contains("shell_words::quote"));
        assert!(!production.contains("fn shell_command_with_optional_timeout"));
        assert!(!production.contains("fn shell_quote"));
    }

    #[tokio::test]
    async fn sidecar_shell_env_times_out_stuck_docker_process() {
        let script = fake_docker_script("exec sleep 30\n");
        let client = DockerClient::new_with_binary("unused-image", script.to_string_lossy());

        let result = client
            .run_sidecar_shell_env(&SidecarParams {
                name: "mai-test-sidecar-timeout",
                image: "unused-image",
                command: "true",
                args: &[],
                cwd: None,
                env: &[],
                workspace_volume: None,
                mounts: &[],
                timeout_secs: Some(1),
            })
            .await;

        assert_timed_out(result);
    }

    #[tokio::test]
    async fn exec_shell_env_times_out_stuck_docker_process() {
        let script = fake_docker_script("exec sleep 30\n");
        let client = DockerClient::new_with_binary("unused-image", script.to_string_lossy());

        let result = client.exec_shell("container", "true", None, Some(1)).await;

        assert_timed_out(result);
    }

    #[tokio::test]
    async fn exec_shell_captured_times_out_stuck_docker_process() {
        let script = fake_docker_script("exec sleep 30\n");
        let client = DockerClient::new_with_binary("unused-image", script.to_string_lossy());
        let dir = tempfile_dir();
        let stdout_path = dir.join("stdout.txt");
        let stderr_path = dir.join("stderr.txt");

        let result = client
            .exec_shell_captured_with_cancel(
                "container",
                "true",
                None,
                Some(1),
                ExecCaptureOptions {
                    stdout_path: &stdout_path,
                    stderr_path: &stderr_path,
                    output_bytes_cap: 1024,
                },
                &CancellationToken::new(),
            )
            .await;

        assert_timed_out(result);
    }

    #[tokio::test]
    async fn managed_exec_uses_spawn_exec_with_pid_ledger_and_workspace_cwd() {
        let dir = tempfile_dir();
        let args_file = dir.join("args.txt");
        let script = fake_docker_script(&format!(
            "printf '%s\\n' \"$@\" > {}\nprintf ready\n",
            pl_core::shell_quote_word(&args_file.display().to_string())
        ));
        let client = DockerClient::new_with_binary("unused-image", script.to_string_lossy());

        let child = client
            .spawn_managed_exec(
                "container-1",
                "proc-7",
                "printf 'hello world'",
                Some("/workspace/repo"),
            )
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        let args = fs::read_to_string(args_file).unwrap();

        assert_eq!(String::from_utf8(output.stdout).unwrap(), "ready");
        assert!(args.contains("-w\n/workspace/repo"), "{args}");
        assert!(args.contains("container-1"), "{args}");
        assert!(args.contains("/tmp/mai-proc-7.pid"), "{args}");
        assert!(
            args.contains("eval ") && args.contains("hello world"),
            "{args}"
        );
    }

    #[test]
    fn managed_exec_wrapper_preserves_multiline_heredocs_and_quotes() {
        let pid_file = std::env::temp_dir().join(format!(
            "mai-managed-exec-wrapper-{}.pid",
            unique_test_path_id()
        ));
        let command = "set -e\ncat <<'FIRST'\nalpha 'beta'\nFIRST\nprintf '%s\\n' separator\ncat <<'SECOND'\ngamma\nSECOND";
        let wrapped = managed_exec_shell_command(&pid_file.display().to_string(), command);

        let output = std::process::Command::new("/bin/sh")
            .args(["-lc", &wrapped])
            .output()
            .expect("run managed command wrapper");

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "alpha 'beta'\nseparator\ngamma\n"
        );
        assert!(!pid_file.exists());
    }

    #[test]
    fn managed_exec_rejects_untrusted_process_id() {
        let client = DockerClient::new("unused-image");
        let error = client
            .spawn_managed_exec("container", "../escape", "true", Some("/workspace"))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid managed exec process id")
        );
    }

    fn fake_docker_script(body: &str) -> std::path::PathBuf {
        let unique = unique_test_path_id();
        let dir = std::env::temp_dir().join(format!("mai-docker-test-{unique}"));
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("docker");
        let mut file = fs::File::create(&path).expect("create script");
        file.write_all(format!("#!/bin/sh\n{body}").as_bytes())
            .expect("write script");
        file.sync_all().expect("sync script");
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let unique = unique_test_path_id();
        let dir = std::env::temp_dir().join(format!("mai-docker-capture-test-{unique}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn unique_test_path_id() -> String {
        let counter = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        format!("{}-{counter}-{nanos}", std::process::id())
    }

    fn assert_timed_out<T: std::fmt::Debug>(result: Result<T>) {
        assert!(
            matches!(result, Err(DockerError::CommandFailed(ref message)) if message.contains("timed out")),
            "expected docker command timeout error, got {result:?}"
        );
    }
}
