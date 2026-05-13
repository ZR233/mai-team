pub mod chat_completions;
pub mod responses;

use crate::error::Result;
use crate::types::ModelStreamEvent;
use mai_protocol::{ModelInputItem, ToolDefinition};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Debug;

pub(crate) trait WireProtocol: Debug + Send + Sync {
    fn path(&self) -> &'static str;
    fn build_body(&self, req: &WireRequest<'_>) -> Result<Vec<u8>>;
    fn parse_stream_event(&self, event: &SseFrame) -> Result<Vec<ModelStreamEvent>>;
    fn parse_stream_done(&self) -> Result<Vec<ModelStreamEvent>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseFrame {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

pub(crate) struct WireRequest<'a> {
    pub(crate) model_id: &'a str,
    pub(crate) instructions: &'a str,
    pub(crate) input: &'a [ModelInputItem],
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) tool_choice: Option<&'a str>,
    pub(crate) stream: bool,
    pub(crate) store: Option<bool>,
    pub(crate) previous_response_id: Option<&'a str>,
    pub(crate) max_output_tokens: u64,
    pub(crate) extra_body: BTreeMap<String, Value>,
    pub(crate) supports_tools: bool,
}

pub(crate) fn parse_usage(value: Option<&Value>) -> Option<mai_protocol::TokenUsage> {
    use mai_protocol::TokenUsage;
    let value = value?;
    let input_tokens = value
        .get("input_tokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = value
        .get("output_tokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

pub(crate) fn parse_sse_frames(buffer: &mut Vec<u8>, chunk: &[u8]) -> Result<Vec<SseFrame>> {
    buffer.extend_from_slice(chunk);
    let mut frames = Vec::new();
    loop {
        let Some(end) = find_frame_end(buffer) else {
            break;
        };
        let raw = buffer[..end].to_vec();
        let drain_to = if buffer[end..].starts_with(b"\r\n\r\n") {
            end + 4
        } else {
            end + 2
        };
        buffer.drain(..drain_to);
        let raw = std::str::from_utf8(&raw)
            .map_err(|err| crate::error::ModelError::Stream(format!("invalid SSE utf-8: {err}")))?;
        if let Some(frame) = parse_sse_frame(raw) {
            frames.push(frame);
        }
    }
    Ok(frames)
}

fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_sse_frame(raw: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data = Vec::new();
    for line in raw.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value.to_string()),
            _ => {}
        }
    }
    if event.is_none() && data.is_empty() {
        return None;
    }
    Some(SseFrame {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_sse_frames_with_multiline_data_and_comments() {
        let mut buffer = Vec::new();
        assert!(parse_sse_frames(&mut buffer, b": hi\n\nevent: a\ndata: one\n").unwrap().is_empty());
        let frames = parse_sse_frames(&mut buffer, b"data: two\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[0].data, "one\ntwo");
    }

    #[test]
    fn parses_utf8_split_across_chunks_without_lossy_replacement() {
        let mut buffer = Vec::new();
        let bytes = "event: a\ndata: 你\n\n".as_bytes();
        assert!(parse_sse_frames(&mut buffer, &bytes[..16]).unwrap().is_empty());
        let frames = parse_sse_frames(&mut buffer, &bytes[16..]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "你");
    }
}
