use std::future::Future;
use std::path::{Path, PathBuf};

use chrono::Utc;
use mai_protocol::{AgentId, ArtifactInfo, TaskId};
use uuid::Uuid;

use crate::state::RuntimeState;
use crate::{Result, RuntimeError};

use super::task;

/// Supplies container, persistence, and event effects for task artifact capture.
pub(crate) trait TaskArtifactOps: Send + Sync {
    fn agent_task_id(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<TaskId>>> + Send;

    fn agent_container_id(&self, agent_id: AgentId) -> impl Future<Output = Result<String>> + Send;

    fn artifact_files_root(&self) -> PathBuf;

    fn copy_artifact_from_container(
        &self,
        container_id: String,
        source_path: String,
        dest_path: PathBuf,
    ) -> impl Future<Output = Result<()>> + Send;

    fn save_artifact_record(&self, info: &ArtifactInfo) -> Result<()>;

    fn publish_artifact_created(&self, info: ArtifactInfo) -> impl Future<Output = ()> + Send;
}

pub(crate) async fn save_artifact(
    state: &RuntimeState,
    ops: &impl TaskArtifactOps,
    agent_id: AgentId,
    path: String,
    display_name: Option<String>,
) -> Result<ArtifactInfo> {
    let task_id = ops
        .agent_task_id(agent_id)
        .await?
        .ok_or_else(|| RuntimeError::InvalidInput("Agent has no task".to_string()))?;
    let container_id = ops.agent_container_id(agent_id).await?;

    let name = display_name.unwrap_or_else(|| {
        Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone())
    });
    let name = safe_artifact_name(&name)?;

    let artifact_id = Uuid::new_v4().to_string();
    let dir = artifact_file_dir(&ops.artifact_files_root(), task_id, &artifact_id);
    std::fs::create_dir_all(&dir)?;

    let dest = dir.join(&name);
    ops.copy_artifact_from_container(container_id, path.clone(), dest.clone())
        .await?;

    let size_bytes = std::fs::metadata(&dest)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    let info = ArtifactInfo {
        id: artifact_id,
        agent_id,
        task_id,
        name,
        path,
        size_bytes,
        created_at: Utc::now(),
    };

    ops.save_artifact_record(&info)?;

    let task = task(state, task_id).await?;
    task.artifacts.write().await.push(info.clone());

    ops.publish_artifact_created(info.clone()).await;

    Ok(info)
}

pub(crate) fn artifact_file_path(root: &Path, info: &ArtifactInfo) -> PathBuf {
    artifact_file_dir(root, info.task_id, &info.id).join(&info.name)
}

fn artifact_file_dir(root: &Path, task_id: TaskId, artifact_id: &str) -> PathBuf {
    root.join(task_id.to_string()).join(artifact_id)
}

fn safe_artifact_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "artifact name cannot be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(RuntimeError::InvalidInput(
            "artifact name must be a file name".to_string(),
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(RuntimeError::InvalidInput(
            "artifact name cannot contain path separators".to_string(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(RuntimeError::InvalidInput(
            "artifact name cannot contain control characters".to_string(),
        ));
    }
    Ok(name.to_string())
}
