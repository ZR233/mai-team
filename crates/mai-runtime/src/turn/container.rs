use std::path::Path;
use std::sync::Arc;

use mai_docker::ExecCaptureOptions;
use mai_protocol::{AgentId, ToolOutputArtifactInfo, now};
use pl_tool::container::{
    ContainerBackend, ContainerCopyFromRequest, ContainerCopyToRequest, ContainerExecOutput,
    ContainerExecRequest,
};
use uuid::Uuid;

use crate::{AgentRuntime, Result, RuntimeError};

#[derive(Clone)]
pub(crate) struct MaiContainerBackend {
    runtime: Arc<AgentRuntime>,
    agent_id: AgentId,
}

impl MaiContainerBackend {
    pub(crate) fn new(runtime: Arc<AgentRuntime>, agent_id: AgentId) -> Self {
        Self { runtime, agent_id }
    }
}

impl std::fmt::Debug for MaiContainerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaiContainerBackend")
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

impl ContainerBackend for MaiContainerBackend {
    type Error = RuntimeError;

    async fn exec(&self, request: ContainerExecRequest) -> Result<ContainerExecOutput> {
        execute_with_container_backend(
            &self.runtime.deps.docker,
            &self.runtime.artifact_files_root,
            self.runtime.container_id(self.agent_id).await,
            self.agent_id,
            request,
        )
        .await
    }

    async fn copy_from(&self, request: ContainerCopyFromRequest) -> Result<Vec<u8>> {
        copy_from_container_backend(
            &self.runtime.deps.docker,
            self.runtime.container_id(self.agent_id).await,
            request,
        )
        .await
    }

    async fn copy_to(&self, request: ContainerCopyToRequest) -> Result<()> {
        copy_to_container_backend(
            &self.runtime.deps.docker,
            self.runtime.container_id(self.agent_id).await,
            request,
        )
        .await
    }
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
    container_id: Result<String>,
    request: ContainerCopyToRequest,
) -> Result<()> {
    let container_id = container_id?;
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
