use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use pl_core::{
    AgentWorkspace, ContainerBackend, ContainerExecRequest, ContainerWorkspaceFileBackend,
    PureError, WorkspaceBoundary, WorkspaceFileBackend, WorkspaceFileListRequest,
    WorkspaceFileListResult, WorkspaceFileReadBytesRequest, WorkspaceFileReadRequest,
    WorkspaceFileRemoveRequest, WorkspaceFileStat, WorkspaceFileStatRequest,
    WorkspaceFileWriteRequest, shell_quote_word,
};

use super::container::MaiContainerBackend;

/// 将 PL 统一文件工具协议绑定到 Mai 容器和冻结 workspace receipt。
///
/// PL 的容器 backend 负责传输；Mai 在产品边界实施虚拟容器路径和 directory
/// writablePaths，避免宿主路径策略被错误套用到容器路径。
#[derive(Debug, Clone)]
pub(crate) struct MaiWorkspaceFileBackend {
    backend: Arc<MaiContainerBackend>,
    inner: ContainerWorkspaceFileBackend<MaiContainerBackend>,
    policy: WorkspaceFilePolicy,
}

impl MaiWorkspaceFileBackend {
    pub(crate) fn new(backend: Arc<MaiContainerBackend>, workspace: &AgentWorkspace) -> Self {
        let read_roots = match workspace.boundary() {
            WorkspaceBoundary::HostPermitted => vec![PathBuf::from("/")],
            WorkspaceBoundary::Confined => vec![workspace.root().to_path_buf()],
        };
        let writable_roots = match (workspace.boundary(), workspace.project_writable_paths()) {
            (WorkspaceBoundary::HostPermitted, None) => vec![PathBuf::from("/")],
            (_, Some(paths)) => paths.to_vec(),
            (WorkspaceBoundary::Confined, None) => vec![workspace.root().to_path_buf()],
        };
        Self {
            backend: backend.clone(),
            inner: ContainerWorkspaceFileBackend::new(backend),
            policy: WorkspaceFilePolicy {
                default_cwd: workspace.root().to_path_buf(),
                read_roots,
                writable_roots,
            },
        }
    }

    async fn resolve_read(&self, path: &str, cwd: Option<&str>) -> pl_core::Result<String> {
        self.resolve_path(path, cwd, &self.policy.read_roots, "read_file")
            .await
    }

    async fn resolve_write(&self, path: &str, cwd: Option<&str>) -> pl_core::Result<String> {
        self.resolve_path(path, cwd, &self.policy.writable_roots, "apply_patch")
            .await
    }

    async fn resolve_path(
        &self,
        path: &str,
        cwd: Option<&str>,
        allowed_roots: &[PathBuf],
        tool: &str,
    ) -> pl_core::Result<String> {
        let lexical = self.policy.resolve(path, cwd, allowed_roots, tool)?;
        let command = format!(
            "resolved=$(readlink -f -- {path}) || exit 2; printf '%s' \"$resolved\"",
            path = shell_quote_word(&lexical),
        );
        let output = self
            .backend
            .exec(ContainerExecRequest {
                call_id: None,
                command,
                cwd: Some("/".to_string()),
                timeout_secs: Some(10),
                output_bytes_cap: None,
                cancellation_token: None,
            })
            .await
            .map_err(|error| file_error(tool, error))?;
        let resolved = PathBuf::from(output.stdout.trim());
        if output.status != 0 || !resolved.is_absolute() {
            return Err(file_error(
                tool,
                format!("container path `{lexical}` cannot be resolved"),
            ));
        }
        if allowed_roots
            .iter()
            .any(|root| path_is_within(&resolved, root))
        {
            return Ok(resolved.to_string_lossy().into_owned());
        }
        Err(file_error(
            tool,
            format!(
                "container path `{lexical}` resolves outside the frozen Agent workspace receipt"
            ),
        ))
    }
}

impl WorkspaceFileBackend for MaiWorkspaceFileBackend {
    async fn default_cwd(&self) -> pl_core::Result<String> {
        Ok(self.policy.default_cwd.to_string_lossy().into_owned())
    }

