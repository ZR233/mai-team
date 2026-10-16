use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_references::AgentEnvironmentLease;
use pl_protocol::PureError;
use pl_tool::container::{ContainerBackend, ContainerExecRequest};
use pl_tool::shell::shell_quote_word;
use pl_tool::workspace::{AgentWorkspace, WorkspaceBoundary};
use pl_tool::workspace_file::{
    ContainerWorkspaceFileBackend, WorkspaceFileBackend, WorkspaceFileListRequest,
    WorkspaceFileListResult, WorkspaceFileReadBytesRequest, WorkspaceFileReadRequest,
    WorkspaceFileRemoveRequest, WorkspaceFileStat, WorkspaceFileStatRequest,
    WorkspaceFileWriteRequest,
};

use super::container::{
    ContainerWritePolicy, MaiContainerBackend, normalize_container_path, path_is_within,
};

/// 将 PL 统一文件工具协议绑定到 Mai 容器和冻结 workspace receipt。
///
/// PL 的容器 backend 负责传输；Mai 在产品边界实施虚拟容器路径和 directory
/// writablePaths，避免宿主路径策略被错误套用到容器路径。
///
/// 读根 = 原读取根 ∪ 本 generation 已应用的各 `/references/<alias>`；写范围保持原样，但整个
/// `/references` 命名空间优先拒写。写/删除与 `docker cp` 上传都走
/// [`MaiContainerBackend::authorize_container_write`] 的同一套规范化 + 容器内解析两次校验。
#[derive(Debug, Clone)]
pub(crate) struct MaiWorkspaceFileBackend {
    backend: Arc<MaiContainerBackend>,
    inner: ContainerWorkspaceFileBackend<MaiContainerBackend>,
    policy: WorkspaceFilePolicy,
}

impl MaiWorkspaceFileBackend {
    pub(crate) fn new(backend: Arc<MaiContainerBackend>, workspace: &AgentWorkspace) -> Self {
        let mut read_roots = match workspace.boundary() {
            WorkspaceBoundary::HostPermitted => vec![PathBuf::from("/")],
            WorkspaceBoundary::Confined => vec![workspace.root().to_path_buf()],
        };
        // 参考源是**额外只读根**：只加读取，不改写范围，也不改变 reviewer 的只读语义。
        // 只取租约里的 applied 快照，不重新查项目配置，因此读根与实际挂载的 generation 一致。
        read_roots.extend(reference_read_roots(backend.lease()));
        let writable_roots = match (workspace.boundary(), workspace.writable_paths()) {
            (WorkspaceBoundary::HostPermitted, None) => vec![PathBuf::from("/")],
            (_, Some(paths)) => paths.to_vec(),
            (WorkspaceBoundary::Confined, None) => vec![workspace.root().to_path_buf()],
        };
        Self {
            backend: backend.clone(),
            inner: ContainerWorkspaceFileBackend::new(backend),
            policy: WorkspaceFilePolicy::new(
                workspace.root().to_path_buf(),
                read_roots,
                writable_roots,
            ),
        }
    }

    /// 读路径：先按读根判规范化后的容器路径，再在容器内解析 readlink 并判解析后的最终路径。
    ///
    /// 参考源只是额外读根，因此 reviewer 的只读语义、原读取范围都不变。
    async fn resolve_read(&self, path: &str, cwd: Option<&str>) -> pl_protocol::Result<String> {
        let lexical = self.policy.resolve_read(path, cwd, "read_file")?;
        let resolved = self.resolve_in_container(&lexical, "read_file").await?;
        self.policy
            .authorize_read(Path::new(&resolved), "read_file")
    }

    /// 写路径检查：规范化词法路径与容器内解析后的最终路径各判一次，`/references` 优先于写许可。
    async fn resolve_write(&self, path: &str, cwd: Option<&str>) -> pl_protocol::Result<String> {
        self.backend
            .authorize_container_write(&self.policy.write_policy, cwd, path)
            .await
            .map_err(|refusal| file_error("apply_patch", refusal.message()))
    }

    /// 在容器内解析一个已归一化容器路径的最终形态；解析失败显式报错，不降级成未校验路径。
    async fn resolve_in_container(&self, lexical: &str, tool: &str) -> pl_protocol::Result<String> {
        let command = format!(
            "resolved=$(readlink -f -- {path}) || exit 2; printf '%s' \"$resolved\"",
            path = shell_quote_word(lexical),
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
        Ok(resolved.to_string_lossy().into_owned())
    }
}

/// 本 generation 已应用的参考源在容器内的只读路径。
///
/// 只来自租约持有的 applied 快照：同一 Thread 的读根与实际挂载的 generation 必须一致，
/// 不能按 agent 重新查一个已经换代的环境。
fn reference_read_roots(lease: &AgentEnvironmentLease) -> Vec<PathBuf> {
    lease
        .references()
        .references
        .iter()
        .map(|reference| PathBuf::from(&reference.container_path))
        .collect()
}

impl WorkspaceFileBackend for MaiWorkspaceFileBackend {
    async fn default_cwd(&self) -> pl_protocol::Result<String> {
        Ok(self.policy.default_cwd.to_string_lossy().into_owned())
    }

