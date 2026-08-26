use std::sync::{Arc, Weak};

use mai_protocol::AgentId;
use pl_core::{
    AgentTurnFactory, AgentTurnPreparationContext, AgentWorkspace, ContextCompactionConfig,
    ContextCompactionReplacement, CoreRuntimeProfile, PreparedAgentTurn, PreparedSessionRuntime,
    RecentInteractionTailConfig, TurnEngineBuilder, TurnOptions, TurnRequest,
    instruction::InstructionProfile,
};
use pl_model::OpenAiCompactionMode;
use tokio::sync::RwLock;

use crate::state::AgentRecord;
use crate::turn::core_adapter::{MaiFrameworkKernelBuildContext, build_mai_turn_engine};
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
        let exclusive_web_search =
            web_search.visibility == pl_core::ToolVisibilityConstraint::Exclusive;
        let mut builder = TurnEngineBuilder::from_route(&route).map_err(RuntimeError::Model)?;
        let agent_tools = runtime.tool_sets.get(product_agent_id).await;

        if let Err(error) = runtime.refresh_project_skills_for_agent(&agent).await {
            tracing::warn!(agent_id = %product_agent_id, "failed to refresh project skills: {error}");
        }
        let skills_config = runtime.deps.store.load_skills_config().await?;
        let skill_catalog_service = runtime.skill_catalog_for_agent(&agent).await?;
        let project_skill_guard = runtime.project_skill_read_guard(&agent).await;
        let skill_catalog = skill_catalog_service
            .discover(
                &skills_config,
                &config.skills,
                context.cancellation_token.clone(),
            )
            .await?;
        let mut skills_response =
            skill_catalog_service.project(&skill_catalog, &skills_config, &config.skills);
        if let Some(project_id) = agent.summary.read().await.project_id {
            runtime
                .apply_project_skill_source_paths_for_agent(
                    &agent,
                    project_id,
                    &mut skills_response,
                )
                .await;
        }
        runtime
            .sync_agent_skills_to_container(&agent, &skills_response)
            .await?;
        let skill_load = if config.skills.enabled && !exclusive_web_search {
            skill_catalog
                .load_user_invocations_with_selections(
                    &context.input.payload.message,
                    &skill_mentions(&context.input.payload.metadata),
                    context.turn_id.as_str(),
                    context.cancellation_token.clone(),
                )
                .await
                .map_err(RuntimeError::Model)?
        } else {
            pl_core::skill::SkillUserInvocationLoad::default()
        };
        drop(project_skill_guard);
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
            can_manage_agents: super::policy::can_manage_agents(&runtime.state, &agent).await,
        };
        let configured_roles = config.models.routes.keys().cloned().collect::<Vec<_>>();
        let policy =
            super::compile_execution_policy(&context.snapshot, configured_roles, policy_context);
        let generated_instructions =
            crate::instructions::build_instructions(agent.system_prompt.as_deref(), &mcp_tools);
        let workspace_instructions = runtime
            .project_review_workspace_instructions_for_agent(&agent)
            .await?;
        let review_manifest = agent
            .review_context
            .read()
            .await
            .as_deref()
            .map(|context| super::review_manifest::section(context, &skill_load.activations))
            .transpose()?;
        let mut instruction_profile =
            InstructionProfile::new().with_developer_block("mai runtime", generated_instructions);
        if !config.instructions.base.trim().is_empty() {
            instruction_profile =
                instruction_profile.with_base_system_prompt(config.instructions.base.clone());
        }
        if !config.instructions.developer.trim().is_empty() {
            instruction_profile = instruction_profile.with_developer_block(
                "mai config developer",
                config.instructions.developer.clone(),
            );
        }
        if !config.instructions.user.trim().is_empty() {
            instruction_profile = instruction_profile
                .with_user_context_block("mai config user", config.instructions.user.clone());
        }
        let workspace_root = if agent.summary.read().await.project_id.is_some() {
            crate::projects::workspace::AGENT_WORKSPACE_REPO_PATH
        } else {
            "/workspace"
        };
        let mut profile = CoreRuntimeProfile::minimal()
            .with_agent_workspace(AgentWorkspace::confined(
                workspace_root,
                pl_core::WorkspaceMutability::ReadWrite,
            ))
            .with_instruction_profile(instruction_profile)
            .with_context_compaction(context_compaction());
        if let Some(workspace_instructions) = workspace_instructions {
            profile = profile.with_workspace_instructions(workspace_instructions);
        }
        let engine_skill_catalog =
            (config.skills.enabled && !exclusive_web_search).then(|| skill_catalog.clone());
        if let Some(catalog) = &engine_skill_catalog {
            builder = builder.with_skill_catalog(catalog.clone());
        }
        if config.mcp.enabled && !exclusive_web_search {
            let refresh_agent = agent.clone();
            builder =
                builder.with_before_model_step(pl_core::BeforeModelStepHook::new(move |step| {
                    let refresh_agent = refresh_agent.clone();
                    async move {
                        let tools = if let Some(runtime) = refresh_agent.mcp.read().await.clone() {
                            runtime.handle().acquire_turn_lease().await?.agent_tools()
                        } else {
                            Vec::new()
                        };
                        step.agent_tools
                            .install(pl_core::ToolGroupId::new("mcp"), tools)
                    }
                }));
        } else {
            agent_tools.uninstall(&pl_core::ToolGroupId::new("mcp"));
        }
        let mut engine = build_mai_turn_engine(
            builder,
            profile,
            MaiFrameworkKernelBuildContext {
                runtime: runtime.clone(),
                agent,
                agent_id: product_agent_id,
                framework_agent_id: context.snapshot.identity.id.clone(),
                framework_runtime: context.runtime.clone(),
                policy: policy.clone(),
                agent_tools,
                skill_catalog: engine_skill_catalog,
                exclusive_web_search,
            },
        )
        .await?;
        web_search.install(&mut engine, &config.web_search)?;
        let mut request = TurnRequest::new(context.input.payload.message)
            .with_turn_id(context.turn_id.to_string())
            .with_skill_activations(skill_load.activations);
        if let Some(instruction) = skill_load.instruction {
            request = request.with_skill_invocation_instruction(instruction);
        }
        let options = TurnOptions::default()
            // mai 的文件和进程工具都在 agent 容器内执行；产品级 effect policy
            // 已经完成授权，因此不能再按 server 主机路径触发人工审批。
            .with_permission_mode(pl_core::PermissionMode::FullAccess)
            .with_prompt_cache_namespace(context.thread_id.to_string())
            .with_user_input_end_turn();
        let mut session_runtime = PreparedSessionRuntime::new(route.model.slug.clone())
            .with_mcp_servers(active_mcp_servers);
        if let Some(context_window) = route.model.resolved_context_window() {
            session_runtime = session_runtime.with_context_window(context_window);
        }
        if let Some(mcp_health) = mcp_health {
            session_runtime = session_runtime.with_mcp_health(mcp_health);
        }
        let mut prepared = PreparedAgentTurn::new(engine, request, options, policy)
            .with_session_runtime(session_runtime);
        if let Some(review_manifest) = review_manifest {
            prepared = prepared.with_pinned_context(review_manifest);
        }
        Ok(prepared)
    }
}

pub(crate) async fn product_agent(
    runtime: &AgentRuntime,
    framework_id: &pl_core::ThreadId,
) -> Result<(AgentId, Arc<AgentRecord>)> {
    let id = framework_id.as_str().parse::<AgentId>().map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "invalid canonical thread id `{framework_id}`: {error}"
        ))
    })?;
    runtime.agent(id).await.map(|agent| (id, agent))
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
        output_schema: tool.output_schema.clone(),
    }
}