    async fn stat(&self, request: WorkspaceFileStatRequest) -> pl_core::Result<WorkspaceFileStat> {
        let path = self
            .resolve_read(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .stat(WorkspaceFileStatRequest {
                path,
                cwd: Some("/".to_string()),
            })
            .await
    }

    async fn read_text(&self, request: WorkspaceFileReadRequest) -> pl_core::Result<String> {
        let path = self
            .resolve_read(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .read_text(WorkspaceFileReadRequest {
                path,
                cwd: Some("/".to_string()),
            })
            .await
    }

    async fn read_bytes(&self, request: WorkspaceFileReadBytesRequest) -> pl_core::Result<Vec<u8>> {
        let path = self
            .resolve_read(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .read_bytes(WorkspaceFileReadBytesRequest {
                path,
                cwd: Some("/".to_string()),
                max_bytes: request.max_bytes,
            })
            .await
    }

    async fn write_text(&self, request: WorkspaceFileWriteRequest) -> pl_core::Result<()> {
        let path = self
            .resolve_write(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .write_text(WorkspaceFileWriteRequest {
                path,
                cwd: Some("/".to_string()),
                content: request.content,
            })
            .await
    }

    async fn remove_file(&self, request: WorkspaceFileRemoveRequest) -> pl_core::Result<()> {
        let path = self
            .resolve_write(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .remove_file(WorkspaceFileRemoveRequest {
                path,
                cwd: Some("/".to_string()),
            })
            .await
    }

    async fn list(
        &self,
        request: WorkspaceFileListRequest,
    ) -> pl_core::Result<WorkspaceFileListResult> {
        let path = self
            .resolve_read(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .list(WorkspaceFileListRequest {
                path,
                cwd: Some("/".to_string()),
                glob: request.glob,
                max_files: request.max_files,
                include_dirs: request.include_dirs,
            })
            .await
    }
}

#[derive(Debug, Clone)]
struct WorkspaceFilePolicy {
    default_cwd: PathBuf,
    read_roots: Vec<PathBuf>,
    writable_roots: Vec<PathBuf>,
}

impl WorkspaceFilePolicy {
    fn resolve(
        &self,
        path: &str,
        cwd: Option<&str>,
        allowed_roots: &[PathBuf],
        tool: &str,
    ) -> pl_core::Result<String> {
        let candidate = normalized_container_path(&self.default_cwd, cwd, path)
            .map_err(|error| file_error(tool, error))?;
        if allowed_roots
            .iter()
            .any(|root| path_is_within(&candidate, root))
        {
            return Ok(candidate.to_string_lossy().into_owned());
        }
        Err(file_error(
            tool,
            format!(
                "container path `{}` is outside the frozen Agent workspace receipt",
                candidate.display()
            ),
        ))
    }
}

fn normalized_container_path(
    default_cwd: &Path,
    cwd: Option<&str>,
    path: &str,
) -> std::result::Result<PathBuf, String> {
    if path.is_empty() || path.contains('\0') || path.contains('\\') {
        return Err("container path is empty or contains an invalid separator".to_string());
    }
    let cwd = match cwd {
        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
        Some(cwd) => default_cwd.join(cwd),
        None => default_cwd.to_path_buf(),
    };
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in candidate.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                return Err("container path must not contain parent traversal".to_string());
            }
            Component::Prefix(_) => {
                return Err("container path must use POSIX syntax".to_string());
            }
        }
    }
    Ok(normalized)
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    path == root || path.strip_prefix(root).is_ok()
}

fn file_error(tool: &str, error: impl std::fmt::Display) -> PureError {
    PureError::ToolExecutionFailed {
        tool: tool.to_string(),
        error: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn policy() -> WorkspaceFilePolicy {
        WorkspaceFilePolicy {
            default_cwd: PathBuf::from("/workspace/repo"),
            read_roots: vec![
                PathBuf::from("/workspace/repo"),
                PathBuf::from("/project/repo"),
            ],
            writable_roots: vec![PathBuf::from("/workspace/repo/crates/runtime")],
        }
    }

    #[test]
    fn project_view_is_readable_but_never_writable() {
        let policy = policy();
        assert_eq!(
            policy
                .resolve(
                    "/project/repo/.agents/skills/review/SKILL.md",
                    None,
                    &policy.read_roots,
                    "read_file",
                )
                .unwrap(),
            "/project/repo/.agents/skills/review/SKILL.md"
        );
        assert!(
            policy
                .resolve(
                    "/project/repo/.agents/skills/review/SKILL.md",
                    None,
                    &policy.writable_roots,
                    "apply_patch",
                )
                .is_err()
        );
    }

    #[test]
    fn frozen_directory_write_scope_rejects_escape() {
        let policy = policy();
        assert_eq!(
            policy
                .resolve(
                    "crates/runtime/src/lib.rs",
                    None,
                    &policy.writable_roots,
                    "apply_patch",
                )
                .unwrap(),
            "/workspace/repo/crates/runtime/src/lib.rs"
        );
        assert!(
            policy
                .resolve("../Cargo.toml", None, &policy.writable_roots, "apply_patch",)
                .is_err()
        );
        assert!(
            policy
                .resolve(
                    "/workspace/repo/Cargo.toml",
                    None,
                    &policy.writable_roots,
                    "apply_patch",
                )
                .is_err()
        );
        assert!(
            policy
                .resolve("/etc/passwd", None, &policy.read_roots, "read_file")
                .is_err()
        );
    }

    #[test]
    fn physical_path_outside_receipt_is_rejected_after_symlink_resolution() {
        let roots = [PathBuf::from("/workspace/repo")];
        assert!(
            !roots
                .iter()
                .any(|root| path_is_within(Path::new("/etc/passwd"), root))
        );
    }
}
