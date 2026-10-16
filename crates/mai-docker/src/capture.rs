//! docker exec 输出的宿主侧捕获：有界的内存观察视图 + 有界的落盘保存。
//!
//! 一条命令的 stdout/stderr 共享同一个落盘额度。chunk 在**写盘之前**计费，
//! 超出上限的字节根本不会落盘，因此真实磁盘占用不会超过声明的上限；被拒绝的字节
//! 只累计到 `refused_bytes`，不会被当成已保存的输出。
//!
//! 容量终止（[`ExecCaptureFailure::OutputLimitExceeded`]）与可靠保存故障
//! （[`ExecCaptureFailure::Write`]／[`ExecCaptureFailure::Read`]）是类型不同的值，
//! 调用方不需要解析诊断文本就能区分"按上限被有意停下"和"已受理输出没能保存"。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use crate::error::{DockerError, Result};
use crate::exec::{
    CapturedExecOutput, ExecOutput, MANAGED_EXEC_LAUNCHER, managed_exec_kill_command,
};

/// 单条 `docker exec` 命令在宿主侧落盘捕获的硬上限（字节）。
///
/// stdout 与 stderr 共享这一个额度，语义与 PL 命令后端的单命令捕获上限一致：
/// 计费发生在落盘**之前**，被拒绝的字节不会写进任何文件。
pub const MAX_EXEC_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;

/// 每个输出流真正落盘的份数。
///
/// mai 的 docker exec 捕获当前每个流只写一份文件（`stdout_path` / `stderr_path`），
/// 合并视图只存在于内存里的有界 [`HeadTailBuffer`]，不落盘，因此不额外计费。
///
/// **若将来把合并输出（例如 `output.log`）也落盘，必须把这个份数改成真实份数**，
/// 否则共享额度只约束了其中一份，同一份输出的真实磁盘占用会是上限的两倍。
pub(crate) const CAPTURE_FILE_COPIES_PER_STREAM: u64 = 1;

/// 把一个已读 chunk 落盘后的真实存储成本（字节）。
pub(crate) fn capture_storage_cost(chunk_len: usize) -> u64 {
    (chunk_len as u64).saturating_mul(CAPTURE_FILE_COPIES_PER_STREAM)
}

/// 输出流标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecCaptureStream {
    Stdout,
    Stderr,
}

impl ExecCaptureStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

impl std::fmt::Display for ExecCaptureStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 容器内 PID 台账进程组的终止结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerTerminationOutcome {
    /// 终止命令成功下发。
    Issued,
    /// 终止命令失败或超时：容器内的命令可能仍在运行。
    Failed,
    /// 没有下发：宿主 `docker exec` 在终止前已经自行退出。
    NotNeeded,
}

/// 超限终止的执行事实，用于说明"命令是真的被停下了"而不是只丢下观察者。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecTerminationOutcome {
    /// 宿主 `docker exec` 子进程是否已确认退出。
    pub docker_exec_exited: bool,
    /// 容器内 PID 台账进程组的终止结果。
    pub container_termination: ContainerTerminationOutcome,
}

/// 容量终止的载荷：上限、已受理成本、被拒绝成本，以及真实终止事实。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputLimitExceeded {
    pub limit_bytes: u64,
    pub charged_bytes: u64,
    pub refused_bytes: u64,
    pub termination: ExecTerminationOutcome,
}

/// 一条命令的宿主侧捕获为什么提前结束。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecCaptureFailure {
    /// 输出触及本命令声明的落盘上限：命令被终止，已受理并写盘的字节保留。
    OutputLimitExceeded(OutputLimitExceeded),
    /// 捕获文件写入/刷盘失败。可靠保存故障，与容量终止不同类。
    Write {
        stream: ExecCaptureStream,
        message: String,
    },
    /// 输出流读取失败：同样是"已受理输出无法继续保存"的可靠保存故障。
    Read {
        stream: ExecCaptureStream,
        message: String,
    },
}

impl ExecCaptureFailure {
    /// 出现在命令结果里的可读原因。
    pub fn message(&self) -> String {
        match self {
            Self::OutputLimitExceeded(limit) => format!(
                "command output exceeded the {}-byte per-command capture limit: {} bytes of capture were stored, {} bytes of further output were refused, docker exec exited: {}, container process group terminated: {:?}",
                limit.limit_bytes,
                limit.charged_bytes,
                limit.refused_bytes,
                limit.termination.docker_exec_exited,
                limit.termination.container_termination,
            ),
            Self::Write { stream, message } => {
                format!("{stream} output capture failed: {message}")
            }
            Self::Read { stream, message } => {
                format!("{stream} output stream could not be read: {message}")
            }
        }
    }

