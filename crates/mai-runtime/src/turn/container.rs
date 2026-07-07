use std::future::Future;
use std::path::Path;

use mai_docker::ExecCaptureOptions;
use mai_protocol::{AgentId, ToolOutputArtifactInfo};
use pl_core::{
    ContainerBackend, ContainerCopyFromRequest, ContainerCopyToRequest, ContainerExecOutput,
    ContainerExecRequest,
};
use pl_protocol::PureError;
use serde_json::Value;

use crate::turn::tools::{
    ToolExecution, prepare_tool_output_capture, tool_output_artifacts_from_capture,
};
use crate::{Result, RuntimeError};

pub(crate) struct ContainerToolContext<'a, O: ContainerToolOps + ?Sized> {
    pub(crate) docker: &'a mai_docker::DockerClient,
    pub(crate) artifact_files_root: &'a Path,
    pub(crate) ops: &'a O,
}

struct MaiContainerBackend<'a, O: ContainerToolOps + ?Sized> {
    context: &'a ContainerToolContext<'a, O>,
    agent_id: AgentId,
}

impl<O: ContainerToolOps + ?Sized> std::fmt::Debug for MaiContainerBackend<'_, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaiContainerBackend")
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

impl<O: ContainerToolOps + ?Sized> ContainerBackend for MaiContainerBackend<'_, O> {
    async fn exec(&self, request: ContainerExecRequest) -> pl_core::Result<ContainerExecOutput> {
        let container_id = self
            .context
            .ops
            .container_id(self.agent_id)
            .await
            .map_err(container_backend_error)?;
        let cancellation_token = request
            .cancellation_token
            .unwrap_or_else(tokio_util::sync::CancellationToken::new);
        if let Some(output_bytes_cap) = request.output_bytes_cap {
            let capture = prepare_tool_output_capture(
                self.context.artifact_files_root,
                self.agent_id,
                &request.command,
            )
            .await
            .map_err(container_backend_error)?;
            let output = self
                .context
                .docker
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
                self.agent_id,
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

        let output = self
            .context
            .docker
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

    async fn copy_from(&self, request: ContainerCopyFromRequest) -> pl_core::Result<Vec<u8>> {
        let container_id = self
            .context
            .ops
            .container_id(self.agent_id)
            .await
            .map_err(container_backend_error)?;
        if request.archive {
            return self
                .context
                .docker
                .copy_from_container_tar(&container_id, &request.path)
                .await
                .map_err(container_backend_error);
        }
        let dir = tempfile::tempdir().map_err(container_backend_error)?;
        let host_path = dir.path().join("file");
        self.context
            .docker
            .copy_from_container_to_file(&container_id, &request.path, &host_path)
            .await
            .map_err(container_backend_error)?;
        tokio::fs::read(&host_path)
            .await
            .map_err(container_backend_error)
    }

    async fn copy_to(&self, request: ContainerCopyToRequest) -> pl_core::Result<()> {
        let container_id = self
            .context
            .ops
            .container_id(self.agent_id)
            .await
            .map_err(container_backend_error)?;
        let temp = tempfile::NamedTempFile::new().map_err(container_backend_error)?;
        std::fs::write(temp.path(), &request.content).map_err(container_backend_error)?;
        self.context
            .docker
            .copy_to_container(&container_id, temp.path(), &request.path)
            .await
            .map_err(container_backend_error)
    }
}

fn container_backend_error(error: impl std::fmt::Display) -> PureError {
    PureError::ToolExecutionFailed {
        tool: "container".to_string(),
        error: error.to_string(),
    }
}

/// 容器类工具依赖的最小运行时能力。
///
/// 实现方只负责把 agent 解析为可执行命令的容器 ID。返回的 future 必须可
/// 线程间移动，且错误应保持为 runtime 层的 `Result` 以便工具生命周期统一记录。
pub(crate) trait ContainerToolOps: Send + Sync {
    fn container_id(&self, agent_id: AgentId) -> impl Future<Output = Result<String>> + Send;
}

pub(crate) async fn execute_container_tool(
    context: &ContainerToolContext<'_, impl ContainerToolOps + ?Sized>,
    agent_id: AgentId,
    name: &str,
    arguments: &Value,
    cancellation_token: &tokio_util::sync::CancellationToken,
) -> Result<Option<ToolExecution>> {
    if !matches!(
        mai_tools::route_tool(name),
        mai_tools::RoutedTool::ContainerExec
            | mai_tools::RoutedTool::ReadFile
            | mai_tools::RoutedTool::ListFiles
            | mai_tools::RoutedTool::SearchFiles
            | mai_tools::RoutedTool::ApplyPatch
            | mai_tools::RoutedTool::ContainerCpUpload
            | mai_tools::RoutedTool::ContainerCpDownload
    ) {
        return Ok(None);
    }
    let backend = MaiContainerBackend { context, agent_id };
    let Some(execution) = pl_core::execute_container_tool(
        &backend,
        name,
        arguments.clone(),
        Some(cancellation_token.clone()),
    )
    .await?
    else {
        return Ok(None);
    };
    let output_artifacts = output_artifacts_from_json(execution.output_artifacts)?;
    Ok(Some(ToolExecution::with_model_output(
        execution.success,
        execution.output,
        execution.model_output,
        false,
        output_artifacts,
    )))
}

fn output_artifacts_from_json(values: Vec<Value>) -> Result<Vec<ToolOutputArtifactInfo>> {
    values
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid tool artifact: {err}")))
}
