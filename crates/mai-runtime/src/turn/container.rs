use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use mai_docker::ExecCaptureOptions;
use mai_protocol::{AgentId, ToolOutputArtifactInfo, now};
use mai_references::AgentEnvironmentLease;
use pl_tool::container::{
    ContainerBackend, ContainerCopyFromRequest, ContainerCopyToRequest, ContainerExecOutput,
    ContainerExecRequest,
};
use pl_tool::shell::shell_quote_word;
use uuid::Uuid;

use crate::{AgentRuntime, Result, RuntimeError};

/// 参考源码在容器内的保留命名空间。
///
/// 整个子树只读：写、删除、重命名、上传与 `copy_to` 都必须在任何写许可**之前**拒绝它，
/// 包括通过 workspace 内符号链接指向该命名空间的情况。
pub(crate) const REFERENCES_CONTAINER_ROOT: &str = "/references";

/// 一个 Thread 的容器后端，绑定**冻结的环境租约**。
///
/// 所有 exec/readlink/cp 都只使用租约里的 container_id：同一个 turn 内绝不重新按 agent 查容器，
/// 因此换代后残存的句柄会 fail closed（按取代/取消报告，并记录带 typed code 的日志），而不是被
/// 悄悄切到新环境。
#[derive(Clone)]
pub(crate) struct MaiContainerBackend {
    runtime: Arc<AgentRuntime>,
    lease: Arc<AgentEnvironmentLease>,
}

impl MaiContainerBackend {
    pub(crate) fn new(runtime: Arc<AgentRuntime>, lease: Arc<AgentEnvironmentLease>) -> Self {
        Self { runtime, lease }
    }

    /// 冻结的环境租约；文件后端从这里取本 generation 的参考只读根。
    pub(crate) fn lease(&self) -> &Arc<AgentEnvironmentLease> {
        &self.lease
    }

    /// 本 generation 的容器 ID。
    ///
    /// 租约已被撤销时 fail closed：返回 `EnvironmentSuperseded`/`Cancelled` 语义的错误，
    /// 不重新按 agent 解析容器。
    fn container_id(&self) -> Result<String> {
        ensure_lease_active(&self.lease)?;
        Ok(self.lease.container_id().to_string())
    }

    /// 统一写路径检查：规范化容器路径 → 引用命名空间优先拒写与原写范围 → 容器内
    /// `readlink -f` 解析 → 对解析后的最终路径**再判一次**。
    ///
    /// 返回的是解析后的容器路径，调用方后续的 copy/exec 都用它。文件工具的写/删除、上传与
    /// `copy_to` 全部复用这一个方法，不允许各自实现放宽版本。词法路径按容器语义处理，
    /// 绝不把它当宿主路径 canonicalize。
    pub(crate) async fn authorize_container_write(
        &self,
        policy: &ContainerWritePolicy,
        cwd: Option<&str>,
        path: &str,
    ) -> std::result::Result<String, ContainerWriteRefusal> {
        let lexical = normalize_container_path(policy.default_cwd(), cwd, path)
            .map_err(ContainerWriteRefusal::InvalidPath)?;
        policy.authorize(&lexical)?;
        let resolved = self.resolve_container_path(&lexical).await?;
        policy.authorize(&resolved)?;
        Ok(resolved.to_string_lossy().into_owned())
    }

    /// 在容器内解析一个容器路径的最终形态。
    ///
    /// `readlink -m` 解析全部已存在的符号链接并允许新建的父目录；解析失败
    /// 一律显式拒绝，不降级成"未校验路径"。
    pub(crate) async fn resolve_container_path(
        &self,
        lexical: &Path,
    ) -> std::result::Result<PathBuf, ContainerWriteRefusal> {
        let container_id =
            self.container_id()
                .map_err(|error| ContainerWriteRefusal::Unresolvable {
                    path: lexical.display().to_string(),
                    reason: error.to_string(),
                })?;
        let candidate = lexical.to_str().ok_or_else(|| {
            ContainerWriteRefusal::InvalidPath("container path must be valid UTF-8".to_string())
        })?;
        let command = format!(
            "resolved=$(readlink -m -- {candidate}) || exit 2; printf '%s' \"$resolved\"",
            candidate = shell_quote_word(candidate),
        );
        let output = self
            .runtime
            .deps
            .docker
            .exec_shell(&container_id, &command, Some("/"), Some(10))
            .await
            .map_err(|error| ContainerWriteRefusal::Unresolvable {
                path: lexical.display().to_string(),
                reason: error.to_string(),
            })?;
        let resolved = PathBuf::from(output.stdout.trim());
        if output.status != 0 || !resolved.is_absolute() {
            return Err(ContainerWriteRefusal::Unresolvable {
                path: lexical.display().to_string(),
                reason: "the container could not resolve the path".to_string(),
            });
        }
        Ok(resolved)
    }
}

