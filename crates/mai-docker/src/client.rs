use tokio::process::Command;

use crate::error::{DockerError, Result};

#[derive(Debug, Clone)]
pub struct DockerClient {
    pub(crate) binary: String,
    pub(crate) image: String,
}

impl DockerClient {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            binary: "docker".to_string(),
            image: image.into(),
        }
    }

    pub fn new_with_binary(image: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
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
}

pub(crate) fn stderr_or_stdout(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}
