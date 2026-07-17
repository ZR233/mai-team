use std::sync::{Arc, Weak};

use mai_protocol::AgentId;
use pl_core::{
    AgentTurnFactory, AgentTurnPreparationContext, ContextCompactionConfig,
    ContextCompactionReplacement, CoreAgentProfile, HostMcpToolSpec, InstructionSnapshot,
    PreparedAgentTurn, RecentInteractionTailConfig, TurnEngineBuilder, TurnOptions, TurnRequest,
};
use pl_model::{OpenAiCompactionMode, create_provider_with_catalog};
use tokio::sync::RwLock;

use crate::skills::{SkillInput, SkillSelection};
use crate::state::AgentRecord;
use crate::turn::core_adapter::{
    MaiFrameworkKernelBuildContext, build_mai_framework_kernel, mai_user_input_interaction_callback,
};
use crate::{AgentRuntime, MaiConfig, Result, RuntimeError};

use super::protocol_uuid;

/// 由 MaiConfig 和产品资源为一次 PL turn 准备 kernel/policy。
#[derive(Clone)]
pub(crate) struct MaiAgentTurnFactory {
    runtime: Weak<AgentRuntime>,
    config: Arc<RwLock<MaiConfig>>,
}

impl MaiAgentTurnFactory {
    pub(crate) fn new(runtime: Weak<AgentRuntime>, config: Arc<RwLock<MaiConfig>>) -> Self {
        Self { runtime, config }
    }
}

impl AgentTurnFactory for MaiAgentTurnFactory {
    type Error = RuntimeError;

    async fn prepare_turn(
        &self,
        context: AgentTurnPreparationContext,
    ) -> Result<PreparedAgentTurn> {
        let runtime = self.runtime.upgrade().ok_or_else(|| {
            RuntimeError::InvalidInput("mai agent host is shutting down".to_string())
        })?;
        let (product_agent_id, agent) =
            product_agent(&runtime, &context.snapshot.identity.id).await?;
        let config = self.config.read().await.clone();
        let route = config
            .models
            .resolve(&context.snapshot.identity.role)
            .map_err(RuntimeError::Model)?;
        let provider = create_provider_with_catalog(route.provider_info, route.models)
            .map_err(RuntimeError::Model)?;
        let mut builder = TurnEngineBuilder::new(provider);
        if let Some(effort) = &route.reasoning_effort {
            builder = builder.with_reasoning_effort(pl_core::ReasoningEffort::new(effort.as_str()));
        }

        if let Err(error) = runtime.refresh_project_skills_for_agent(&agent).await {
            tracing::warn!(agent_id = %product_agent_id, "failed to refresh project skills: {error}");
        }
        let mut skills_config = runtime.deps.store.load_skills_config().await?;
        apply_skill_policy(&mut skills_config, &config.skills);
        let skills_manager = runtime.skills_manager_for_agent(&agent).await?;
        let container_skill_paths = runtime
            .sync_agent_skills_to_container(&agent, &skills_manager, &skills_config)
            .await?;
        let initial_mcp_tools = if config.mcp.enabled {
            runtime.agent_mcp_tools(&agent).await
        } else {
            Vec::new()
        };
        let policy_context = super::MaiPolicyContext {
            can_manage_agents: crate::turn::tool_visibility::can_manage_agents(
                &runtime.state,
                &agent,
            )
            .await,
        };
        let configured_roles = config.models.routes.keys().cloned().collect::<Vec<_>>();
        let initial_visibility = super::compile_execution_policy(
            &context.snapshot,
            configured_roles.clone(),
            crate::turn::tool_visibility::visible_tool_names(&agent, &initial_mcp_tools).await,
            policy_context,
        )
        .visible_tools;
        let skill_injections = {
            let _guard = runtime.project_skill_read_guard(&agent).await;
            skills_manager.build_injections_for_input(
                SkillInput {
                    text: Some(&context.input.message),
                    selections: skill_mentions(&context.input.metadata)
                        .into_iter()
                        .map(SkillSelection::from_mention)
                        .collect(),
                    reserved_names: initial_visibility.to_btree_set(),
                },
                &skills_config,
            )?
        };
        let product_session_id = protocol_uuid(context.session_id.as_str());
        let product_turn_id = protocol_uuid(context.turn_id.as_str());
        if !skill_injections.items.is_empty() {
            runtime
                .events
                .publish(mai_protocol::ServiceEventKind::SkillsActivated {
                    agent_id: product_agent_id,
                    session_id: Some(product_session_id),
                    turn_id: product_turn_id,
                    skills: crate::instructions::skill_activation_info(
                        &skill_injections,
                        &container_skill_paths,
                    ),
                })
                .await;
        }
        if config.mcp.enabled {
            runtime
                .inject_project_mcp_tools(
                    &agent,
                    product_agent_id,
                    product_session_id,
                    &context.cancellation_token,
                )
                .await?;
        }
        let mcp_tools = if config.mcp.enabled {
            runtime.agent_mcp_tools(&agent).await
        } else {
            Vec::new()
        };
        let base_visibility =
            crate::turn::tool_visibility::visible_tool_names(&agent, &mcp_tools).await;
        let policy = super::compile_execution_policy(
            &context.snapshot,
            configured_roles,
            base_visibility,
            policy_context,
        );
        let product_tools = policy
            .visible_tools
            .filter_schemas(crate::turn::product_tool_schemas::build_tool_schemas());
        let mcp_tool_schemas = pl_core::host_mcp_tool_schemas(
            mcp_tools
                .iter()
                .filter(|tool| policy.visible_tools.contains(&tool.model_name))
                .map(host_mcp_tool_spec),
        );
        let generated_instructions = {
            let _guard = runtime.project_skill_read_guard(&agent).await;
            runtime
                .build_instructions(
                    &agent,
                    &skills_manager,
                    &skill_injections,
                    &skills_config,
                    &mcp_tools,
                    &container_skill_paths,
                )
                .await?
        };
        let workspace_instructions = runtime
            .project_review_workspace_instructions_for_agent(&agent)
            .await?;
        let instructions = instructions_for_turn(
            &config,
            Some(&generated_instructions),
            None,
            workspace_instructions.as_deref(),
        );
        let profile = CoreAgentProfile::host_provided(
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        )
        .with_context_compaction(context_compaction());
        let kernel = build_mai_framework_kernel(
            builder,
            profile,
            MaiFrameworkKernelBuildContext {
                runtime: runtime.clone(),
                agent,
                agent_id: product_agent_id,
                framework_agent_id: context.snapshot.identity.id.clone(),
                framework_runtime: context.runtime.clone(),
                policy: policy.clone(),
                product_tool_schemas: product_tools,
                mcp_tool_schemas,
                cancellation_token: context.cancellation_token.clone(),
            },
        )
        .await?;
        let request = TurnRequest::new(context.input.message)
            .with_turn_id(context.turn_id.to_string())
            .with_instruction_snapshot(InstructionSnapshot::profile_base_override(
                "mai instructions",
                instructions,
            ));
        let options = TurnOptions::default()
            // mai 的文件和进程工具都在 agent 容器内执行；产品级 effect policy
            // 已经完成授权，因此不能再按 server 主机路径触发人工审批。
            .with_permission_mode(pl_core::PermissionMode::FullAccess)
            .with_prompt_cache_key(format!(
                "agent:{}:session:{}",
                context.snapshot.identity.id, context.session_id
            ))
            .with_interaction_callback(mai_user_input_interaction_callback(
                runtime,
                product_agent_id,
                product_session_id,
                product_turn_id,
            ));
        Ok(PreparedAgentTurn::new(kernel, request, options, policy))
    }
}

