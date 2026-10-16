//! 命令捕获文件的单一写入边界；进程与 generation 生命周期仍由命令后端持有。

use crate::{Result, RuntimeError};
use pl_tool::command::{CommandCaptureStream, CommandOutputTarget};
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// 合并文件与原始流副本共用同一个写入顺序，确认偏移只描述合并文件。
#[derive(Debug, Default)]
pub(super) struct CommandCaptureFiles {
    write_lock: tokio::sync::Mutex<()>,
}

impl CommandCaptureFiles {
    /// 返回本次写入的全部宿主捕获成本；不读取文件，也不获取异步锁。
    pub(super) fn storage_cost(
        target: &CommandOutputTarget,
        stream: CommandCaptureStream,
        chunk: &[u8],
    ) -> u64 {
        let stream_capture = match stream {
            CommandCaptureStream::Stdout => target.stdout_capture_file(),
            CommandCaptureStream::Stderr => target.stderr_capture_file(),
        };
        let copies = if stream_capture.is_some() { 2 } else { 1 };
        (chunk.len() as u64) * copies
            + b"=== STDOUT ===\n".len() as u64
            + u64::from(!chunk.ends_with(b"\n"))
    }

    pub(super) async fn prepare_output(
        &self,
        target: &CommandOutputTarget,
        command: &str,
        working_directory: &str,
        capture_limit_bytes: u64,
    ) -> Result<u64> {
        let header = format!("=== COMMAND ===\n{command}\n\n=== CWD ===\n{working_directory}\n\n");
        if header.len() as u64 > capture_limit_bytes {
            return Err(RuntimeError::InvalidInput(format!(
                "命令捕获头部需要 {} 字节，超过 {capture_limit_bytes} 字节上限；命令未启动",
                header.len()
            )));
        }
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = target.capture_file().parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(target.capture_file(), header.as_bytes()).await?;
        Ok(header.len() as u64)
    }

    pub(super) async fn append_output_chunk(
        &self,
        target: &CommandOutputTarget,
        stream: CommandCaptureStream,
        chunk: &[u8],
    ) -> Result<u64> {
        let _guard = self.write_lock.lock().await;
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

    pub(super) async fn repair_output_chunk(
        &self,
        capture_file: &Path,
        stream: CommandCaptureStream,
        committed_len: u64,
        chunk: &[u8],
    ) -> Result<u64> {
        // 先把可能写坏的部分截断回上次确认的偏移，再按正常 append 的 frame 重放一次，
        // 因此重复 repair 得到相同字节而不是重复追加，命令本身不会被重跑。
        // 这里只修复模型可见的合并输出片段；artifact 的 per-stream 文件仍只由成功 append 写入。
        let _guard = self.write_lock.lock().await;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn exact_cost_matches_both_streams_framing_and_optional_copies() {
        for with_copies in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = CommandOutputTarget::new(dir.path().join("output.log"), "output.log");
            let target = if with_copies {
                target
                    .with_stream_capture_files(dir.path().join("stdout"), dir.path().join("stderr"))
            } else {
                target
            };
            let files = CommandCaptureFiles::default();
            let header = files
                .prepare_output(&target, "probe", "/workspace", 4096)
                .await
                .unwrap();
            let mut charged = header;
            for (stream, chunk) in [
                (CommandCaptureStream::Stdout, b"line\n".as_slice()),
                (CommandCaptureStream::Stderr, b"tail".as_slice()),
                (CommandCaptureStream::Stdout, "中文\n".as_bytes()),
                (CommandCaptureStream::Stderr, &[0xff, 0x00]),
            ] {
                charged += CommandCaptureFiles::storage_cost(&target, stream, chunk);
                files
                    .append_output_chunk(&target, stream, chunk)
                    .await
                    .unwrap();
            }
            let mut actual = tokio::fs::metadata(target.capture_file())
                .await
                .unwrap()
                .len();
            if with_copies {
                actual += tokio::fs::metadata(target.stdout_capture_file().unwrap())
                    .await
                    .unwrap()
                    .len();
                actual += tokio::fs::metadata(target.stderr_capture_file().unwrap())
                    .await
                    .unwrap()
                    .len();
            }
            assert_eq!(charged, actual);
        }
    }

    #[tokio::test]
    async fn oversized_header_is_rejected_before_creating_capture_files() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("uncreated");
        let target = CommandOutputTarget::new(parent.join("output.log"), "output.log");
        let result = CommandCaptureFiles::default()
            .prepare_output(&target, "probe", "/workspace", 1)
            .await;
        assert!(result.is_err(), "超限头部必须在写盘前拒绝");
        assert!(!parent.exists(), "超限准备不得创建捕获文件或目录");
    }

    #[tokio::test]
    async fn repair_replays_only_the_accepted_fragment_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let target = CommandOutputTarget::new(dir.path().join("output.log"), "output.log");
        let files = CommandCaptureFiles::default();
        let header_len = files
            .prepare_output(&target, "probe", "/workspace", 4096)
            .await
            .unwrap();
        let header = tokio::fs::read(target.capture_file()).await.unwrap();
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(target.capture_file())
            .await
            .unwrap();
        file.write_all(b"partial frame").await.unwrap();
        drop(file);
        let first = files
            .repair_output_chunk(
                target.capture_file(),
                CommandCaptureStream::Stderr,
                header_len,
                b"accepted\n",
            )
            .await
            .unwrap();
        let second = files
            .repair_output_chunk(
                target.capture_file(),
                CommandCaptureStream::Stderr,
                header_len,
                b"accepted\n",
            )
            .await
            .unwrap();
        let expected = [header.as_slice(), b"=== STDERR ===\naccepted\n"].concat();
        assert_eq!(
            (
                first,
                second,
                tokio::fs::read(target.capture_file()).await.unwrap()
            ),
            (expected.len() as u64, expected.len() as u64, expected)
        );
    }
}
