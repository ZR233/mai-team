use serde::{Deserialize, Serialize};

/// mai 产品外部资源（容器、workspace、MCP）的生命周期。
///
/// 该状态只描述产品资源，不能表达 PL Agent 的执行阶段。执行状态唯一来自
/// `pl_protocol::AgentSnapshot`。
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Default,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AgentResourceState {
    #[default]
    Provisioning,
    Ready,
    Deleting,
    Failed,
    Deleted,
}

/// 产品资源的完整、正交快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AgentResourceSnapshot {
    pub state: AgentResourceState,
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn resource_snapshot_has_no_framework_lifecycle_projection() {
        let value = serde_json::to_value(AgentResourceSnapshot {
            state: AgentResourceState::Ready,
            error: None,
        })
        .expect("serialize resource snapshot");

        assert_eq!(value, json!({ "state": "ready", "error": null }));
        assert!(value.get("runtime").is_none());
        assert!(value.get("activity").is_none());
        assert!(value.get("activeTurn").is_none());
    }
}
