use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use mai_protocol::ToolOutputArtifactInfo;
use mai_references::AgentEnvironmentLease;
use pl_tool::command::{
    CommandBackend, CommandCaptureOwnership, CommandCaptureStream, CommandOutputSizes,
    CommandOutputTarget, CommandSpawnRequest, ManagedCommand, command_output_model_path,
};
use pl_tool::shell::shell_quote_word;
use uuid::Uuid;

use super::command_capture::CommandCaptureFiles;
use super::container::{
    MaiToolOutputCapture, MaiToolOutputStreamSizes, ensure_lease_active,
    tool_output_artifact_file_path,
};
use crate::{AgentRuntime, Result, RuntimeError};

/// Mai 每个命令的落盘捕获硬上限。
///
/// 上限约束的是**真实磁盘占用**，不是单一流的字节数：合并输出文件与 per-stream 捕获各写一份，
/// 因此 [`MaiCommandBackend::capture_storage_cost`] 按两份计费。达到上限即取消命令并等待其真实
/// 结束，已写字节保留、由 artifact 归档，按 typed 输出上限报告。
pub(crate) const MAI_COMMAND_CAPTURE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Mai 的容器工作区命令后端。
///
/// PL 统一管理进程表、stdin、超时和输出截断；该类型只负责把命令映射到
/// 本 Thread 冻结环境租约的容器，并把完整输出同步回容器 workspace 与 Mai artifact。
///
/// 装配租约之后**不再**按 agent 重新查容器：同一个 turn 内的 spawn、cwd 解析、输出发布与后台进程
/// 清理都用同一个 container_id，因此环境换代不会把旧调用漂移到新环境。
#[derive(Clone)]
pub(crate) struct MaiCommandBackend {
    runtime: Arc<AgentRuntime>,
    lease: Arc<AgentEnvironmentLease>,
    workspace_root: PathBuf,
    captures: Arc<Mutex<HashMap<PathBuf, MaiToolOutputCapture>>>,
    capture_files: Arc<CommandCaptureFiles>,
}

impl MaiCommandBackend {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        lease: Arc<AgentEnvironmentLease>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            runtime,
            lease,
            workspace_root: workspace_root.into(),
            captures: Arc::new(Mutex::new(HashMap::new())),
            capture_files: Arc::new(CommandCaptureFiles::default()),
        }
    }

    /// 本 generation 的容器 ID；租约已撤销时 fail closed，不重新按 agent 解析容器。
    fn container_id(&self) -> Result<String> {
        ensure_lease_active(&self.lease)?;
        Ok(self.lease.container_id().to_string())
    }

    fn workspace_path(&self, path: &Path) -> Result<PathBuf> {
        resolve_workspace_path(&self.workspace_root, path)
    }

    async fn canonical_workspace_path(
        &self,
        candidate: &Path,
        allow_workspace_escape: bool,
    ) -> Result<PathBuf> {
        let container_id = self.container_id()?;
        let candidate = candidate.to_str().ok_or_else(|| {
            RuntimeError::InvalidInput("exec cwd must be valid UTF-8".to_string())
        })?;
        let root = self.workspace_root.to_str().ok_or_else(|| {
            RuntimeError::InvalidInput("workspace root must be valid UTF-8".to_string())
        })?;
        let command = if allow_workspace_escape {
            format!(
                "resolved=$(readlink -f -- {candidate}) || exit 2; printf '%s' \"$resolved\"",
                candidate = shell_quote_word(candidate),
            )
        } else {
            format!(
                "resolved=$(readlink -f -- {candidate}) || exit 2; case \"$resolved\" in {root}|{root}/*) printf '%s' \"$resolved\" ;; *) exit 3 ;; esac",
                candidate = shell_quote_word(candidate),
                root = shell_quote_word(root),
            )
        };
        let output = self
            .runtime
            .deps
            .docker
            .exec_shell(&container_id, &command, Some("/"), Some(10))
            .await?;
        if output.status != 0 || output.stdout.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(format!(
                "exec cwd does not resolve to an existing directory inside {}",
                self.workspace_root.display()
            )));
        }
        Ok(PathBuf::from(output.stdout.trim()))
    }

    fn container_output_path(&self, model_file: &Path) -> Result<String> {
        let path = self.workspace_root.join(model_file);
        path.to_str().map(ToOwned::to_owned).ok_or_else(|| {
            RuntimeError::InvalidInput("output path must be valid UTF-8".to_string())
        })
    }
}

fn resolve_workspace_path(workspace_root: &Path, path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        && path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(RuntimeError::InvalidInput(format!(
            "exec cwd must stay inside {}",
            workspace_root.display()
        )));
    }

    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    };
    if !candidate.starts_with(workspace_root)
        || candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(RuntimeError::InvalidInput(format!(
            "exec cwd must stay inside {}",
            workspace_root.display()
        )));
    }
    Ok(candidate)
}

impl std::fmt::Debug for MaiCommandBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaiCommandBackend")
            .field("agent_id", &self.lease.agent_id())
            .field("container_id", &self.lease.container_id())
            .field("generation", &self.lease.generation())
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

impl CommandBackend for MaiCommandBackend {
    type Error = RuntimeError;

    /// Mai 显式声明的单命令落盘上限（比 PL 默认值更宽），由本后端自己负责如实计费。
    fn model_output_bytes_cap(&self) -> Option<u64> {
        Some(32 * 1024)
    }

