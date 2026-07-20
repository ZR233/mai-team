use mai_docker::DockerClient;
use mai_protocol::ProjectId;

use crate::state::RuntimeState;
use crate::{Result, RuntimeError};

pub(crate) const PROJECT_WORKSPACE_PATH: &str = "/workspace/repo";

/// 清理旧版本可能遗留的项目 MCP sidecar。
///
/// 新 MCP runtime 直接绑定 agent 容器，不再创建项目级执行状态机或 sidecar。
pub(crate) async fn delete_sidecar(
    state: &RuntimeState,
    docker: &DockerClient,
    project_id: ProjectId,
) -> Result<Vec<String>> {
    let project = match crate::projects::service::project(state, project_id).await {
        Ok(project) => project,
        Err(RuntimeError::ProjectNotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let preferred = project
        .sidecar
        .write()
        .await
        .take()
        .map(|container| container.id);
    Ok(docker
        .delete_project_sidecar_containers(&project_id.to_string(), preferred.as_deref())
        .await?)
}
