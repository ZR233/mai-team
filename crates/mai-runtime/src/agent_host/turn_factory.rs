use std::sync::{Arc, Weak};

use mai_protocol::AgentId;
use pl_core::{
    AgentTurnFactory, AgentTurnPreparationContext, ContextCompactionConfig,
    ContextCompactionReplacement, CoreAgentProfile, InstructionSnapshot, PreparedAgentTurn,
    PreparedSessionRuntime, RecentInteractionTailConfig, TurnEngineBuilder, TurnOptions,
    TurnRequest,
};
use pl_model::{OpenAiCompactionMode, create_provider_with_catalog};
use tokio::sync::RwLock;

use crate::skills::{SkillInput, SkillSelection};
use crate::state::AgentRecord;
use crate::turn::core_adapter::{
    MaiFrameworkKernelBuildContext, build_mai_framework_kernel, mai_user_input_interaction_callback,
};
use crate::{AgentRuntime, MaiConfig, Result, RuntimeError};

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
        let web_search = pl_core::plan_web_search(&config.models, &route, &config.web_search)?;
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
        let mcp_lease = runtime.prepare_agent_mcp_lease(&agent, &config).await?;
        let active_mcp_servers = mcp_lease
            .as_ref()
            .map_or_else(Vec::new, |lease| lease.server_ids().to_vec());
        let mcp_health = if config.mcp.enabled {
            match agent.mcp.read().await.clone() {
                Some(runtime) => Some(runtime.handle().health_snapshot().await?),
                None => None,
            }
        } else {
            None
        };
        let mcp_tools: Vec<crate::mcp::McpTool> = mcp_lease
            .as_ref()
            .map(|lease| lease.tools().iter().map(mcp_tool).collect::<Vec<_>>())
            .unwrap_or_default();
        let policy_context = super::MaiPolicyContext {
            can_manage_agents: crate::turn::tool_visibility::can_manage_agents(
                &runtime.state,
                &agent,
            )
            .await,
        };
        let configured_roles = config.models.routes.keys().cloned().collect::<Vec<_>>();
        let mut initial_policy = super::compile_execution_policy(
            &context.snapshot,
            configured_roles.clone(),
            crate::turn::tool_visibility::visible_tool_names(&agent, &mcp_tools).await,
            policy_context,
        );
        initial_policy.visible_tools =
            web_search.constrain_visibility(initial_policy.visible_tools);
        let initial_visibility = initial_policy.visible_tools;
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
        let mut policy = super::compile_execution_policy(
            &context.snapshot,
            configured_roles,
            crate::turn::tool_visibility::visible_tool_names(&agent, &mcp_tools).await,
            policy_context,
        );
        policy.visible_tools = web_search.constrain_visibility(policy.visible_tools);
        let product_tools = policy
            .visible_tools
            .filter_schemas(crate::turn::product_tool_schemas::build_tool_schemas());
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
        let review_manifest = agent
            .review_context
            .read()
            .await
            .as_deref()
            .map(|context| super::review_manifest::section(context, &skill_injections))
            .transpose()?;
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
        let mut kernel = build_mai_framework_kernel(
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
                mcp_lease,
            },
        )
        .await?;
        web_search.install(kernel.core_mut(), &config.web_search)?;
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
            .with_interaction_callback(mai_user_input_interaction_callback());
        let mut session_runtime = PreparedSessionRuntime::new(route.model.slug.clone())
            .with_mcp_servers(active_mcp_servers);
        if let Some(context_window) = route.model.resolved_context_window() {
            session_runtime = session_runtime.with_context_window(context_window);
        }
        if let Some(mcp_health) = mcp_health {
            session_runtime = session_runtime.with_mcp_health(mcp_health);
        }
        let mut prepared = PreparedAgentTurn::new(kernel, request, options, policy)
            .with_session_runtime(session_runtime);
        if let Some(review_manifest) = review_manifest {
            prepared = prepared.with_pinned_context(review_manifest);
        }
        Ok(prepared)
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

fn mcp_tool(tool: &pl_core::McpRuntimeToolDescriptor) -> crate::mcp::McpTool {
    crate::mcp::McpTool {
        server: tool.server_id.clone(),
        name: tool.raw_name.clone(),
        model_name: tool.exposed_name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
        output_schema: None,
    }
}