    fn capture_limit_bytes(&self) -> Option<u64> {
        Some(MAI_COMMAND_CAPTURE_LIMIT_BYTES)
    }

    fn capture_ownership(&self) -> CommandCaptureOwnership {
        CommandCaptureOwnership::Manager
    }

    fn capture_storage_cost(
        &self,
        target: &CommandOutputTarget,
        stream: CommandCaptureStream,
        chunk: &[u8],
    ) -> u64 {
        CommandCaptureFiles::storage_cost(target, stream, chunk)
    }

    async fn resolve_cwd(
        &self,
        cwd: Option<&Path>,
        allow_workspace_escape: bool,
    ) -> Result<String> {
        let candidate = if allow_workspace_escape {
            cwd.map_or_else(
                || self.workspace_root.clone(),
                |path| {
                    if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        self.workspace_root.join(path)
                    }
                },
            )
        } else {
            cwd.map_or_else(
                || Ok(self.workspace_root.clone()),
                |path| self.workspace_path(path),
            )?
        };
        self.canonical_workspace_path(&candidate, allow_workspace_escape)
            .await?
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| RuntimeError::InvalidInput("exec cwd must be valid UTF-8".to_string()))
    }

    async fn output_target(
        &self,
        session_id: &str,
        tool_id: &str,
        call_id: &str,
        command: &str,
    ) -> Result<CommandOutputTarget> {
        let namespace = self.lease.agent_id().to_string();
        let stdout_id = Uuid::new_v4().to_string();
        let stderr_id = Uuid::new_v4().to_string();
        let capture = MaiToolOutputCapture::prepare(
            &self.runtime.artifact_files_root,
            &namespace,
            call_id,
            &stdout_id,
            &stderr_id,
            command,
        )
        .await?;
        let combined_id = Uuid::new_v4().to_string();
        let capture_file = tool_output_artifact_file_path(
            &self.runtime.artifact_files_root,
            &namespace,
            call_id,
            &combined_id,
            "output.log",
        );
        let model_file = command_output_model_path(session_id, tool_id);
        let target = CommandOutputTarget::new(capture_file.clone(), model_file)
            .with_stream_capture_files(capture.stdout_path(), capture.stderr_path());
        self.captures
            .lock()
            .map_err(lock_error)?
            .insert(capture_file, capture);
        Ok(target)
    }

    async fn spawn(&self, request: CommandSpawnRequest) -> Result<ManagedCommand> {
        super::command_process::spawn_generation_command(
            self.runtime.deps.docker.clone(),
            Arc::clone(&self.lease),
            request,
        )
        .await
    }

    async fn prepare_output(
        &self,
        target: &CommandOutputTarget,
        command: &str,
        working_directory: &str,
        capture_limit_bytes: u64,
    ) -> Result<u64> {
        self.capture_files
            .prepare_output(target, command, working_directory, capture_limit_bytes)
            .await
    }

    async fn append_output_chunk(
        &self,
        target: &CommandOutputTarget,
        stream: CommandCaptureStream,
        chunk: &[u8],
    ) -> Result<u64> {
        self.capture_files
            .append_output_chunk(target, stream, chunk)
            .await
    }

    async fn repair_output_chunk(
        &self,
        capture_file: &Path,
        stream: CommandCaptureStream,
        committed_len: u64,
        chunk: &[u8],
    ) -> Result<u64> {
        self.capture_files
            .repair_output_chunk(capture_file, stream, committed_len, chunk)
            .await
    }

    async fn publish_output(&self, target: &CommandOutputTarget) -> Result<()> {
        let container_id = self.container_id()?;
        let output_path = self.container_output_path(target.model_file())?;
        self.runtime
            .deps
            .docker
            .copy_to_container(&container_id, target.capture_file(), &output_path)
            .await?;
        Ok(())
    }

    async fn collect_output_artifacts(
        &self,
        target: &CommandOutputTarget,
        sizes: CommandOutputSizes,
    ) -> Result<Vec<serde_json::Value>> {
        let capture = self
            .captures
            .lock()
            .map_err(lock_error)?
            .remove(target.capture_file());
        let Some(capture) = capture else {
            return Ok(Vec::new());
        };
        let artifacts: Vec<ToolOutputArtifactInfo> = capture
            .collect_artifacts(
                self.lease.agent_id(),
                MaiToolOutputStreamSizes {
                    stdout_bytes: sizes.stdout_bytes,
                    stderr_bytes: sizes.stderr_bytes,
                },
            )
            .await?;
        artifacts
            .into_iter()
            .map(|artifact| serde_json::to_value(artifact).map_err(runtime_invalid_input))
            .collect()
    }
}

fn lock_error<T>(error: std::sync::PoisonError<T>) -> RuntimeError {
    RuntimeError::InvalidInput(format!("exec backend state lock poisoned: {error}"))
}

fn runtime_invalid_input(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_rejects_parent_escape_and_external_absolute_path() {
        let root = Path::new("/workspace/repo");
        assert!(resolve_workspace_path(root, Path::new("../secret")).is_err());
        assert!(resolve_workspace_path(root, Path::new("/etc")).is_err());
        assert_eq!(
            resolve_workspace_path(root, Path::new("src")).unwrap(),
            PathBuf::from("/workspace/repo/src")
        );
    }
}