impl std::fmt::Debug for MaiContainerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaiContainerBackend")
            .field("agent_id", &self.lease.agent_id())
            .field("container_id", &self.lease.container_id())
            .field("generation", &self.lease.generation())
            .finish()
    }
}

impl ContainerBackend for MaiContainerBackend {
    type Error = RuntimeError;

    async fn exec(&self, request: ContainerExecRequest) -> Result<ContainerExecOutput> {
        let agent_id = self.lease.agent_id();
        execute_with_container_backend(
            &self.runtime.deps.docker,
            &self.runtime.artifact_files_root,
            self.container_id(),
            agent_id,
            request,
        )
        .await
    }

    async fn copy_from(&self, request: ContainerCopyFromRequest) -> Result<Vec<u8>> {
        copy_from_container_backend(&self.runtime.deps.docker, self.container_id(), request).await
    }

    async fn copy_to(&self, request: ContainerCopyToRequest) -> Result<()> {
        // `docker cp` 是文件写入与上传的公共落点，因此这里再走一次同一套规则：
        // 直接或经符号链接指向 `/references` 一律拒绝，其余沿用调用方自己的写许可。
        let policy = ContainerWritePolicy::references_denied("/");
        let path = self
            .authorize_container_write(&policy, None, &request.path)
            .await
            .map_err(write_refusal_error)?;
        copy_to_container_backend(
            &self.runtime.deps.docker,
            self.container_id()?,
            ContainerCopyToRequest {
                path,
                content: request.content,
            },
        )
        .await
    }
}

/// 参考命名空间之外的写许可范围。
#[derive(Debug, Clone)]
pub(crate) enum ContainerWriteScope {
    /// 只拒绝保留命名空间；其余沿用调用方自己的既有许可（例如上传落点）。
    ReferencesDenied,
    /// 再收紧到这些容器写根；保留命名空间仍然优先拒绝。
    Roots(Vec<PathBuf>),
}

/// 容器写路径策略：保留命名空间永远优先拒绝，然后才是调用方的写许可。
///
/// 策略只描述"哪些容器路径可写"，不参与路径解析；解析与两次校验由
/// [`MaiContainerBackend::authorize_container_write`] 统一完成，避免各写入口各自实现一套。
#[derive(Debug, Clone)]
pub(crate) struct ContainerWritePolicy {
    default_cwd: PathBuf,
    scope: ContainerWriteScope,
}

impl ContainerWritePolicy {
    /// 只拒绝 `/references` 的策略：给需要同一套规范化、解析与拒绝顺序的入口复用。
    pub(crate) fn references_denied(default_cwd: impl Into<PathBuf>) -> Self {
        Self {
            default_cwd: default_cwd.into(),
            scope: ContainerWriteScope::ReferencesDenied,
        }
    }

    /// 冻结在若干写根内的策略；写根为空表示该 Thread 没有任何可写目录。
    pub(crate) fn within(default_cwd: impl Into<PathBuf>, writable_roots: Vec<PathBuf>) -> Self {
        Self {
            default_cwd: default_cwd.into(),
            scope: ContainerWriteScope::Roots(writable_roots),
        }
    }

    pub(crate) fn default_cwd(&self) -> &Path {
        &self.default_cwd
    }

