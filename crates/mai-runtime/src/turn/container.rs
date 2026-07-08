use std::path::Path;
use std::sync::Arc;

use mai_docker::ExecCaptureOptions;
use mai_protocol::AgentId;
use pl_core::{
    ContainerBackend, ContainerCopyFromRequest, ContainerCopyToRequest, ContainerExecOutput,
    ContainerExecRequest,
};
use pl_protocol::PureError;

use crate::turn::tool_output::{prepare_tool_output_capture, tool_output_artifacts_from_capture};
use crate::{AgentRuntime, Result};

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
    async fn exec(&self, request: ContainerExecRequest) -> pl_core::Result<ContainerExecOutput> {
        execute_with_container_backend(
            &self.runtime.deps.docker,
            &self.runtime.artifact_files_root,
            self.runtime.container_id(self.agent_id).await,
            self.agent_id,
            request,
        )
        .await
    }

    async fn copy_from(&self, request: ContainerCopyFromRequest) -> pl_core::Result<Vec<u8>> {
        copy_from_container_backend(
            &self.runtime.deps.docker,
            self.runtime.container_id(self.agent_id).await,
            request,
        )
        .await
    }

    async fn copy_to(&self, request: ContainerCopyToRequest) -> pl_core::Result<()> {
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
) -> pl_core::Result<ContainerExecOutput> {
    let container_id = container_id.map_err(container_backend_error)?;
    let cancellation_token = request
        .cancellation_token
        .unwrap_or_else(tokio_util::sync::CancellationToken::new);
    if let Some(output_bytes_cap) = request.output_bytes_cap {
        let capture = prepare_tool_output_capture(artifact_files_root, agent_id, &request.command)
            .await
            .map_err(container_backend_error)?;
        let output = docker
            .exec_shell_captured_with_cancel(
                &container_id,
                &request.command,
                request.cwd.as_deref(),
                request.timeout_secs,
                ExecCaptureOptions {
                    stdout_path: &capture.stdout_path,
                    stderr_path: &capture.stderr_path,
                    output_bytes_cap,
                },
                &cancellation_token,
            )
            .await
            .map_err(container_backend_error)?;
        let artifacts = tool_output_artifacts_from_capture(
            agent_id,
            &capture,
            output.stdout_bytes,
            output.stderr_bytes,
        )
        .await
        .map_err(container_backend_error)?;
        let output_artifacts = artifacts
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(container_backend_error)?;
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
        .await
        .map_err(container_backend_error)?;
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
) -> pl_core::Result<Vec<u8>> {
    let container_id = container_id.map_err(container_backend_error)?;
    if request.archive {
        return docker
            .copy_from_container_tar(&container_id, &request.path)
            .await
            .map_err(container_backend_error);
    }
    let dir = tempfile::tempdir().map_err(container_backend_error)?;
    let host_path = dir.path().join("file");
    docker
        .copy_from_container_to_file(&container_id, &request.path, &host_path)
        .await
        .map_err(container_backend_error)?;
    tokio::fs::read(&host_path)
        .await
        .map_err(container_backend_error)
}

async fn copy_to_container_backend(
    docker: &mai_docker::DockerClient,
    container_id: Result<String>,
    request: ContainerCopyToRequest,
) -> pl_core::Result<()> {
    let container_id = container_id.map_err(container_backend_error)?;
    let temp = tempfile::NamedTempFile::new().map_err(container_backend_error)?;
    std::fs::write(temp.path(), &request.content).map_err(container_backend_error)?;
    docker
        .copy_to_container(&container_id, temp.path(), &request.path)
        .await
        .map_err(container_backend_error)
}

fn container_backend_error(error: impl std::fmt::Display) -> PureError {
    PureError::ToolExecutionFailed {
        tool: "container".to_string(),
        error: error.to_string(),
    }
}
