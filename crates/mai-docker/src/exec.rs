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
    pub mounts: &'a [(&'a str, &'a str)],
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
        for (volume, target) in params.mounts {
            cmd.args(["-v", &format!("{volume}:{target}")]);
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
        Ok(cmd.spawn()?)
    }

    pub fn spawn_sidecar(&self, params: &SidecarParams<'_>) -> Result<Child> {
        let image = validate_image(params.image)?;
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
        for (volume, target) in params.mounts {
            cmd.args(["-v", &format!("{volume}:{target}")]);
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
