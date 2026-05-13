use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::Result;
use crate::naming::{
    AGENT_LABEL_KEY, PROJECT_LABEL_KEY, SIDECAR_KIND_LABEL_KEY, SIDECAR_LABEL_KEY,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub agent_id: Option<String>,
    pub project_id: Option<String>,
    pub sidecar: bool,
    pub sidecar_kind: Option<String>,
}

impl ManagedContainer {
    pub(crate) fn matches_identifier(&self, identifier: &str) -> bool {
        let identifier = identifier.trim().trim_start_matches('/');
        self.id == identifier
            || self.id.starts_with(identifier)
            || self.name == identifier
            || self.name.trim_start_matches('/') == identifier
    }
}

pub(crate) fn managed_containers_from_inspect(json: &str) -> Result<Vec<ManagedContainer>> {
    let inspected = serde_json::from_str::<Vec<InspectContainer>>(json)?;
    Ok(inspected.into_iter().map(ManagedContainer::from).collect())
}

#[derive(Debug, Deserialize)]
struct InspectContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Config")]
    config: Option<InspectConfig>,
    #[serde(rename = "State")]
    state: Option<InspectState>,
}

#[derive(Debug, Deserialize)]
struct InspectConfig {
    #[serde(rename = "Image")]
    image: Option<String>,
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(rename = "Status")]
    status: Option<String>,
}

impl From<InspectContainer> for ManagedContainer {
    fn from(value: InspectContainer) -> Self {
        let labels = value
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref());
        let agent_id = labels.and_then(|labels| labels.get(AGENT_LABEL_KEY).cloned());
        let project_id = labels.and_then(|labels| labels.get(PROJECT_LABEL_KEY).cloned());
        let sidecar = labels
            .and_then(|labels| labels.get(SIDECAR_LABEL_KEY))
            .is_some_and(|value| value == "true");
        let sidecar_kind = labels.and_then(|labels| labels.get(SIDECAR_KIND_LABEL_KEY).cloned());
        let image = value
            .config
            .and_then(|config| config.image)
            .unwrap_or_default();
        let state = value
            .state
            .and_then(|state| state.status)
            .unwrap_or_default();

        Self {
            id: value.id,
            name: value.name.trim_start_matches('/').to_string(),
            image,
            state,
            agent_id,
            project_id,
            sidecar,
            sidecar_kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_managed_containers_from_inspect_json() {
        let containers = managed_containers_from_inspect(
            r#"
            [
                {
                    "Id": "abc123",
                    "Name": "/mai-team-agent-1",
                    "Config": {
                        "Image": "ubuntu:latest",
                        "Labels": {
                            "mai.team.managed": "true",
                            "mai.team.agent": "agent-1",
                            "mai.team.project": "project-1",
                            "mai.team.sidecar": "true",
                            "mai.team.sidecar.kind": "project"
                        }
                    },
                    "State": { "Status": "exited" }
                },
                {
                    "Id": "def456",
                    "Name": "/mai-team-unlabeled",
                    "Config": {
                        "Image": "ubuntu:latest",
                        "Labels": {
                            "mai.team.managed": "true"
                        }
                    },
                    "State": { "Status": "running" }
                }
            ]
            "#,
        )
        .expect("parse containers");

        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].id, "abc123");
        assert_eq!(containers[0].name, "mai-team-agent-1");
        assert_eq!(containers[0].image, "ubuntu:latest");
        assert_eq!(containers[0].state, "exited");
        assert_eq!(containers[0].agent_id.as_deref(), Some("agent-1"));
        assert_eq!(containers[0].project_id.as_deref(), Some("project-1"));
        assert!(containers[0].sidecar);
        assert_eq!(containers[0].sidecar_kind.as_deref(), Some("project"));
        assert_eq!(containers[1].agent_id, None);
        assert!(!containers[1].sidecar);
    }
}