pub(crate) async fn product_agent(
    runtime: &AgentRuntime,
    framework_id: &pl_core::AgentId,
) -> Result<(AgentId, Arc<AgentRecord>)> {
    let agents = runtime
        .state
        .agents
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for agent in agents {
        if &*agent.runtime_agent_id.read().await == framework_id {
            let id = agent.summary.read().await.id;
            return Ok((id, agent));
        }
    }
    Err(RuntimeError::InvalidInput(format!(
        "product agent mapping not found for `{framework_id}`"
    )))
}

fn instructions_for_turn(
    config: &MaiConfig,
    generated: Option<&str>,
    system_prompt: Option<&str>,
    workspace_instructions: Option<&str>,
) -> String {
    [
        Some(config.instructions.base.as_str()),
        Some(config.instructions.developer.as_str()),
        Some(config.instructions.user.as_str()),
        generated,
        system_prompt,
        workspace_instructions,
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn apply_skill_policy(
    skills: &mut mai_protocol::SkillsConfigRequest,
    policy: &crate::config::MaiSkillsConfig,
) {
    for entry in &mut skills.config {
        let disabled_by_name = entry
            .name
            .as_ref()
            .is_some_and(|name| policy.disabled.iter().any(|disabled| disabled == name));
        if !policy.enabled || disabled_by_name {
            entry.enabled = false;
        }
    }
}

fn skill_mentions(metadata: &serde_json::Value) -> Vec<String> {
    metadata
        .get("skillMentions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect()
}

fn context_compaction() -> ContextCompactionConfig {
    ContextCompactionConfig::new(
        crate::COMPACT_PROMPT,
        crate::COMPACT_PROMPT,
        crate::COMPACT_SUMMARY_PREFIX,
        "compact response did not include a summary",
    )
    .with_replacement(ContextCompactionReplacement::RecentInteractionTail(
        RecentInteractionTailConfig {
            max_user_chars: crate::COMPACT_USER_MESSAGE_MAX_CHARS,
            max_assistant_chars: 8_000,
            max_tool_output_chars: 4_000,
            assistant_items: 2,
            tool_output_items: 3,
        },
    ))
    .with_openai_mode(OpenAiCompactionMode::Local)
}

fn host_mcp_tool_spec(tool: &crate::mcp::McpTool) -> HostMcpToolSpec {
    HostMcpToolSpec {
        model_name: tool.model_name.clone(),
        server: tool.server.clone(),
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
    }
}
