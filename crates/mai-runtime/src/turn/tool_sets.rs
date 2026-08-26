use std::collections::HashMap;

use mai_protocol::AgentId;
use tokio::sync::RwLock;

/// mai runtime 中每个产品 Agent 唯一拥有的 PL 工具集合。
pub(crate) struct AgentToolSets {
    manager: pl_core::ToolManager,
    sets: RwLock<HashMap<AgentId, pl_core::AgentToolSet>>,
}

impl AgentToolSets {
    pub(crate) fn new() -> Self {
        Self {
            manager: pl_core::ToolManager::new(),
            sets: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) async fn get(&self, agent_id: AgentId) -> pl_core::AgentToolSet {
        let mut sets = self.sets.write().await;
        sets.entry(agent_id)
            .or_insert_with(|| {
                self.manager.agent_tool_set(
                    agent_id.to_string(),
                    pl_core::GlobalToolInheritance::Isolated,
                )
            })
            .clone()
    }

    pub(crate) async fn remove(&self, agent_id: AgentId) {
        self.sets.write().await.remove(&agent_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pl_core::{Tool, ToolGroupId, ToolResult, TypedTool};
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct EmptyInput {}

    #[tokio::test]
    async fn agent_tool_groups_never_leak_between_agents() {
        let sets = AgentToolSets::new();
        let first_id = AgentId::new_v4();
        let second_id = AgentId::new_v4();
        let first = sets.get(first_id).await;
        let second = sets.get(second_id).await;
        let tool = TypedTool::<EmptyInput>::new("project_only", "project-scoped tool").handler(
            |_: EmptyInput, _| async { Ok::<_, pl_protocol::PureError>(ToolResult::success("ok")) },
        );

        first
            .install(
                ToolGroupId::new("project"),
                vec![Arc::new(tool) as Arc<dyn Tool>],
            )
            .expect("install first agent tool");

        assert_eq!(
            first.freeze().names().collect::<Vec<_>>(),
            vec!["project_only"]
        );
        assert_eq!(second.freeze().names().count(), 0);
    }
}