    /// 引用命名空间优先于任何写许可；随后才判写根。
    pub(crate) fn authorize(&self, path: &Path) -> std::result::Result<(), ContainerWriteRefusal> {
        if path_is_within(path, Path::new(REFERENCES_CONTAINER_ROOT)) {
            return Err(ContainerWriteRefusal::ReferenceReadOnly {
                path: path.display().to_string(),
            });
        }
        match &self.scope {
            ContainerWriteScope::ReferencesDenied => Ok(()),
            ContainerWriteScope::Roots(roots) => {
                if roots.iter().any(|root| path_is_within(path, root)) {
                    Ok(())
                } else {
                    Err(ContainerWriteRefusal::OutsideWritableRoot {
                        path: path.display().to_string(),
                    })
                }
            }
        }
    }
}

/// 写路径被拒绝的原因。
///
/// 调用方把它映射到自己的错误类型（工具错误或产品错误）并保留可读原因，不需要解析文本，
/// 因此"写入引用"与"越出写范围"不会退化成一个笼统的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContainerWriteRefusal {
    /// 路径不合法：空、含非法分隔符、`..` traversal 或非 POSIX 语法。
    InvalidPath(String),
    /// 目标落在只读的 `/references` 命名空间内（词法路径或容器内解析后的最终路径命中）。
    ReferenceReadOnly { path: String },
    /// 目标不在本 Thread 的写许可范围内。
    OutsideWritableRoot { path: String },
    /// 容器内无法解析该路径（父路径不存在，或租约已撤销导致容器不可用）。
    Unresolvable { path: String, reason: String },
}

impl ContainerWriteRefusal {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::InvalidPath(reason) => format!("invalid container path: {reason}"),
            Self::ReferenceReadOnly { path } => format!(
                "container path `{path}` is inside the read-only reference namespace {REFERENCES_CONTAINER_ROOT}"
            ),
            Self::OutsideWritableRoot { path } => {
                format!(
                    "container path `{path}` is outside the writable paths of this Agent workspace"
                )
            }
            Self::Unresolvable { path, reason } => {
                format!("container path `{path}` could not be resolved: {reason}")
            }
        }
    }
}

fn write_refusal_error(refusal: ContainerWriteRefusal) -> RuntimeError {
    RuntimeError::InvalidInput(refusal.message())
}