    /// 容量终止的载荷；可靠保存故障返回 `None`。
    pub fn output_limit_exceeded(&self) -> Option<OutputLimitExceeded> {
        match self {
            Self::OutputLimitExceeded(limit) => Some(*limit),
            Self::Write { .. } | Self::Read { .. } => None,
        }
    }

    /// 是否为可靠保存故障（调用方需要按存储故障处理，而不是按容量终止处理）。
    pub fn is_storage_fault(&self) -> bool {
        match self {
            Self::OutputLimitExceeded(_) => false,
            Self::Write { .. } | Self::Read { .. } => true,
        }
    }

    /// 补齐真实终止事实；只有确认过宿主子进程状态之后才允许改写。
    pub(crate) fn with_termination(self, termination: ExecTerminationOutcome) -> Self {
        match self {
            Self::OutputLimitExceeded(limit) => Self::OutputLimitExceeded(OutputLimitExceeded {
                termination,
                ..limit
            }),
            other => other,
        }
    }
}

/// 一个 chunk 的接纳决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureAcceptance {
    /// 已计费，可以写盘。
    Accepted,
    /// 本命令的落盘额度已满：该 chunk 不写盘。
    OutputLimitExceeded {
        limit_bytes: u64,
        charged_bytes: u64,
        refused_bytes: u64,
    },
    /// 已经有过首个故障：该 chunk 不写盘。
    Faulted,
}

/// 一条命令的共享捕获会话：stdout/stderr 共用一个落盘额度与"首个故障"。
///
/// 计费与故障记录都是先到先得：一旦有了故障，后面的 chunk 一律拒绝进计划，
/// 因此已受理（可能已写盘）的字节始终是有界的，且不会出现两个互相矛盾的失败原因。
#[derive(Debug, Clone)]
pub(crate) struct CaptureSession {
    limit_bytes: u64,
    charged_bytes: Arc<AtomicU64>,
    refused_bytes: Arc<AtomicU64>,
    fault: Arc<Mutex<Option<ExecCaptureFailure>>>,
    capture_stopped: CancellationToken,
}

impl CaptureSession {
    pub(crate) fn new(limit_bytes: u64) -> Self {
        Self {
            limit_bytes,
            charged_bytes: Arc::new(AtomicU64::new(0)),
            refused_bytes: Arc::new(AtomicU64::new(0)),
            fault: Arc::new(Mutex::new(None)),
            capture_stopped: CancellationToken::new(),
        }
    }

    /// 本命令生效的落盘上限。
    #[cfg(test)]
    pub(crate) fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    /// 已受理（已占用额度）的存储成本。
    #[cfg(test)]
    pub(crate) fn charged_bytes(&self) -> u64 {
        self.charged_bytes.load(Ordering::Acquire)
    }

    /// 被拒绝、从未进入写盘路径的存储成本。
    #[cfg(test)]
    pub(crate) fn refused_bytes(&self) -> u64 {
        self.refused_bytes.load(Ordering::Acquire)
    }

    /// 接纳前计费：只有额度足够的 chunk 才允许写盘。
    pub(crate) fn charge(&self, cost: u64) -> CaptureAcceptance {
        if self.fault().is_some() {
            self.refused_bytes.fetch_add(cost, Ordering::AcqRel);
            return CaptureAcceptance::Faulted;
        }
        let mut charged = self.charged_bytes.load(Ordering::Acquire);
        loop {
            let next = charged.saturating_add(cost);
            if next > self.limit_bytes {
                let refused = self.refused_bytes.fetch_add(cost, Ordering::AcqRel) + cost;
                // 首个被拒的 chunk 就唤醒调用方：命令必须被真实终止，而不是继续产出。
                self.capture_stopped.cancel();
                return CaptureAcceptance::OutputLimitExceeded {
                    limit_bytes: self.limit_bytes,
                    charged_bytes: charged,
                    refused_bytes: refused,
                };
            }
            match self.charged_bytes.compare_exchange_weak(
                charged,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return CaptureAcceptance::Accepted,
                Err(actual) => charged = actual,
            }
        }
    }