    async fn stat_optional(
        &self,
        request: WorkspaceFileStatRequest,
    ) -> pl_protocol::Result<Option<WorkspaceFileStat>> {
        let path = self
            .resolve_read(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .stat_optional(WorkspaceFileStatRequest {
                path,
                cwd: Some("/".to_string()),
            })
            .await
    }

    async fn read_text(&self, request: WorkspaceFileReadRequest) -> pl_protocol::Result<String> {
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

    async fn read_bytes(
        &self,
        request: WorkspaceFileReadBytesRequest,
    ) -> pl_protocol::Result<Vec<u8>> {
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

    async fn write_text(&self, request: WorkspaceFileWriteRequest) -> pl_protocol::Result<()> {
        let path = self
            .resolve_write(&request.path, request.cwd.as_deref())
            .await?;
        self.inner
            .write_text(WorkspaceFileWriteRequest {
                mode: request.mode,
                path,
                cwd: Some("/".to_string()),
                content: request.content,
            })
            .await
    }

    async fn remove_file(&self, request: WorkspaceFileRemoveRequest) -> pl_protocol::Result<()> {
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
    ) -> pl_protocol::Result<WorkspaceFileListResult> {
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
    /// 写许可：`/references` 整个命名空间优先拒写，其余沿用冻结后的原写范围。
    write_policy: ContainerWritePolicy,
}

impl WorkspaceFilePolicy {
    fn new(default_cwd: PathBuf, read_roots: Vec<PathBuf>, writable_roots: Vec<PathBuf>) -> Self {
        Self {
            write_policy: ContainerWritePolicy::within(default_cwd.clone(), writable_roots),
            default_cwd,
            read_roots,
        }
    }

    /// 读路径的规范化与读根判定；解析后的最终路径由调用方再判一次。
    fn resolve_read(
        &self,
        path: &str,
        cwd: Option<&str>,
        tool: &str,
    ) -> pl_protocol::Result<String> {
        let candidate = normalize_container_path(&self.default_cwd, cwd, path)
            .map_err(|error| file_error(tool, error))?;
        self.authorize_read(&candidate, tool)
    }

    /// 读许可判定：按路径组件边界比较，不做字符串前缀比较。
    fn authorize_read(&self, path: &Path, tool: &str) -> pl_protocol::Result<String> {
        if self
            .read_roots
            .iter()
            .any(|root| path_is_within(path, root))
        {
            return Ok(path.to_string_lossy().into_owned());
        }
        Err(file_error(
            tool,
            format!(
                "container path `{}` is outside the frozen Agent workspace receipt",
                path.display()
            ),
        ))
    }
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
    use crate::turn::container::ContainerWriteRefusal;
    use pretty_assertions::assert_eq;

    fn policy() -> WorkspaceFilePolicy {
        WorkspaceFilePolicy::new(
            PathBuf::from("/workspace/repo"),
            vec![
                PathBuf::from("/workspace/repo"),
                PathBuf::from("/project/repo"),
                PathBuf::from("/references/linux"),
            ],
            vec![PathBuf::from("/workspace/repo/crates/runtime")],
        )
    }

    #[test]
    fn project_view_is_readable_but_never_writable() {
        let policy = policy();
        assert_eq!(
            policy
                .resolve_read(
                    "/project/repo/.agents/skills/review/SKILL.md",
                    None,
                    "read_file",
                )
                .unwrap(),
            "/project/repo/.agents/skills/review/SKILL.md"
        );
        assert_eq!(
            Err(ContainerWriteRefusal::OutsideWritableRoot {
                path: "/project/repo/.agents/skills/review/SKILL.md".to_string(),
            }),
            policy
                .write_policy
                .authorize(Path::new("/project/repo/.agents/skills/review/SKILL.md"))
        );
    }

    #[test]
    fn frozen_directory_write_scope_rejects_escape() {
        let policy = policy();
        assert_eq!(
            Ok(()),
            policy
                .write_policy
                .authorize(Path::new("/workspace/repo/crates/runtime/src/lib.rs"))
        );
        assert!(normalize_container_path(&policy.default_cwd, None, "../Cargo.toml").is_err());
        assert_eq!(
            Err(ContainerWriteRefusal::OutsideWritableRoot {
                path: "/workspace/repo/Cargo.toml".to_string(),
            }),
            policy
                .write_policy
                .authorize(Path::new("/workspace/repo/Cargo.toml"))
        );
        assert!(
            policy
                .resolve_read("/etc/passwd", None, "read_file")
                .is_err()
        );
    }

    #[test]
    fn reference_namespace_is_readable_and_never_writable() {
        let policy = policy();
        assert_eq!(
            policy
                .resolve_read("/references/linux/kernel/irq/manage.c", None, "read_file")
                .unwrap(),
            "/references/linux/kernel/irq/manage.c"
        );
        // 拒写范围优先于任何写许可：即使 Agent 的写根放大到 `/`，引用命名空间也不可写。
        let host_permitted =
            WorkspaceFilePolicy::new(PathBuf::from("/"), Vec::new(), vec![PathBuf::from("/")]);
        assert_eq!(
            Err(ContainerWriteRefusal::ReferenceReadOnly {
                path: "/references/linux/kernel/irq/manage.c".to_string(),
            }),
            host_permitted
                .write_policy
                .authorize(Path::new("/references/linux/kernel/irq/manage.c"))
        );
    }

    #[test]
    fn reserved_namespace_boundary_is_compared_by_components() {
        // `/references-evil` 与 `/references` 共享字符串前缀，但不是保留命名空间内的路径。
        assert!(!path_is_within(
            Path::new("/references-evil/repo"),
            Path::new("/references")
        ));
        assert!(path_is_within(
            Path::new("/references/linux"),
            Path::new("/references")
        ));
        assert!(path_is_within(
            Path::new("/references"),
            Path::new("/references")
        ));
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