/// 归一化容器路径：以 `/` 为根、按组件消解 `.`，拒绝 `..` traversal、反斜杠、NUL 与非 POSIX 前缀。
pub(crate) fn normalize_container_path(
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

/// 引用命名空间等保留根的边界判定：按路径组件边界，不做字符串前缀比较。
pub(crate) fn path_is_within(path: &Path, root: &Path) -> bool {
    path == root || path.strip_prefix(root).is_ok()
}

/// 租约撤销必须 fail closed：旧 generation 的句柄不得改用新容器。
pub(crate) fn ensure_lease_active(lease: &AgentEnvironmentLease) -> Result<()> {
    lease.ensure_active().map_err(|error| {
        tracing::warn!(
            agent_id = %lease.agent_id(),
            generation = %lease.generation(),
            container_id = %lease.container_id(),
            error = %error,
            "refusing to use a revoked environment lease"
        );
        // 环境换代/撤销的语义是"这次调用被取代并取消"，因此按取消报告；
        // 需要精确类别时由上层读租约自己的 typed code。
        RuntimeError::Reference(error)
    })
}

async fn execute_with_container_backend(
    docker: &mai_docker::DockerClient,
    artifact_files_root: &Path,
    container_id: Result<String>,
    agent_id: AgentId,
    request: ContainerExecRequest,
) -> Result<ContainerExecOutput> {
    let container_id = container_id?;
    let cancellation_token = request.cancellation_token.unwrap_or_default();
    if let Some(output_bytes_cap) = request.output_bytes_cap {
        let call_id = request
            .call_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let stdout_id = Uuid::new_v4().to_string();
        let stderr_id = Uuid::new_v4().to_string();
        let namespace = agent_id.to_string();
        let capture = MaiToolOutputCapture::prepare(
            artifact_files_root,
            &namespace,
            &call_id,
            &stdout_id,
            &stderr_id,
            &request.command,
        )
        .await?;
        let output = docker
            .exec_shell_captured_with_cancel(
                &container_id,
                &request.command,
                request.cwd.as_deref(),
                request.timeout_secs,
                ExecCaptureOptions {
                    stdout_path: capture.stdout_path(),
                    stderr_path: capture.stderr_path(),
                    output_bytes_cap,
                },
                &cancellation_token,
            )
            .await?;
        if let Some(failure) = &output.capture_failure {
            return Err(mai_references::ReferenceError::new(
                mai_protocol::ReferenceErrorCode::OutputLimitExceeded,
                failure.message(),
            )
            .into());
        }
        let artifacts = capture
            .collect_artifacts(
                agent_id,
                MaiToolOutputStreamSizes {
                    stdout_bytes: output.stdout_bytes,
                    stderr_bytes: output.stderr_bytes,
                },
            )
            .await?;
        let output_artifacts = artifacts
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(runtime_invalid_input)?;
        return Ok(ContainerExecOutput {
            status: output.output.status,
            stdout: output.output.stdout,
            stderr: output.output.stderr,
            stdout_truncated: output.stdout_truncated,
            stderr_truncated: output.stderr_truncated,
            stdout_bytes: output.stdout_bytes,
            stderr_bytes: output.stderr_bytes,
            output_artifacts,
        });
    }

    let output = docker
        .exec_shell_with_cancel(
            &container_id,
            &request.command,
            request.cwd.as_deref(),
            request.timeout_secs,
            &cancellation_token,
        )
        .await?;
    Ok(ContainerExecOutput {
        status: output.status,
        stdout_bytes: output.stdout.len() as u64,
        stderr_bytes: output.stderr.len() as u64,
        stdout: output.stdout,
        stderr: output.stderr,
        stdout_truncated: false,
        stderr_truncated: false,
        output_artifacts: Vec::new(),
    })
}

async fn copy_from_container_backend(
    docker: &mai_docker::DockerClient,
    container_id: Result<String>,
    request: ContainerCopyFromRequest,
) -> Result<Vec<u8>> {
    let container_id = container_id?;
    if request.archive {
        return Ok(docker
            .copy_from_container_tar(&container_id, &request.path)
            .await?);
    }
    let dir = tempfile::tempdir()?;
    let host_path = dir.path().join("file");
    docker
        .copy_from_container_to_file(&container_id, &request.path, &host_path)
        .await?;
    Ok(tokio::fs::read(&host_path).await?)
}

async fn copy_to_container_backend(
    docker: &mai_docker::DockerClient,
    container_id: String,
    request: ContainerCopyToRequest,
) -> Result<()> {
    let temp = tempfile::NamedTempFile::new()?;
    std::fs::write(temp.path(), &request.content)?;
    docker
        .copy_to_container(&container_id, temp.path(), &request.path)
        .await?;
    Ok(())
}

/// 单个输出流的累计原始字节数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MaiToolOutputStreamSizes {
    pub(super) stdout_bytes: u64,
    pub(super) stderr_bytes: u64,
}

