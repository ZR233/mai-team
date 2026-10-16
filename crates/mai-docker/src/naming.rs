use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

pub(crate) const MANAGED_LABEL: &str = "mai.team.managed=true";
pub(crate) const AGENT_LABEL_KEY: &str = "mai.team.agent";
pub(crate) const KIND_LABEL_KEY: &str = "mai.team.kind";
pub(crate) const PROJECT_LABEL_KEY: &str = "mai.team.project";
pub(crate) const ROLE_LABEL_KEY: &str = "mai.team.role";
pub(crate) const SIDECAR_LABEL_KEY: &str = "mai.team.sidecar";
pub(crate) const SIDECAR_KIND_LABEL_KEY: &str = "mai.team.sidecar.kind";
pub(crate) const PROJECT_SIDECAR_KIND: &str = "project";
pub(crate) const AGENT_MCP_SIDECAR_KIND: &str = "agent-mcp";
/// 单次环境换代的操作标识标签；用于定位候选与旧 generation，不用于授权判断。
pub(crate) const GENERATION_LABEL_KEY: &str = "mai.team.generation";
/// 保留旧容器的快照镜像标签；重启后按 operation 找回 commit 结果。
pub(crate) const SNAPSHOT_OPERATION_LABEL_KEY: &str = "mai.team.snapshot.operation";
/// 快照镜像记录它来自哪个旧容器，便于收据核对。
pub(crate) const SNAPSHOT_CONTAINER_LABEL_KEY: &str = "mai.team.snapshot.container";

pub fn agent_workspace_volume(agent_id: &str) -> String {
    format!("mai-team-workspace-{agent_id}")
}

pub fn project_agent_workspace_volume(project_id: &str, agent_id: &str) -> String {
    format!("mai-team-project-{project_id}-agent-{agent_id}")
}

pub fn project_cache_volume(project_id: &str) -> String {
    format!("mai-team-project-{project_id}-cache")
}

pub(crate) fn agent_container_name(agent_id: &str) -> String {
    format!("mai-team-{agent_id}")
}

/// 换代候选的容器名：同一 Agent 的新 generation 使用新名字，旧容器在被定向清理前
/// 仍然保留自己的名字，因此候选不会被旧容器的名字冲突挡住。
pub(crate) fn agent_generation_container_name(agent_id: &str, generation: Uuid) -> String {
    format!("mai-team-{agent_id}-g{}", generation_short(generation))
}

/// 与 Agent 同 generation 的 MCP sidecar 容器名。
pub(crate) fn agent_generation_mcp_sidecar_container_name(
    agent_id: &str,
    generation: Uuid,
) -> String {
    format!(
        "mai-team-agent-mcp-{agent_id}-g{}",
        generation_short(generation)
    )
}

fn generation_short(generation: Uuid) -> String {
    generation.simple().to_string().chars().take(12).collect()
}

pub(crate) fn generation_label_value(generation: Uuid) -> String {
    format!("{GENERATION_LABEL_KEY}={generation}")
}

pub(crate) fn agent_label(agent_id: &str) -> String {
    format!("{AGENT_LABEL_KEY}={agent_id}")
}

pub(crate) fn project_sidecar_container_name(project_id: &str) -> String {
    format!("mai-team-project-sidecar-{project_id}")
}

pub(crate) fn snapshot_image_name(agent_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("mai-team-snapshot-{agent_id}-{nanos}")
}

/// 受管换代快照镜像的确定性名字。
///
/// 名字只由 Agent 和 operation UUID 决定，因此崩溃重试后仍能按同一名字找回
/// commit 结果；不使用时间戳，也不再依赖任何进程内状态。
pub(crate) fn environment_snapshot_image_name(agent_id: &str, operation_id: Uuid) -> String {
    format!("mai-team-snapshot-{agent_id}-{operation_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_cache_volume_uses_project_id() {
        assert_eq!(
            project_cache_volume("project-1"),
            "mai-team-project-project-1-cache"
        );
    }

    #[test]
    fn project_agent_workspace_volume_uses_project_and_agent_id() {
        assert_eq!(
            project_agent_workspace_volume("project-1", "agent-1"),
            "mai-team-project-project-1-agent-agent-1"
        );
    }

    #[test]
    fn snapshot_image_name_uses_agent_id_and_snapshot_prefix() {
        let image = snapshot_image_name("child-agent");

        assert!(image.starts_with("mai-team-snapshot-child-agent-"));
    }
}
