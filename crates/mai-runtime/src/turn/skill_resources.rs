use std::sync::Arc;

use pl_core::{FunctionToolDefinition, ToolEntry, ToolSourceId, ToolSourceMetadata};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::state::AgentRecord;
use crate::turn::product_tool_schemas::{TOOL_LIST_SKILL_RESOURCES, TOOL_READ_SKILL_RESOURCE};
use crate::turn::tool_output::ToolExecution;
use crate::{AgentRuntime, RuntimeError};

/// mai-team 技能资源工具的工具来源标识。
///
/// 技能资源不再伪装成 MCP server 混入 pl 的 MCP resource 工具，而是作为独立
/// 来源发布；MCP resource 工具由 pl 的 MCP runtime 按自身 generation 发布。
pub(crate) const SKILL_TOOL_SOURCE: &str = "mai-skills";

const LIST_DESCRIPTION: &str = "List enabled skills visible to this agent as readable resources. \
     Each entry carries a `skill:///<skill-name>` URI, a short description, the scope, and a link to the skill document.";

const READ_DESCRIPTION: &str = "Read a skill resource by its `skill:///<skill-name>` URI. \
     The bare skill URI returns the skill's main document; append a relative path to read another file inside the skill directory.";

/// 无参数输入。
#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListSkillResourcesInput {}

#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadSkillResourceInput {
    /// `skill:///<skill-name>` 形式的技能资源 URI。
    pub(crate) uri: String,
}

/// 构建当前 agent 的技能资源工具条目；工具执行时按需读取技能目录。
pub(crate) fn skill_resource_entries(
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
) -> Vec<ToolEntry> {
    let source = ToolSourceId::new(SKILL_TOOL_SOURCE);
    let list_runtime = runtime.clone();
    let list_agent = agent.clone();
    vec![
        ToolEntry::new(
            FunctionToolDefinition::<ListSkillResourcesInput>::new(
                TOOL_LIST_SKILL_RESOURCES,
                LIST_DESCRIPTION,
            )
            .registered(
                move |_: ListSkillResourcesInput, _: pl_core::ToolContext| {
                    let runtime = list_runtime.clone();
                    let agent = list_agent.clone();
                    async move {
                        let broker = runtime.agent_resource_broker(&agent).await?;
                        let execution = ToolExecution::json(broker.list_skill_resources())?;
                        Ok::<_, RuntimeError>(execution)
                    }
                },
            ),
            ToolSourceMetadata::new(source.clone()),
        ),
        ToolEntry::new(
            FunctionToolDefinition::<ReadSkillResourceInput>::new(
                TOOL_READ_SKILL_RESOURCE,
                READ_DESCRIPTION,
            )
            .registered(
                move |input: ReadSkillResourceInput, _: pl_core::ToolContext| {
                    let runtime = runtime.clone();
                    let agent = agent.clone();
                    async move {
                        let broker = runtime.agent_resource_broker(&agent).await?;
                        let resource = broker.read_skill_resource(&input.uri)?;
                        let execution = ToolExecution::json(resource)?;
                        Ok::<_, RuntimeError>(execution)
                    }
                },
            ),
            ToolSourceMetadata::new(source.clone()),
        ),
    ]
}