/// Mai 侧容器命令输出的 artifact 捕获。
///
/// pl-core 原有的 `tool::output_format::capture` 已整体删除，Mai 仍需为统一命令 backend
/// 与容器 exec 保留同一套 artifact 语义：stdout/stderr 各自一个宿主文件，路径布局保持
/// `tool-output/<namespace>/<call>/<artifact>/<name>`，空流在收集时删除、不生成 artifact。
/// 这里只实现 Mai 需要的这部分边界，不复制 PL 的通用命令/文件工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MaiToolOutputCapture {
    call_id: String,
    stdout: MaiToolOutputStreamCapture,
    stderr: MaiToolOutputStreamCapture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MaiToolOutputStreamCapture {
    id: String,
    name: String,
    stream: MaiToolOutputStream,
    path: std::path::PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaiToolOutputStream {
    Stdout,
    Stderr,
}

impl MaiToolOutputStream {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

impl MaiToolOutputCapture {
    /// 准备 stdout/stderr 的捕获文件路径并创建其父目录。
    pub(super) async fn prepare(
        artifact_files_root: &Path,
        namespace: &str,
        call_id: &str,
        stdout_id: &str,
        stderr_id: &str,
        command: &str,
    ) -> Result<Self> {
        let stdout_name = tool_output_file_name(command, MaiToolOutputStream::Stdout);
        let stderr_name = tool_output_file_name(command, MaiToolOutputStream::Stderr);
        let stdout_path = tool_output_artifact_file_path(
            artifact_files_root,
            namespace,
            call_id,
            stdout_id,
            &stdout_name,
        );
        let stderr_path = tool_output_artifact_file_path(
            artifact_files_root,
            namespace,
            call_id,
            stderr_id,
            &stderr_name,
        );
        if let Some(parent) = stdout_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Some(parent) = stderr_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(Self {
            call_id: call_id.to_string(),
            stdout: MaiToolOutputStreamCapture {
                id: stdout_id.to_string(),
                name: stdout_name,
                stream: MaiToolOutputStream::Stdout,
                path: stdout_path,
            },
            stderr: MaiToolOutputStreamCapture {
                id: stderr_id.to_string(),
                name: stderr_name,
                stream: MaiToolOutputStream::Stderr,
                path: stderr_path,
            },
        })
    }

    pub(super) fn stdout_path(&self) -> &Path {
        &self.stdout.path
    }

    pub(super) fn stderr_path(&self) -> &Path {
        &self.stderr.path
    }

    /// 依据每个流的实际写入字节数生成 artifact 记录，并清理空流文件。
    pub(super) async fn collect_artifacts(
        &self,
        agent_id: AgentId,
        sizes: MaiToolOutputStreamSizes,
    ) -> Result<Vec<ToolOutputArtifactInfo>> {
        let created_at = now();
        let mut artifacts = Vec::new();
        push_or_remove_artifact(
            &mut artifacts,
            agent_id,
            &self.call_id,
            &self.stdout,
            sizes.stdout_bytes,
            created_at,
        )
        .await;
        push_or_remove_artifact(
            &mut artifacts,
            agent_id,
            &self.call_id,
            &self.stderr,
            sizes.stderr_bytes,
            created_at,
        )
        .await;
        Ok(artifacts)
    }
}

async fn push_or_remove_artifact(
    artifacts: &mut Vec<ToolOutputArtifactInfo>,
    agent_id: AgentId,
    call_id: &str,
    capture: &MaiToolOutputStreamCapture,
    size_bytes: u64,
    created_at: chrono::DateTime<chrono::Utc>,
) {
    if size_bytes > 0 {
        artifacts.push(ToolOutputArtifactInfo {
            id: capture.id.clone(),
            call_id: call_id.to_string(),
            agent_id,
            name: capture.name.clone(),
            stream: capture.stream.as_str().to_string(),
            size_bytes,
            created_at,
        });
    } else {
        let _ = tokio::fs::remove_file(&capture.path).await;
    }
}

/// 计算一个工具输出 artifact 的宿主文件路径。
///
/// 布局必须与宿主侧按 `(call_id, artifact_id, name)` 反查 artifact 的逻辑一致，
/// 因此这里是 Mai 唯一的 artifact 路径来源。
pub(crate) fn tool_output_artifact_file_path(
    artifact_files_root: &Path,
    namespace: &str,
    call_id: &str,
    artifact_id: &str,
    name: &str,
) -> std::path::PathBuf {
    let mut dir = artifact_files_root.join("tool-output");
    let namespace = safe_path_component(namespace);
    if !namespace.is_empty() {
        dir = dir.join(namespace);
    }
    dir.join(safe_path_component_or(call_id, "tool-call"))
        .join(safe_path_component_or(artifact_id, "artifact"))
        .join(safe_path_component_or(name, "output.txt"))
}

fn tool_output_file_name(command: &str, stream: MaiToolOutputStream) -> String {
    let command = command
        .split_whitespace()
        .next()
        .map(safe_path_component)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "command".to_string());
    format!("{command}-{}.txt", stream.as_str())
}

fn safe_path_component(raw: &str) -> String {
    let value = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    value.trim_matches('.').trim_matches('_').to_string()
}

fn safe_path_component_or(raw: &str, fallback: &str) -> String {
    let safe = safe_path_component(raw);
    if safe.is_empty() {
        fallback.to_string()
    } else {
        safe
    }
}

fn runtime_invalid_input(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}
