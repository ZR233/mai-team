use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use mai_protocol::ToolOutputArtifactInfo;
use mai_references::AgentEnvironmentLease;
use pl_tool::command::{
    CommandBackend, CommandCaptureStream, CommandOutputSizes, CommandOutputTarget,
    CommandSpawnRequest, ManagedCommand, command_output_model_path,
};
use pl_tool::shell::shell_quote_word;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

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

/// 每个 chunk 在合并文件里的 framing 折算（`=== STDOUT ===\n` 与补齐换行）。
const MAI_COMMAND_CAPTURE_FRAME_BYTES: u64 = 16;

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
    output_write_lock: Arc<tokio::sync::Mutex<()>>,
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
            output_write_lock: Arc::new(tokio::sync::Mutex::new(())),
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

    /// 同一个 chunk 会落两份：合并输出文件（带 framing）与它自己的 per-stream 捕获。
    ///
    /// 因此按 `chunk_len × 份数 + framing` 计费，让 PL 的共享额度约束**磁盘总量**，而不是只约束
    /// 其中一份；否则真实占用会是声明上限的两倍。
    fn capture_storage_cost(
        &self,
        target: &CommandOutputTarget,
        stream: CommandCaptureStream,
        chunk_len: usize,
    ) -> u64 {
        let stream_capture = match stream {
            CommandCaptureStream::Stdout => target.stdout_capture_file(),
            CommandCaptureStream::Stderr => target.stderr_capture_file(),
        };
        let copies = if stream_capture.is_some() { 2 } else { 1 };
        (chunk_len as u64)
            .saturating_mul(copies)
            .saturating_add(MAI_COMMAND_CAPTURE_FRAME_BYTES)
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
