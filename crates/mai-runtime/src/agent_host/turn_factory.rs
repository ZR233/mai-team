use std::sync::{Arc, Weak};

use mai_protocol::AgentId;
use pl_core::{
    AgentTurnFactory, AgentTurnPreparationContext, AgentWorkspace, ContextCompactionConfig,
    ContextCompactionReplacement, CoreRuntimeProfile, PreparedAgentTurn, PreparedSessionRuntime,
    RecentInteractionTailConfig, TurnBudget, TurnEngineBuilder, TurnOptions, TurnRequest,
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
        let frozen_profile = context.session.agent_profile().cloned();
        let route = super::resolve_route(
            &config.models,
            &context.snapshot.identity.role,
            frozen_profile.as_ref(),
        )?;
        let agent_profiles = super::agent_profiles(&config.models)?;
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
        let review_context = agent.review_context.read().await.clone();
        let review_mode = if review_context.is_some() {
            Some(
                resolve_review_mode_snapshot(&skill_catalog, context.cancellation_token.clone())
                    .await?,
            )
        } else {
            None
        };
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
        let configured_roles = agent_profiles
            .iter()
            .map(|profile| pl_core::AgentRoleId::new(profile.profile_id.clone()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(RuntimeError::Model)?;
        let policy =
            super::compile_execution_policy(&context.snapshot, configured_roles, policy_context);
        let system_prompt = frozen_profile
            .as_ref()
            .map(|profile| profile.system_instructions.as_str())
            .or(agent.system_prompt.as_deref());
        let generated_instructions =
            crate::instructions::build_instructions(system_prompt, &mcp_tools);
        let workspace_instructions = runtime
            .project_review_workspace_instructions_for_agent(&agent)
            .await?;
        let review_manifest = review_context
            .as_deref()
            .map(|context| super::review_manifest::section(context, &skill_load.activations))
            .transpose()?;
        let mut instruction_profile =
            InstructionProfile::new().with_developer_block("mai runtime", generated_instructions);
        if let Some(mode) = &review_mode {
            instruction_profile = instruction_profile
                .with_developer_block(format!("PL Mode {}", mode.mode_id), mode.content.clone());
        }
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
        let product_has_project = agent.summary.read().await.project_id.is_some();
        let workspace_root = if product_has_project {
            crate::projects::workspace::AGENT_WORKSPACE_REPO_PATH
        } else {
            "/workspace"
        };
        let agent_workspace = resolve_agent_workspace(
            context.snapshot.identity.parent_id.is_some(),
            workspace_root,
            context.session.workspace_assignment(),
        )?;
        let mut profile = CoreRuntimeProfile::minimal()
            .with_agent_workspace(agent_workspace.clone())
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
                            runtime
                                .handle()
                                .acquire_turn_lease()
                                .await?
                                .agent_tools(None)
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
                workspace: agent_workspace,
                profiles: agent_profiles,
                agent_tools,
                skill_catalog: engine_skill_catalog,
                exclusive_web_search,
                collaboration: if review_mode.is_some() {
                    crate::turn::core_adapter::CollaborationAvailability::Disabled
                } else {
                    crate::turn::core_adapter::CollaborationAvailability::Enabled
                },
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
        if review_mode.is_some() {
            prepared =
                prepared.with_budget(TurnBudget::new(crate::projects::review::REVIEW_TURN_BUDGET));
        }
        if let Some(review_manifest) = review_manifest {
            prepared = prepared.with_pinned_context(review_manifest);
        }
        Ok(prepared)
    }
}

async fn resolve_review_mode_snapshot(
    catalog: &pl_core::skill::FrozenSkillCatalog,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<pl_protocol::ModeInstructionSnapshot> {
    let metadata = catalog
        .find_mode(crate::skills::REVIEW_MODE_ID)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "required mai Review Mode is unavailable in the frozen PL catalog".to_string(),
            )
        })?;
    let definition = catalog
        .load(
            crate::skills::REVIEW_MODE_ID,
            pl_core::skill::SkillLoadInvocation::Mode,
            cancellation,
        )
        .await
        .map_err(RuntimeError::Model)?;
    let mode = metadata.mode.as_ref().ok_or_else(|| {
        RuntimeError::InvalidInput("required mai Review Mode has no PL mode metadata".to_string())
    })?;
    Ok(pl_protocol::ModeInstructionSnapshot {
        mode_id: definition.summary.name,
        display_name: mode.display_name.clone(),
        source: skill_source_label(definition.summary.source).to_string(),
        provider_id: definition.summary.provider_id.as_str().to_string(),
        revision: definition.revision,
        content_hash: pl_core::canonical_content_hash(definition.content.as_bytes()),
        content: definition.content,
    })
}

fn skill_source_label(source: pl_core::skill::SkillSourceKind) -> &'static str {
    match source {
        pl_core::skill::SkillSourceKind::Project => "project",
        pl_core::skill::SkillSourceKind::User => "user",
        pl_core::skill::SkillSourceKind::System => "system",
        pl_core::skill::SkillSourceKind::External => "external",
    }
}

fn resolve_agent_workspace(
    is_child: bool,
    product_root: &str,
    assignment: Option<&pl_protocol::AgentWorkspaceAssignmentSnapshot>,
) -> Result<AgentWorkspace> {
    if !is_child {
        return Ok(AgentWorkspace::confined(
            product_root,
            pl_core::WorkspaceMutability::ReadWrite,
        ));
    }
    let assignment = assignment.ok_or_else(|| {
        RuntimeError::InvalidInput("child Agent has no frozen workspace assignment".to_string())
    })?;
    if assignment.project_root != product_root {
        return Err(RuntimeError::InvalidInput(format!(
            "child Agent project root `{}` does not match product root `{product_root}`",
            assignment.project_root
        )));
    }
    match assignment.mode {
        pl_protocol::AgentWorkspaceMode::Unrestricted => {
            if assignment.root != product_root
                || assignment.writable_paths.is_some()
                || assignment.worktree.is_some()
            {
                return Err(RuntimeError::InvalidInput(
                    "unrestricted child Agent has an invalid workspace assignment".to_string(),
                ));
            }
            Ok(AgentWorkspace::local(product_root))
        }
        pl_protocol::AgentWorkspaceMode::Directory => {
            if assignment.root != product_root || assignment.worktree.is_some() {
                return Err(RuntimeError::InvalidInput(
                    "directory child Agent has an invalid workspace assignment".to_string(),
                ));
            }
            Ok(AgentWorkspace::directory(
                product_root,
                assignment.writable_paths.as_ref().map(|paths| {
                    paths
                        .iter()
                        .map(std::path::PathBuf::from)
                        .collect::<Vec<_>>()
                }),
            ))
        }
        pl_protocol::AgentWorkspaceMode::Worktree => Err(RuntimeError::InvalidInput(
            "mai does not advertise worktree Agent Profiles".to_string(),
        )),
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

fn skill_mentions(metadata: &pl_core::MailboxMetadata) -> Vec<String> {
    metadata
        .get("skillMentions")
        .and_then(pl_core::MailboxMetadataValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(pl_core::MailboxMetadataValue::as_str)
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
