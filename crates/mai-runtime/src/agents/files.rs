use std::future::Future;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use mai_protocol::AgentId;
use tempfile::NamedTempFile;

use crate::{Result, RuntimeError};

/// Provides the container file transfer side effects needed by agent file APIs
/// without exposing the full runtime to request decoding and Docker copy logic.
pub(crate) trait AgentFileOps: Send + Sync {
    fn container_id(&self, agent_id: AgentId) -> impl Future<Output = Result<String>> + Send;

    fn copy_to_container(
        &self,
        container_id: String,
        local_path: PathBuf,
        container_path: String,
    ) -> impl Future<Output = Result<()>> + Send;

    fn copy_from_container_tar(
        &self,
        container_id: String,
        container_path: String,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

pub(crate) async fn upload_file(
    ops: &impl AgentFileOps,
    agent_id: AgentId,
    path: String,
    content_base64: String,
) -> Result<usize> {
    let bytes = BASE64
        .decode(content_base64.trim())
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid base64: {err}")))?;
    let temp = NamedTempFile::new()?;
    std::fs::write(temp.path(), &bytes)?;
    let container_id = ops.container_id(agent_id).await?;
    ops.copy_to_container(container_id, temp.path().to_path_buf(), path)
        .await?;
    Ok(bytes.len())
}

pub(crate) async fn download_file_tar(
    ops: &impl AgentFileOps,
    agent_id: AgentId,
    path: String,
) -> Result<Vec<u8>> {
    let container_id = ops.container_id(agent_id).await?;
    ops.copy_from_container_tar(container_id, path).await
}
