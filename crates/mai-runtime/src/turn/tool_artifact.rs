use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::PathBuf;

use base64::Engine;
use mai_protocol::{AgentId, ToolOutputArtifactInfo};
use serde::Deserialize;

use crate::{AgentRuntime, Result, RuntimeError};

const DEFAULT_MAX_LINES: usize = 200;
const MAX_LINES: usize = 500;
const DEFAULT_MAX_BYTES: usize = 12 * 1024;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ReadToolArtifactInput {
    call_id: String,
    artifact_id: String,
    start_line: Option<usize>,
    max_lines: Option<usize>,
    start_byte: Option<u64>,
    max_bytes: Option<usize>,
}

pub(super) async fn read(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    input: ReadToolArtifactInput,
) -> Result<serde_json::Value> {
    validate_range(&input)?;
    let (artifact, path) = runtime
        .tool_output_artifact(
            agent_id,
            None,
            input.call_id.clone(),
            input.artifact_id.clone(),
        )
        .await?;
    if input.start_byte.is_some() || input.max_bytes.is_some() {
        read_bytes(artifact, path, input).await
    } else {
        read_lines(artifact, path, input).await
    }
}

fn validate_range(input: &ReadToolArtifactInput) -> Result<()> {
    let line_range = input.start_line.is_some() || input.max_lines.is_some();
    let byte_range = input.start_byte.is_some() || input.max_bytes.is_some();
    if line_range && byte_range {
        return Err(RuntimeError::InvalidInput(
            "read_tool_artifact accepts either a line range or a byte range, not both".to_string(),
        ));
    }
    if input.start_line == Some(0) {
        return Err(RuntimeError::InvalidInput(
            "read_tool_artifact startLine is 1-based".to_string(),
        ));
    }
    if input
        .max_lines
        .is_some_and(|value| value == 0 || value > MAX_LINES)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "read_tool_artifact maxLines must be between 1 and {MAX_LINES}"
        )));
    }
    if input
        .max_bytes
        .is_some_and(|value| value == 0 || value > MAX_BYTES)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "read_tool_artifact maxBytes must be between 1 and {MAX_BYTES}"
        )));
    }
    Ok(())
}

async fn read_lines(
    artifact: ToolOutputArtifactInfo,
    path: PathBuf,
    input: ReadToolArtifactInput,
) -> Result<serde_json::Value> {
    tokio::task::spawn_blocking(move || {
        let start_line = input.start_line.unwrap_or(1);
        let max_lines = input.max_lines.unwrap_or(DEFAULT_MAX_LINES);
        let file = std::fs::File::open(&path)?;
        let mut lines = std::io::BufReader::new(file).lines().skip(start_line - 1);
        let mut selected = Vec::new();
        for line in lines.by_ref().take(max_lines) {
            selected.push(line?);
        }
        let has_more = lines.next().transpose()?.is_some();
        let end_line = start_line + selected.len().saturating_sub(1);
        let text = selected.join("\n");
        Ok(serde_json::json!({
            "callId": artifact.call_id,
            "artifactId": artifact.id,
            "name": artifact.name,
            "range": "lines",
            "startLine": start_line,
            "endLine": end_line,
            "nextStartLine": has_more.then_some(end_line + 1),
            "contentHash": pl_core::canonical_content_hash(text.as_bytes()),
            "text": text,
        }))
    })
    .await
    .map_err(|error| RuntimeError::InvalidInput(format!("artifact reader task failed: {error}")))?
}

async fn read_bytes(
    artifact: ToolOutputArtifactInfo,
    path: PathBuf,
    input: ReadToolArtifactInput,
) -> Result<serde_json::Value> {
    tokio::task::spawn_blocking(move || {
        let start_byte = input.start_byte.unwrap_or(0);
        let max_bytes = input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        let mut file = std::fs::File::open(&path)?;
        let total_bytes = file.metadata()?.len();
        file.seek(SeekFrom::Start(start_byte))?;
        let mut bytes = Vec::with_capacity(max_bytes);
        file.take(max_bytes as u64).read_to_end(&mut bytes)?;
        let end_byte = start_byte.saturating_add(bytes.len() as u64);
        Ok(serde_json::json!({
            "callId": artifact.call_id,
            "artifactId": artifact.id,
            "name": artifact.name,
            "range": "bytes",
            "startByte": start_byte,
            "endByteExclusive": end_byte,
            "nextStartByte": (end_byte < total_bytes).then_some(end_byte),
            "totalBytes": total_bytes,
            "contentHash": pl_core::canonical_content_hash(&bytes),
            "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }))
    })
    .await
    .map_err(|error| RuntimeError::InvalidInput(format!("artifact reader task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::*;

    fn artifact(size_bytes: u64) -> ToolOutputArtifactInfo {
        ToolOutputArtifactInfo {
            id: "artifact-1".to_string(),
            call_id: "call-1".to_string(),
            agent_id: Uuid::nil(),
            name: "stdout".to_string(),
            stream: "stdout".to_string(),
            size_bytes,
            created_at: Utc::now(),
        }
    }

    fn input() -> ReadToolArtifactInput {
        ReadToolArtifactInput {
            call_id: "call-1".to_string(),
            artifact_id: "artifact-1".to_string(),
            start_line: None,
            max_lines: None,
            start_byte: None,
            max_bytes: None,
        }
    }

    #[tokio::test]
    async fn line_ranges_return_a_stable_continuation() {
        let temp = tempfile::NamedTempFile::new().expect("artifact file");
        std::fs::write(temp.path(), "one\ntwo\nthree\n").expect("write artifact");
        let mut request = input();
        request.start_line = Some(2);
        request.max_lines = Some(1);

        let value = read_lines(
            artifact(temp.path().metadata().expect("metadata").len()),
            temp.path().to_path_buf(),
            request,
        )
        .await
        .expect("read line range");

        assert_eq!(value["text"], "two");
        assert_eq!(value["startLine"], 2);
        assert_eq!(value["endLine"], 2);
        assert_eq!(value["nextStartLine"], 3);
    }

    #[tokio::test]
    async fn byte_ranges_are_bounded_and_base64_encoded() {
        let temp = tempfile::NamedTempFile::new().expect("artifact file");
        std::fs::write(temp.path(), b"abcdef").expect("write artifact");
        let mut request = input();
        request.start_byte = Some(1);
        request.max_bytes = Some(2);

        let value = read_bytes(
            artifact(temp.path().metadata().expect("metadata").len()),
            temp.path().to_path_buf(),
            request,
        )
        .await
        .expect("read byte range");

        assert_eq!(value["base64"], "YmM=");
        assert_eq!(value["startByte"], 1);
        assert_eq!(value["endByteExclusive"], 3);
        assert_eq!(value["nextStartByte"], 3);
    }

    #[test]
    fn mixed_line_and_byte_ranges_are_rejected() {
        let mut request = input();
        request.start_line = Some(1);
        request.start_byte = Some(0);

        assert!(validate_range(&request).is_err());
    }
}