    /// 记录首个故障；后续 chunk 一律拒绝。返回是否为本次调用记录的故障。
    pub(crate) fn record_failure(&self, failure: ExecCaptureFailure) -> bool {
        let mut guard = self.fault.lock().unwrap_or_else(PoisonError::into_inner);
        let recorded = guard.is_none();
        if recorded {
            *guard = Some(failure);
        }
        drop(guard);
        self.capture_stopped.cancel();
        recorded
    }

    pub(crate) fn fault(&self) -> Option<ExecCaptureFailure> {
        self.fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 首个故障（容量或保存）触发；调用方用它唤醒终止路径。
    pub(crate) fn capture_stopped(&self) -> CancellationToken {
        self.capture_stopped.clone()
    }
}

/// 一个流已写盘的捕获结果。
#[derive(Debug)]
pub(crate) struct StreamCapture {
    /// 有界的内存观察视图（可能被截断，与容量终止无关）。
    pub(crate) text: String,
    /// 真实写入捕获文件的字节数；artifact 大小必须用它。
    pub(crate) total_bytes: u64,
    /// 观察视图是否被截断。这是观察事实，不代表命令被取消。
    pub(crate) truncated: bool,
}

/// 读取一个输出流：先计费、再写盘，超限或故障时立即停止读盘。
pub(crate) fn capture_stream<R>(
    reader: R,
    path: PathBuf,
    output_bytes_cap: usize,
    stream: ExecCaptureStream,
    session: CaptureSession,
) -> JoinHandle<StreamCapture>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut view = HeadTailBuffer::new(output_bytes_cap);
        let mut total_bytes = 0_u64;

        let mut file = match open_capture_file(&path).await {
            Ok(file) => file,
            Err(error) => {
                session.record_failure(ExecCaptureFailure::Write {
                    stream,
                    message: error.to_string(),
                });
                return StreamCapture {
                    text: String::new(),
                    total_bytes,
                    truncated: false,
                };
            }
        };

        let mut chunk = [0_u8; 8192];
        loop {
            let read = match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    session.record_failure(ExecCaptureFailure::Read {
                        stream,
                        message: error.to_string(),
                    });
                    break;
                }
            };
            let bytes = &chunk[..read];

            // 接纳前计费：被拒绝的字节不写盘，真实磁盘占用因此不会超过共享额度。
            match session.charge(capture_storage_cost(read)) {
                CaptureAcceptance::Accepted => {}
                CaptureAcceptance::OutputLimitExceeded {
                    limit_bytes,
                    charged_bytes,
                    refused_bytes,
                } => {
                    session.record_failure(ExecCaptureFailure::OutputLimitExceeded(
                        OutputLimitExceeded {
                            limit_bytes,
                            charged_bytes,
                            refused_bytes,
                            termination: ExecTerminationOutcome {
                                docker_exec_exited: false,
                                container_termination: ContainerTerminationOutcome::NotNeeded,
                            },
                        },
                    ));
                    break;
                }
                CaptureAcceptance::Faulted => break,
            }

            if let Err(error) = file.write_all(bytes).await {
                session.record_failure(ExecCaptureFailure::Write {
                    stream,
                    message: error.to_string(),
                });
                break;
            }
            view.push(bytes);
            total_bytes = total_bytes.saturating_add(read as u64);
        }

        if let Err(error) = file.flush().await {
            session.record_failure(ExecCaptureFailure::Write {
                stream,
                message: error.to_string(),
            });
        }

        let truncated = view.truncated();
        StreamCapture {
            text: String::from_utf8_lossy(&view.into_bytes()).into_owned(),
            total_bytes,
            truncated,
        }
    })
}

async fn open_capture_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::File::create(path).await
}

/// 终止容器内进程组时等待那一次 `docker exec` 返回的最长时间。
const CONTAINER_PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);
/// 宿主 `docker exec` 收到 TERM 之后等待它自行退出的宽限时间。
const CAPTURED_EXEC_TERM_GRACE: Duration = Duration::from_secs(2);
/// 强杀宿主进程组之后等待子进程退出的最长时间。
const CAPTURED_EXEC_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// 捕获路径终止之后的真实状态。
#[derive(Debug)]
pub(crate) struct CapturedExecTermination {
    pub(crate) outcome: ExecTerminationOutcome,
    pub(crate) status: Option<ExitStatus>,
}

