use std::path::{Component, Path};

use pl_core::shell_quote_word;
use tokio::process::Command;

use crate::args::{create_workspace_copy_container_args, validate_image};
use crate::client::{DockerClient, stderr_or_stdout};
use crate::error::{DockerError, Result};

const WORKSPACE_EXPORT_CONTAINER_ROOT: &str = "/tmp/mai-workspace-export";

/// 从工作区卷导出一组仓库相对路径，并保留它们在仓库中的相对布局。
///
/// 调用方负责选择业务路径；本类型只接受规范化的相对路径，避免导出仓库边界之外的内容。
#[derive(Debug)]
pub struct WorkspaceExportRequest<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub workspace_volume: &'a str,
    pub workspace_root: &'a str,
    pub relative_paths: &'a [String],
    pub host_path: &'a Path,
}

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
                    &format!("mkdir -p {}", shell_quote_word(&parent)),
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

    pub async fn export_workspace_paths(&self, request: &WorkspaceExportRequest<'_>) -> Result<()> {
        let image = validate_image(request.image)?;
        let stage_command =
            workspace_export_command(request.workspace_root, request.relative_paths)?;
        let args =
            create_workspace_copy_container_args(request.name, image, request.workspace_volume);
        let create = Command::new(&self.binary)
            .args(args.iter().map(String::as_str))
            .output()
            .await?;
        if !create.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&create)));
        }
        let id = String::from_utf8(create.stdout)?.trim().to_string();

        let export_result = async {
            self.start_container(&id).await?;
            let stage = self
                .exec_shell(&id, &stage_command, Some("/"), Some(120))
                .await?;
            if stage.status != 0 {
                return Err(DockerError::CommandFailed(format!(
                    "failed to stage workspace export: {}",
                    stderr_or_stdout_text(&stage.stdout, &stage.stderr)
                )));
            }
            self.copy_from_container_to_file(
                &id,
                WORKSPACE_EXPORT_CONTAINER_ROOT,
                request.host_path,
            )
            .await
        }
        .await;
        let delete_result = self.delete_container(&id).await;
        match (export_result, delete_result) {
            (Err(export_err), _) => Err(export_err),
            (Ok(()), Err(delete_err)) => Err(delete_err),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

fn workspace_export_command(workspace_root: &str, relative_paths: &[String]) -> Result<String> {
    validate_absolute_normal_path(workspace_root)?;
    if relative_paths.is_empty() {
        return Err(DockerError::InvalidWorkspaceExport(
            "at least one relative path is required".to_string(),
        ));
    }

    let mut commands = vec![
        "set -eu".to_string(),
        format!(
            "rm -rf -- {root} && mkdir -p -- {root}",
            root = shell_quote_word(WORKSPACE_EXPORT_CONTAINER_ROOT)
        ),
    ];
    for relative in relative_paths {
        validate_relative_normal_path(relative)?;
        let source = Path::new(workspace_root).join(relative);
        let destination = Path::new(WORKSPACE_EXPORT_CONTAINER_ROOT).join(relative);
        let destination_parent = destination.parent().ok_or_else(|| {
            DockerError::InvalidWorkspaceExport(format!(
                "relative path `{relative}` has no destination parent"
            ))
        })?;
        commands.push(format!(
            "mkdir -p -- {parent} && cp -a -- {source} {destination}",
            parent = shell_quote_word(&destination_parent.to_string_lossy()),
            source = shell_quote_word(&source.to_string_lossy()),
            destination = shell_quote_word(&destination.to_string_lossy()),
        ));
    }
    Ok(commands.join("\n"))
}

fn validate_absolute_normal_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(DockerError::InvalidWorkspaceExport(format!(
            "workspace root `{}` must be an absolute normalized path",
            path.display()
        )));
    }
    Ok(())
}

fn validate_relative_normal_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(DockerError::InvalidWorkspaceExport(format!(
            "workspace export path `{}` must be a normalized relative path",
            path.display()
        )));
    }
    Ok(())
}

fn stderr_or_stdout_text<'a>(stdout: &'a str, stderr: &'a str) -> &'a str {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        stdout.trim()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_handles_common_paths() {
        assert_eq!(parent_dir("/tmp/file.txt"), "/tmp");
        assert_eq!(parent_dir("relative/file.txt"), "relative");
        assert_eq!(parent_dir("file.txt"), "");
    }

    #[test]
    fn workspace_export_command_preserves_repository_layout() {
        let command = workspace_export_command(
            "/workspace/repo",
            &[".claude/skills".to_string(), ".agents/skills".to_string()],
        )
        .expect("workspace export command");

        assert_eq!(
            command,
            "set -eu\n\
             rm -rf -- /tmp/mai-workspace-export && mkdir -p -- /tmp/mai-workspace-export\n\
             mkdir -p -- /tmp/mai-workspace-export/.claude && cp -a -- /workspace/repo/.claude/skills /tmp/mai-workspace-export/.claude/skills\n\
             mkdir -p -- /tmp/mai-workspace-export/.agents && cp -a -- /workspace/repo/.agents/skills /tmp/mai-workspace-export/.agents/skills"
        );
    }
}
