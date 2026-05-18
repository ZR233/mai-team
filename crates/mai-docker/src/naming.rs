use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const MANAGED_LABEL: &str = "mai.team.managed=true";
pub(crate) const AGENT_LABEL_KEY: &str = "mai.team.agent";
pub(crate) const KIND_LABEL_KEY: &str = "mai.team.kind";
pub(crate) const PROJECT_LABEL_KEY: &str = "mai.team.project";
pub(crate) const ROLE_LABEL_KEY: &str = "mai.team.role";
pub(crate) const SIDECAR_LABEL_KEY: &str = "mai.team.sidecar";
pub(crate) const SIDECAR_KIND_LABEL_KEY: &str = "mai.team.sidecar.kind";
pub(crate) const PROJECT_SIDECAR_KIND: &str = "project";

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