/// 捕获路径专用的进程标识：调用方不需要提供，但必须全局唯一。
pub(crate) fn captured_exec_process_id() -> String {
    format!("capture-{}", uuid::Uuid::new_v4())
}

/// 捕获路径的容器内 argv。
///
/// 与 managed exec 使用同一个启动器与 PID 台账，内层 shell 保持 `-lc`，
/// 因此命令文本、登录 shell 语义与退出码契约都不变。
pub(crate) fn captured_exec_shell_args(pid_file: &str, command: &str) -> Vec<String> {
    vec![
        "-c".to_string(),
        MANAGED_EXEC_LAUNCHER.to_string(),
        "mai-managed-exec".to_string(),
        pid_file.to_string(),
        "/bin/sh".to_string(),
        "-lc".to_string(),
        command.to_string(),
    ]
}

/// 组装捕获路径的结果。
///
/// 可靠保存故障（写／读失败）走 `Err`，容量终止作为 typed 值随结果返回：
/// 调用方不需要解析文本就能区分"按上限被有意停下"与"已受理输出没能保存"。
pub(crate) fn captured_exec_result(
    status: ExitStatus,
    stdout_capture: StreamCapture,
    stderr_capture: StreamCapture,
    failure: Option<ExecCaptureFailure>,
) -> Result<CapturedExecOutput> {
    let output = ExecOutput {
        status: status.code().unwrap_or(-1),
        stdout: stdout_capture.text,
        stderr: stderr_capture.text,
    };
    let result = CapturedExecOutput {
        output,
        stdout_bytes: stdout_capture.total_bytes,
        stderr_bytes: stderr_capture.total_bytes,
        stdout_truncated: stdout_capture.truncated,
        stderr_truncated: stderr_capture.truncated,
        capture_failure: failure,
    };
    // 先取出保存故障的文本，借用结束后再决定返回 Ok 还是 Err。
    let storage_fault = result
        .capture_failure
        .as_ref()
        .filter(|failure| failure.is_storage_fault())
        .map(|failure| failure.message());
    match storage_fault {
        Some(message) => Err(DockerError::CommandFailed(message)),
        None => Ok(result),
    }
}

/// 真实终止一条 docker exec：容器内进程组与宿主进程组都停，并等待子进程退出。
pub(crate) async fn terminate_captured_exec(
    docker_binary: &str,
    container_id: &str,
    pid_file: &str,
    host_pid: Option<u32>,
    child: &mut Child,
) -> CapturedExecTermination {
    let (container_termination, (docker_exec_exited, status)) = tokio::join!(
        terminate_container_process_group(docker_binary, container_id, pid_file),
        terminate_exec_process_group(host_pid, child),
    );
    CapturedExecTermination {
        outcome: ExecTerminationOutcome {
            docker_exec_exited,
            container_termination,
        },
        status,
    }
}

