use std::collections::VecDeque;
use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::error::{DockerError, Result};

#[derive(Debug)]
pub(crate) struct StreamCapture {
    pub(crate) text: String,
    pub(crate) total_bytes: u64,
    pub(crate) truncated: bool,
}

pub(crate) fn capture_stream<R>(
    reader: R,
    path: PathBuf,
    output_bytes_cap: usize,
) -> JoinHandle<Result<StreamCapture>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut reader = reader;
        let mut file = tokio::fs::File::create(path).await?;
        let mut view = HeadTailBuffer::new(output_bytes_cap);
        let mut total_bytes = 0_u64;
        let mut chunk = [0_u8; 8192];
        loop {
            let n = reader.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            let bytes = &chunk[..n];
            file.write_all(bytes).await?;
            view.push(bytes);
            total_bytes = total_bytes.saturating_add(n as u64);
        }
        file.flush().await?;
        let truncated = view.truncated();
        Ok(StreamCapture {
            text: String::from_utf8_lossy(&view.into_bytes()).into_owned(),
            total_bytes,
            truncated,
        })
    })
}

pub(crate) async fn await_capture_task(
    task: JoinHandle<Result<StreamCapture>>,
) -> Result<StreamCapture> {
    match task.await {
        Ok(result) => result,
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
}
