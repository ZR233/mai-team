use std::path::Path;

use tokio::process::Command;

use crate::args::{create_workspace_copy_container_args, validate_image};
use crate::client::{DockerClient, stderr_or_stdout};
use crate::error::{DockerError, Result};
use crate::exec::shell_quote;

impl DockerClient {
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

    pub async fn copy_from_container_to_file(
        &self,
        container_id: &str,
        container_path: &str,
        host_path: &Path,
    ) -> Result<()> {
        let source = format!("{container_id}:{container_path}");
        let output = Command::new(&self.binary)
            .args(["cp", &source, &host_path.to_string_lossy()])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(())
    }

    pub async fn copy_from_workspace_volume_to_file(
        &self,
        name: &str,
        image: &str,
        workspace_volume: &str,
        container_path: &str,
        host_path: &Path,
    ) -> Result<()> {
        let image = validate_image(image)?;
        let args = create_workspace_copy_container_args(name, image, workspace_volume);
        let create = Command::new(&self.binary)
            .args(args.iter().map(String::as_str))
            .output()
            .await?;
        if !create.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&create)));
        }
        let id = String::from_utf8(create.stdout)?.trim().to_string();

        let copy_result = self
            .copy_from_container_to_file(&id, container_path, host_path)
            .await;
        let delete_result = self.delete_container(&id).await;
        match (copy_result, delete_result) {
            (Err(copy_err), _) => Err(copy_err),
            (Ok(()), Err(delete_err)) => Err(delete_err),
            (Ok(()), Ok(())) => Ok(()),
        }
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
