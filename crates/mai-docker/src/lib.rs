use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use thiserror::Error;
use tokio::process::{Child, Command};

const MANAGED_LABEL: &str = "mai.team.managed=true";

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker is not available: {0}")]
    NotAvailable(String),
    #[error("docker command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("utf8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

pub type Result<T> = std::result::Result<T, DockerError>;

#[derive(Debug, Clone)]
pub struct DockerClient {
    binary: String,
    image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerHandle {
    pub id: String,
    pub name: String,
    pub image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl DockerClient {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            binary: "docker".to_string(),
            image: image.into(),
        }
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub async fn check_available(&self) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(["version", "--format", "{{.Server.Version}}"])
            .output()
            .await
            .map_err(|err| DockerError::NotAvailable(err.to_string()))?;
        if !output.status.success() {
            return Err(DockerError::NotAvailable(stderr_or_stdout(&output)));
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    pub async fn cleanup_stale_containers(&self) -> Result<Vec<String>> {
        let output = Command::new(&self.binary)
            .args(["ps", "-aq", "--filter", &format!("label={MANAGED_LABEL}")])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }

        let ids = String::from_utf8(output.stdout)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        for id in &ids {
            let _ = self.delete_container(id).await;
        }

        Ok(ids)
    }

    pub async fn create_agent_container(&self, agent_id: &str) -> Result<ContainerHandle> {
        let name = format!("mai-team-{agent_id}");
        let create = Command::new(&self.binary)
            .args([
                "create",
                "--name",
                &name,
                "--label",
                MANAGED_LABEL,
                "--label",
                &format!("mai.team.agent={agent_id}"),
                "-w",
                "/workspace",
                &self.image,
                "sleep",
                "infinity",
            ])
            .output()
            .await?;
        if !create.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&create)));
        }
        let id = String::from_utf8(create.stdout)?.trim().to_string();

        let start = Command::new(&self.binary)
            .args(["start", &id])
            .output()
            .await?;
        if !start.status.success() {
            let _ = self.delete_container(&id).await;
            return Err(DockerError::CommandFailed(stderr_or_stdout(&start)));
        }

        let mkdir = self
            .exec_shell(&id, "mkdir -p /workspace", Some("/"), Some(10))
            .await?;
        if mkdir.status != 0 {
            let _ = self.delete_container(&id).await;
            return Err(DockerError::CommandFailed(format!(
                "failed to initialize /workspace: {}",
                mkdir.stderr
            )));
        }

        Ok(ContainerHandle {
            id,
            name,
            image: self.image.clone(),
        })
    }

    pub async fn delete_container(&self, container_id: &str) -> Result<()> {
        let output = Command::new(&self.binary)
            .args(["rm", "-f", container_id])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(())
    }

    pub async fn exec_shell(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        let shell_command = match timeout_secs {
            Some(seconds) if seconds > 0 => {
                format!(
                    "timeout --preserve-status {seconds}s /bin/sh -lc {}",
                    shell_quote(command)
                )
            }
            _ => command.to_string(),
        };
        let mut cmd = Command::new(&self.binary);
        cmd.arg("exec");
        if let Some(cwd) = cwd {
            cmd.args(["-w", cwd]);
        }
        cmd.args([container_id, "/bin/sh", "-lc", &shell_command]);

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
            cmd.args(["-e", &format!("{key}={value}")]);
        }
        cmd.arg(container_id).arg(command).args(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd.spawn()?)
    }

    pub async fn copy_to_container(
        &self,
        container_id: &str,
        local_path: &Path,
        container_path: &str,
    ) -> Result<()> {
        let parent = parent_dir(container_path);
        if !parent.is_empty() {
            let mkdir = self
                .exec_shell(
                    container_id,
                    &format!("mkdir -p {}", shell_quote(&parent)),
                    Some("/"),
                    Some(10),
                )
                .await?;
            if mkdir.status != 0 {
                return Err(DockerError::CommandFailed(mkdir.stderr));
            }
        }

        let target = format!("{container_id}:{container_path}");
        let output = Command::new(&self.binary)
            .arg("cp")
            .arg(local_path)
            .arg(target)
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(())
    }

    pub async fn copy_from_container_tar(
        &self,
        container_id: &str,
        container_path: &str,
    ) -> Result<Vec<u8>> {
        let source = format!("{container_id}:{container_path}");
        let output = Command::new(&self.binary)
            .args(["cp", &source, "-"])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(output.stdout)
    }
}

fn stderr_or_stdout(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| {
            if parent.is_empty() {
                "/".to_string()
            } else {
                parent.to_string()
            }
        })
        .unwrap_or_default()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_handles_common_paths() {
        assert_eq!(parent_dir("/tmp/file.txt"), "/tmp");
        assert_eq!(parent_dir("relative/file.txt"), "relative");
        assert_eq!(parent_dir("file.txt"), "");
    }
}
