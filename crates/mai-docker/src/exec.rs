use std::path::Path;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::args::validate_image;
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
        let shell_command = shell_command_with_optional_timeout(command, opts.timeout_secs);
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

        let status = tokio::select! {
            status = child.wait() => status?,
            _ = cancellation_token.cancelled() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                await_capture_task(stdout_task).await?;
                await_capture_task(stderr_task).await?;
                return Err(DockerError::Cancelled);
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
        let shell_command = shell_command_with_optional_timeout(command, opts.timeout_secs);
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

        cmd.kill_on_drop(true);
        let output = cmd.output();
        tokio::pin!(output);
        let output = tokio::select! {
            output = &mut output => output?,
            _ = cancellation_token.cancelled() => {
                return Err(DockerError::Cancelled);
            }
        };
        Ok(ExecOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }

    pub async fn run_sidecar_shell_env(&self, params: &SidecarParams<'_>) -> Result<ExecOutput> {
        let image = validate_image(params.image)?;
        let shell_command =
            shell_command_with_optional_timeout(params.command, params.timeout_secs);
        let mut cmd = Command::new(&self.binary);
        cmd.arg("run")
            .arg("--rm")
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

        let output = cmd.output().await?;
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

pub(crate) fn shell_command_with_optional_timeout(
    command: &str,
    timeout_secs: Option<u64>,
) -> String {
    match timeout_secs {
        Some(seconds) if seconds > 0 => {
            format!(
                "timeout --preserve-status {seconds}s /bin/sh -lc {}",
                shell_quote(command)
            )
        }
        _ => command.to_string(),
    }
}

pub(crate) fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_shell_command_omits_timeout_wrapper_when_unlimited() {
        assert_eq!(
            shell_command_with_optional_timeout("sleep 1000", None),
            "sleep 1000"
        );
        assert_eq!(
            shell_command_with_optional_timeout("sleep 1000", Some(0)),
            "sleep 1000"
        );
        assert!(
            shell_command_with_optional_timeout("sleep 1000", Some(5))
                .starts_with("timeout --preserve-status 5s /bin/sh -lc ")
        );
    }
}
