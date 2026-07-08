use std::path::Path;
use std::sync::Arc;

use mai_docker::ExecCaptureOptions;
use mai_protocol::{AgentId, ToolOutputArtifactInfo, now};
use pl_core::{
    ContainerBackend, ContainerCopyFromRequest, ContainerCopyToRequest, ContainerExecOutput,
    ContainerExecRequest, ToolOutputArtifactDescriptor, ToolOutputCapture,
    ToolOutputCaptureRequest, ToolOutputStreamSizes,
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
    let cancellation_token = request
        .cancellation_token
        .unwrap_or_else(tokio_util::sync::CancellationToken::new);
    if let Some(output_bytes_cap) = request.output_bytes_cap {
        let call_id = Uuid::new_v4().to_string();
        let stdout_id = Uuid::new_v4().to_string();
        let stderr_id = Uuid::new_v4().to_string();
        let namespace = agent_id.to_string();
        let capture = ToolOutputCapture::prepare(
            ToolOutputCaptureRequest::new(
                artifact_files_root,
                &call_id,
                &stdout_id,
                &stderr_id,
                &request.command,
            )
            .with_namespace(&namespace),
        )
        .await
        .map_err(runtime_invalid_input)?;
        let output = docker
            .exec_shell_captured_with_cancel(
                &container_id,
                &request.command,
                request.cwd.as_deref(),
                request.timeout_secs,
                ExecCaptureOptions {
                    stdout_path: &capture.stdout.path,
                    stderr_path: &capture.stderr.path,
                    output_bytes_cap,
                },
                &cancellation_token,
            )
            .await?;
        let artifacts = capture
            .collect_artifacts(ToolOutputStreamSizes::new(
                output.stdout_bytes,
                output.stderr_bytes,
            ))
            .await
            .map_err(runtime_invalid_input)?;
        let artifacts = artifact_records_from_descriptors(agent_id, artifacts);
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

fn artifact_records_from_descriptors(
    agent_id: AgentId,
    descriptors: Vec<ToolOutputArtifactDescriptor>,
) -> Vec<ToolOutputArtifactInfo> {
    let created_at = now();
    descriptors
        .into_iter()
        .map(|descriptor| ToolOutputArtifactInfo {
            id: descriptor.id,
            call_id: descriptor.call_id,
            agent_id,
            name: descriptor.name,
            stream: descriptor.stream.as_str().to_string(),
            size_bytes: descriptor.size_bytes,
            created_at,
        })
        .collect()
}

fn runtime_invalid_input(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn container_backend_delegates_tool_error_shape_to_pl_core() {
        let source = include_str!("container.rs");

        assert!(
            !source.contains(&format!("{}{}", "ToolExecution", "Failed")),
            "container backend adapter 不应手动构造 pl-core 工具错误"
        );
        assert!(
            !source.contains(&format!("{}{}", "Pure", "Error")),
            "container backend adapter 不应依赖 pl 协议错误类型"
        );
    }
}
