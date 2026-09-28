use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use mai_protocol::{AgentId, ToolOutputArtifactInfo};
use pl_tool::command::{
    CommandBackend, CommandCaptureStream, CommandExit, CommandIo, CommandOutputSizes,
    CommandOutputTarget, CommandReader, CommandSpawnRequest, CommandWriter, ManagedCommand,
    command_output_model_path,
};
use pl_tool::shell::shell_quote_word;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use super::container::{
    MaiToolOutputCapture, MaiToolOutputStreamSizes, tool_output_artifact_file_path,
};
use crate::{AgentRuntime, Result, RuntimeError};

/// Mai 的容器工作区命令后端。
///
/// PL 统一管理进程表、stdin、超时和输出截断；该类型只负责把命令映射到
/// 当前 Agent 容器，并把完整输出同步回容器 workspace 与 Mai artifact。
#[derive(Clone)]
pub(crate) struct MaiCommandBackend {
    runtime: Arc<AgentRuntime>,
    agent_id: AgentId,
    workspace_root: PathBuf,
    captures: Arc<Mutex<HashMap<PathBuf, MaiToolOutputCapture>>>,
    output_write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl MaiCommandBackend {
    pub(crate) fn new(
        runtime: Arc<AgentRuntime>,
        agent_id: AgentId,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            runtime,
            agent_id,
            workspace_root: workspace_root.into(),
            captures: Arc::new(Mutex::new(HashMap::new())),
            output_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn current_container_id(&self) -> Result<String> {
        self.runtime.container_id(self.agent_id).await
    }

    fn workspace_path(&self, path: &Path) -> Result<PathBuf> {
        resolve_workspace_path(&self.workspace_root, path)
    }

    async fn canonical_workspace_path(
        &self,
        candidate: &Path,
        allow_workspace_escape: bool,
    ) -> Result<PathBuf> {
        let container_id = self.current_container_id().await?;
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
            .field("agent_id", &self.agent_id)
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

impl CommandBackend for MaiCommandBackend {
    type Error = RuntimeError;

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
        let namespace = self.agent_id.to_string();
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
        let container_id = self.current_container_id().await?;
        let mut child = self
            .runtime
            .deps
            .docker
            .spawn_managed_exec(
                &container_id,
                &request.process_id,
                &request.command,
                Some(&request.cwd),
            )
            .map_err(RuntimeError::from)?;
        let host_pid = child.id();
        let io = CommandIo {
            stdin: child
                .stdin
                .take()
                .map(|value| Box::pin(value) as CommandWriter),
            stdout: child
                .stdout
                .take()
                .map(|value| Box::pin(value) as CommandReader),
            stderr: child
                .stderr
                .take()
                .map(|value| Box::pin(value) as CommandReader),
        };
        let runtime = self.runtime.clone();
        let process_id = request.process_id.clone();
        Ok(ManagedCommand::new(
            host_pid,
            io,
            move |cancellation| async move {
                let result = tokio::select! {
                    result = child.wait() => result,
                    _ = cancellation.cancelled() => {
                        runtime
                            .deps
                            .docker
                            .terminate_managed_exec(&container_id, &process_id, host_pid)
                            .await;
                        child.wait().await
                    }
                };
                result
                    .map(|status| CommandExit {
                        exit_code: status.code(),
                    })
                    .map_err(|error| format!("failed to wait for container command: {error}"))
            },
        ))
    }

    async fn prepare_output(
        &self,
        target: &CommandOutputTarget,
        command: &str,
        working_directory: &str,
    ) -> Result<u64> {
        let _guard = self.output_write_lock.lock().await;
        if let Some(parent) = target.capture_file().parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let header = format!("=== COMMAND ===\n{command}\n\n=== CWD ===\n{working_directory}\n\n");
        tokio::fs::write(target.capture_file(), header.as_bytes()).await?;
        Ok(header.len() as u64)
    }

    async fn append_output_chunk(
        &self,
        target: &CommandOutputTarget,
        stream: CommandCaptureStream,
        chunk: &[u8],
    ) -> Result<u64> {
        let _guard = self.output_write_lock.lock().await;
        let mut combined = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(target.capture_file())
            .await?;
        let (label, stream_file) = match stream {
            CommandCaptureStream::Stdout => ("STDOUT", target.stdout_capture_file()),
            CommandCaptureStream::Stderr => ("STDERR", target.stderr_capture_file()),
        };
        combined
            .write_all(format!("=== {label} ===\n").as_bytes())
            .await?;
        combined.write_all(chunk).await?;
        if !chunk.ends_with(b"\n") {
            combined.write_all(b"\n").await?;
        }
        if let Some(stream_file) = stream_file {
            let mut capture = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(stream_file)
                .await?;
            capture.write_all(chunk).await?;
            capture.flush().await?;
        }
        combined.flush().await?;
        Ok(combined.metadata().await?.len())
    }

    async fn repair_output_chunk(
        &self,
        capture_file: &Path,
        stream: CommandCaptureStream,
        committed_len: u64,
        chunk: &[u8],
    ) -> Result<u64> {
        // 先把可能写坏的部分截断回上次确认的偏移，再按正常 append 的 frame 重放一次，
        // 因此重复 repair 得到相同字节而不是重复追加，命令本身不会被重跑。
        // 这里只修复模型可见的合并输出片段；artifact 的 per-stream 文件仍只由成功 append 写入。
        let _guard = self.output_write_lock.lock().await;
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(capture_file)
            .await?;
        file.set_len(committed_len).await?;
        drop(file);
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(capture_file)
            .await?;
        let label = match stream {
            CommandCaptureStream::Stdout => "STDOUT",
            CommandCaptureStream::Stderr => "STDERR",
        };
        file.write_all(format!("=== {label} ===\n").as_bytes())
            .await?;
        file.write_all(chunk).await?;
        if !chunk.ends_with(b"\n") {
            file.write_all(b"\n").await?;
        }
        file.flush().await?;
        Ok(file.metadata().await?.len())
    }

    async fn publish_output(&self, target: &CommandOutputTarget) -> Result<()> {
        let container_id = self.current_container_id().await?;
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
                self.agent_id,
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