/// 用一次独立的 docker exec 终止容器内 PID 台账记录的进程组。
///
/// 命令已经自行退出时台账文件可能不存在；终止脚本对这种情形是幂等的。
async fn terminate_container_process_group(
    docker_binary: &str,
    container_id: &str,
    pid_file: &str,
) -> ContainerTerminationOutcome {
    let command = managed_exec_kill_command(pid_file);
    let mut kill = Command::new(docker_binary);
    kill.arg("exec")
        .arg(container_id)
        .arg("/bin/sh")
        .args(["-c", command.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match timeout(CONTAINER_PROCESS_TERMINATION_TIMEOUT, kill.status()).await {
        Ok(Ok(status)) if status.success() => ContainerTerminationOutcome::Issued,
        _ => ContainerTerminationOutcome::Failed,
    }
}

/// 终止宿主 docker exec 进程组并等待子进程退出，返回是否确认退出与退出状态。
async fn terminate_exec_process_group(
    host_pid: Option<u32>,
    child: &mut Child,
) -> (bool, Option<ExitStatus>) {
    signal_process_group(host_pid, "TERM").await;
    let waited = tokio::select! {
        result = child.wait() => Some(result),
        _ = tokio::time::sleep(CAPTURED_EXEC_TERM_GRACE) => None,
    };
    if let Some(result) = waited {
        return (result.is_ok(), result.ok());
    }
    signal_process_group(host_pid, "KILL").await;
    match timeout(CAPTURED_EXEC_EXIT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => (true, Some(status)),
        Ok(Err(_)) => (false, None),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            (false, None)
        }
    }
}

/// 只向宿主 docker exec 的进程组发送一个信号；返回 kill 命令是否成功执行。
#[cfg(unix)]
async fn signal_process_group(host_pid: Option<u32>, signal: &str) -> bool {
    let Some(host_pid) = host_pid else {
        return false;
    };
    let process_group = format!("-{host_pid}");
    let mut kill = Command::new("kill");
    kill.args(["-s", signal, "--", &process_group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    kill.status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 非 Unix 平台没有进程组信号；用 taskkill 终止整棵进程树。
#[cfg(windows)]
async fn signal_process_group(host_pid: Option<u32>, signal: &str) -> bool {
    let Some(host_pid) = host_pid else {
        return false;
    };
    let _ = signal;
    let mut taskkill = Command::new("taskkill");
    taskkill
        .args(["/F", "/T", "/PID", &host_pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    taskkill
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

pub(crate) async fn await_capture_task(task: JoinHandle<StreamCapture>) -> Result<StreamCapture> {
    match task.await {
        Ok(capture) => Ok(capture),
        Err(err) => Err(DockerError::CommandFailed(format!(
            "stream capture task failed: {err}"
        ))),
    }
}

#[derive(Debug)]
struct HeadTailBuffer {
    cap: usize,
    head: Vec<u8>,
    tail: VecDequeBytes,
    total: usize,
}

impl HeadTailBuffer {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            head: Vec::new(),
            tail: VecDequeBytes::new(cap / 2),
            total: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        if self.cap == 0 {
            return;
        }
        let head_cap = self.head_cap();
        if self.head.len() < head_cap {
            let take = (head_cap - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
            if take < bytes.len() {
                self.tail.push(&bytes[take..]);
            }
        } else {
            self.tail.push(bytes);
        }
    }

    fn truncated(&self) -> bool {
        self.total > self.cap
    }

    fn into_bytes(self) -> Vec<u8> {
        if !self.truncated() {
            let mut out = self.head;
            out.extend_from_slice(&self.tail.into_vec());
            return out;
        }
        let omitted = self
            .total
            .saturating_sub(self.head.len())
            .saturating_sub(self.tail.len());
        let marker = format!("\n... omitted {omitted} bytes ...\n");
        let mut out = self.head;
        out.extend_from_slice(marker.as_bytes());
        out.extend_from_slice(&self.tail.into_vec());
        out
    }

    fn head_cap(&self) -> usize {
        self.cap.saturating_sub(self.cap / 2)
    }
}

#[derive(Debug)]
struct VecDequeBytes {
    cap: usize,
    bytes: VecDeque<u8>,
}

impl VecDequeBytes {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            bytes: VecDeque::new(),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.cap == 0 {
            return;
        }
        if bytes.len() >= self.cap {
            self.bytes.clear();
            self.bytes.extend(
                bytes[bytes.len().saturating_sub(self.cap)..]
                    .iter()
                    .copied(),
            );
            return;
        }
        self.bytes.extend(bytes.iter().copied());
        if self.bytes.len() > self.cap {
            let excess = self.bytes.len() - self.cap;
            self.bytes.drain(..excess);
        }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn into_vec(self) -> Vec<u8> {
        self.bytes.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_buffer_keeps_bounded_head_and_tail() {
        let mut buffer = HeadTailBuffer::new(10);
        buffer.push(b"abcdef");
        buffer.push(b"ghijklmnop");

        assert!(buffer.truncated());
        let text = String::from_utf8(buffer.into_bytes()).expect("utf8");
        assert!(text.starts_with("abcde"));
        assert!(text.contains("omitted 6 bytes"));
        assert!(text.ends_with("lmnop"));
    }

    #[test]
    fn head_tail_buffer_keeps_full_when_under_cap() {
        let mut buffer = HeadTailBuffer::new(10);
        buffer.push(b"abc");
        buffer.push(b"def");

        assert!(!buffer.truncated());
        assert_eq!(buffer.into_bytes(), b"abcdef");
    }

    #[test]
    fn storage_cost_counts_every_written_copy() {
        assert_eq!(
            capture_storage_cost(4096),
            4096 * CAPTURE_FILE_COPIES_PER_STREAM
        );
        // 同一份输出若真的落两份文件，成本必须翻倍，否则共享额度只约束其中一份。
        let two_copies = 4096 * 2;
        let session = CaptureSession::new(4096 * 2);
        assert_eq!(session.charge(two_copies), CaptureAcceptance::Accepted);
        assert_eq!(session.charged_bytes(), two_copies);
        assert_eq!(
            session.charge(capture_storage_cost(1)),
            CaptureAcceptance::OutputLimitExceeded {
                limit_bytes: 4096 * 2,
                charged_bytes: two_copies,
                refused_bytes: 1,
            }
        );
    }

    #[test]
    fn shared_budget_refuses_the_chunk_that_would_exceed_the_limit() {
        let session = CaptureSession::new(10);
        let stopped = session.capture_stopped();

        assert_eq!(session.charge(6), CaptureAcceptance::Accepted);
        assert_eq!(session.charge(4), CaptureAcceptance::Accepted);
        assert_eq!(session.charged_bytes(), 10);
        assert!(!stopped.is_cancelled());

        let acceptance = session.charge(1);
        assert_eq!(
            acceptance,
            CaptureAcceptance::OutputLimitExceeded {
                limit_bytes: 10,
                charged_bytes: 10,
                refused_bytes: 1,
            }
        );
        assert_eq!(session.charged_bytes(), 10);
        assert_eq!(session.refused_bytes(), 1);
        assert!(stopped.is_cancelled());
        assert_eq!(session.fault(), None);
    }

    #[test]
    fn both_streams_share_one_limit() {
        let session = CaptureSession::new(8);
        // stdout 先吃掉全部额度，stderr 的 chunk 必须被拒绝，而不是各自享受完整额度。
        assert_eq!(session.charge(8), CaptureAcceptance::Accepted);
        assert!(matches!(
            session.charge(1),
            CaptureAcceptance::OutputLimitExceeded { .. }
        ));
        assert_eq!(session.refused_bytes(), 1);
    }

    #[test]
    fn first_fault_refuses_every_later_chunk() {
        let session = CaptureSession::new(1024);
        let stopped = session.capture_stopped();

        assert!(session.record_failure(ExecCaptureFailure::Read {
            stream: ExecCaptureStream::Stderr,
            message: "broken pipe".to_string(),
        }));
        assert!(!session.record_failure(ExecCaptureFailure::Write {
            stream: ExecCaptureStream::Stdout,
            message: "disk full".to_string(),
        }));
        assert!(stopped.is_cancelled());
        assert_eq!(
            session.charge(10),
            CaptureAcceptance::Faulted,
            "故障之后的 chunk 不允许再进入写盘路径"
        );
        assert_eq!(session.refused_bytes(), 10);
        assert_eq!(session.charged_bytes(), 0);
        assert!(matches!(
            session.fault(),
            Some(ExecCaptureFailure::Read { .. })
        ));
        assert!(session.fault().expect("fault").is_storage_fault());
    }

    #[test]
    fn limit_payload_separates_capacity_from_storage_faults() {
        let session = CaptureSession::new(4);
        assert!(matches!(
            session.charge(8),
            CaptureAcceptance::OutputLimitExceeded { .. }
        ));
        session.record_failure(ExecCaptureFailure::OutputLimitExceeded(
            OutputLimitExceeded {
                limit_bytes: 4,
                charged_bytes: 0,
                refused_bytes: 8,
                termination: ExecTerminationOutcome {
                    docker_exec_exited: false,
                    container_termination: ContainerTerminationOutcome::NotNeeded,
                },
            },
        ));

        let failure =
            session
                .fault()
                .expect("limit fault")
                .with_termination(ExecTerminationOutcome {
                    docker_exec_exited: true,
                    container_termination: ContainerTerminationOutcome::Issued,
                });

        let limit = failure.output_limit_exceeded().expect("limit payload");
        assert_eq!(limit.limit_bytes, 4);
        assert_eq!(limit.refused_bytes, 8);
        assert!(limit.termination.docker_exec_exited);
        assert_eq!(
            limit.termination.container_termination,
            ContainerTerminationOutcome::Issued
        );
        assert!(!failure.is_storage_fault());
        assert!(failure.message().contains("capture limit"));

        let storage = ExecCaptureFailure::Write {
            stream: ExecCaptureStream::Stdout,
            message: "no space left".to_string(),
        };
        assert!(storage.is_storage_fault());
        assert!(storage.output_limit_exceeded().is_none());
        assert!(storage.message().contains("stdout output capture failed"));
    }

    #[tokio::test]
    async fn capture_stream_stops_at_the_limit_and_keeps_bounded_bytes_on_disk() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("stdout.log");
        // 两个 8192 字节的 chunk 之后超限：第一份必须落盘，第二份必须被拒绝。
        let session = CaptureSession::new(12 * 1024);
        let payload = vec![b'x'; 20_000];

        let capture = capture_stream(
            std::io::Cursor::new(payload),
            path.clone(),
            128,
            ExecCaptureStream::Stdout,
            session.clone(),
        );
        let capture = await_capture_task(capture).await.expect("capture");

        let written = tokio::fs::read(&path).await.expect("read capture");
        assert!(
            (written.len() as u64) <= session.limit_bytes(),
            "真实落盘字节必须不超过共享额度"
        );
        assert_eq!(written.len(), 8192, "已受理的第一个 chunk 必须保留在磁盘上");
        assert_eq!(capture.total_bytes, written.len() as u64);
        assert!(capture.truncated, "观察视图按 output_bytes_cap 截断");
        assert!(
            !session.fault().expect("fault").is_storage_fault(),
            "超限是预期容量终止，不是保存故障"
        );
        let limit = session
            .fault()
            .expect("fault")
            .output_limit_exceeded()
            .expect("limit payload");
        assert_eq!(limit.limit_bytes, 12 * 1024);
        assert_eq!(limit.charged_bytes, 8192);
        assert_eq!(limit.refused_bytes, 8192);
        assert!(
            !limit.termination.docker_exec_exited,
            "终止事实只能由调用方在确认宿主子进程后补齐"
        );
    }

    #[tokio::test]
    async fn capture_stream_keeps_every_accepted_byte_when_under_the_limit() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("stderr.log");
        let session = CaptureSession::new(1024);

        let capture = capture_stream(
            b"partial failure output".as_slice(),
            path.clone(),
            1024,
            ExecCaptureStream::Stderr,
            session.clone(),
        );
        let capture = await_capture_task(capture).await.expect("capture");

        assert_eq!(
            tokio::fs::read(&path).await.expect("read"),
            b"partial failure output"
        );
        assert_eq!(capture.total_bytes, 22);
        assert!(!capture.truncated);
        assert_eq!(session.fault(), None);
        assert_eq!(session.charged_bytes(), 22);
    }

    #[tokio::test]
    async fn capture_stream_reports_a_write_fault_instead_of_a_capacity_termination() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        // 目标路径是目录：创建捕获文件必然失败，必须报成 Write 而不是超限。
        let path = directory.path().to_path_buf();
        let session = CaptureSession::new(1024);

        let capture = capture_stream(
            b"payload".as_slice(),
            path,
            1024,
            ExecCaptureStream::Stdout,
            session.clone(),
        );
        let capture = await_capture_task(capture).await.expect("capture");

        assert_eq!(capture.total_bytes, 0);
        let fault = session.fault().expect("write fault");
        assert!(matches!(
            fault,
            ExecCaptureFailure::Write {
                stream: ExecCaptureStream::Stdout,
                ..
            }
        ));
        assert!(fault.is_storage_fault());
        assert!(session.capture_stopped().is_cancelled());
    }

    #[tokio::test]
    async fn capture_stream_reports_a_read_fault_and_stops() {
        struct FailingReader;

        impl AsyncRead for FailingReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("stream broke")))
            }
        }

        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("stdout.log");
        let session = CaptureSession::new(1024);

        let capture = capture_stream(
            FailingReader,
            path,
            1024,
            ExecCaptureStream::Stdout,
            session.clone(),
        );
        let capture = await_capture_task(capture).await.expect("capture");

        assert_eq!(capture.total_bytes, 0);
        let fault = session.fault().expect("read fault");
        assert!(matches!(
            fault,
            ExecCaptureFailure::Read {
                stream: ExecCaptureStream::Stdout,
                ..
            }
        ));
        assert!(fault.is_storage_fault());
    }
}
