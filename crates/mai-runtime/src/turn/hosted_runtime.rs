use std::sync::Arc;

use mai_protocol::{AgentId, MessageRole, ServiceEventKind, SessionId, TodoItem, TurnId};
use pl_core::{
    AgentKernel, CompileMode, ContextCompactionConfig, ContextCompactionReplacement,
    CoreAgentProfile, HostedAgentRunError, HostedAgentRunner, HostedAgentRuntime,
    HostedTurnCompletion, HostedTurnPreparation, HostedTurnRequest, InstructionSnapshot,
    PureCoreBuilder, ReasoningEffort, RecentInteractionTailConfig, TurnOptions, TurnRequest,
};
use pl_model::OpenAiCompactionMode;
use pl_trace::AgentEvent;
use uuid::Uuid;

use crate::{
    AgentRuntime, Result, RuntimeError, completion_response_usage, core_provider_for_selection,
};

use super::core_adapter::{
    MaiAgentKernelBuildContext, PureCoreTurnContext, build_mai_agent_kernel,
    mai_user_input_interaction_callback,
};
use super::outcome::CoreTurnOutcome;

pub(crate) async fn run_hosted_agent_turn(ctx: PureCoreTurnContext) -> Result<()> {
    let request = HostedTurnRequest::new(ctx.session_id.to_string(), ctx.turn_id.to_string());
    HostedAgentRunner::new(MaiHostedAgentRuntime { ctx: Arc::new(ctx) })
        .run(request)
        .await
        .map_err(runtime_error_from_hosted)
}

#[derive(Clone)]
struct MaiHostedAgentRuntime {
    ctx: Arc<PureCoreTurnContext>,
}

impl HostedAgentRuntime for MaiHostedAgentRuntime {
    type Error = RuntimeError;

    fn prepare_turn(
        &self,
        request: HostedTurnRequest,
    ) -> impl std::future::Future<Output = Result<HostedTurnPreparation>> + Send {
        let ctx = self.ctx.clone();
        async move {
            let provider = core_provider_for_selection(&ctx.provider_selection)?;
            let workspace_root =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let workspace_instructions = ctx
                .workspace_instructions
                .as_ref()
                .map(|instructions| instructions.trim())
                .filter(|instructions| !instructions.is_empty())
                .map(ToOwned::to_owned);
            let instructions = instructions_with_workspace_overlay(
                &ctx.instructions,
                workspace_instructions.as_deref(),
            );
            let runtime_profile = CoreAgentProfile::host_provided(workspace_root)
                .with_context_compaction(
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
                    .with_openai_mode(OpenAiCompactionMode::Local),
                );
            let mut builder = PureCoreBuilder::new(provider);
            if let Some(effort) = ctx.reasoning_effort.as_deref() {
                builder = builder.with_reasoning_effort(ReasoningEffort::new(effort));
            }
            let kernel = build_mai_hosted_agent_kernel(&ctx, builder, runtime_profile).await?;
            let turn_request = TurnRequest::new(ctx.message.clone(), CompileMode::Simple)
                .with_turn_id(ctx.turn_id.to_string())
                .with_instruction_snapshot(raw_instruction_snapshot(instructions));
            let options = TurnOptions::default()
                .with_cancellation(ctx.cancellation_token.clone())
                .with_prompt_cache_key(format!("agent:{}:session:{}", ctx.agent_id, ctx.session_id))
                .with_interaction_callback(mai_user_input_interaction_callback(
                    ctx.runtime.clone(),
                    ctx.agent_id,
                    ctx.session_id,
                    ctx.turn_id,
                ));
            Ok(HostedTurnPreparation::new(
                request,
                kernel,
                ctx.history.clone(),
                turn_request,
                options,
            ))
        }
    }

    fn handle_event(
        &self,
        event: AgentEvent,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        let ctx = self.ctx.clone();
        async move {
            project_hosted_agent_event(
                ctx.runtime.as_ref(),
                ctx.agent_id,
                ctx.session_id,
                ctx.turn_id,
                event,
            )
            .await;
            Ok(())
        }
    }

    fn complete_turn(
        &self,
        completion: HostedTurnCompletion,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        let ctx = self.ctx.clone();
        async move { complete_hosted_turn(&ctx, completion).await }
    }
}

pub(crate) async fn project_hosted_agent_event(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    event: AgentEvent,
) {
    if let AgentEvent::TodoListUpdated { snapshot } = event {
        runtime
            .events
            .publish(ServiceEventKind::TodoListUpdated {
                agent_id,
                session_id: Some(session_id),
                turn_id,
                items: snapshot.items.into_iter().map(todo_item_from_pl).collect(),
            })
            .await;
    }
}

async fn build_mai_hosted_agent_kernel(
    ctx: &PureCoreTurnContext,
    builder: PureCoreBuilder,
    runtime_profile: CoreAgentProfile,
) -> Result<AgentKernel> {
    build_mai_agent_kernel(
        builder,
        runtime_profile,
        MaiAgentKernelBuildContext {
            runtime: ctx.runtime.clone(),
            agent: ctx.agent.clone(),
            agent_id: ctx.agent_id,
            visible_tool_names: ctx.visible_tool_names.clone(),
            product_tool_schemas: ctx.product_tools.clone(),
            mcp_tool_schemas: ctx.mcp_tool_schemas.clone(),
            cancellation_token: ctx.cancellation_token.clone(),
        },
    )
    .await
}

async fn complete_hosted_turn(
    ctx: &PureCoreTurnContext,
    completion: HostedTurnCompletion,
) -> Result<()> {
    let (_request, session, result) = completion.into_parts();
    super::kernel_tools::project_tool_trace_events(
        &ctx.runtime,
        ctx.agent_id,
        ctx.session_id,
        ctx.turn_id,
        &result.trace_events,
    )
    .await;
    super::history::replace_session_history(
        ctx.runtime.deps.store.as_ref(),
        &ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        session.messages().to_vec(),
    )
    .await?;
    if let Some(snapshot) = result.context_compactions.last() {
        record_context_compacted(
            ctx,
            snapshot.summary.as_deref().unwrap_or_default(),
            snapshot.tokens_before,
        )
        .await;
    }
    if let Some(last_context_tokens) = result.last_context_tokens {
        super::history::record_session_context_tokens(
            ctx.runtime.deps.store.as_ref(),
            &ctx.agent,
            ctx.agent_id,
            ctx.session_id,
            last_context_tokens,
        )
        .await?;
    }
    if result.usage.total_tokens > 0 {
        super::accounting::record_model_usage(
            ctx.runtime.deps.store.as_ref(),
            &ctx.runtime.events,
            &ctx.agent,
            ctx.agent_id,
            ctx.session_id,
            &completion_response_usage(&result.usage),
        )
        .await?;
    }
    let mut outcome = CoreTurnOutcome::from_result(&result);
    if let Some(final_text) = outcome.final_text() {
        record_assistant_message(ctx, final_text.to_string()).await?;
    }

    let return_error = outcome.take_return_error();
    super::completion::finish_turn(
        ctx.runtime.deps.store.as_ref(),
        &ctx.runtime.events,
        &ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        outcome.into_completion(ctx.turn_id),
    )
    .await?;
    ctx.runtime
        .start_next_queued_input_after_turn(ctx.agent_id)
        .await;
    if let Some(error) = return_error {
        return Err(error);
    }
    Ok(())
}

fn runtime_error_from_hosted(error: HostedAgentRunError<RuntimeError>) -> RuntimeError {
    match error {
        HostedAgentRunError::Prepare(error)
        | HostedAgentRunError::Event(error)
        | HostedAgentRunError::Complete(error) => error,
        HostedAgentRunError::Core(error) => RuntimeError::Model(error),
        HostedAgentRunError::EventTaskJoin(error) => RuntimeError::InvalidInput(error),
    }
}

async fn record_assistant_message(ctx: &PureCoreTurnContext, text: String) -> Result<()> {
    super::history::record_message(
        ctx.runtime.deps.store.as_ref(),
        &ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        MessageRole::Assistant,
        text.clone(),
    )
    .await?;
    let message_id = format!("msg_{}", Uuid::new_v4());
    ctx.runtime
        .events
        .publish(ServiceEventKind::AgentMessageCompleted {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: ctx.turn_id,
            message_id: message_id.clone(),
            role: MessageRole::Assistant,
            channel: "final".to_string(),
            content: text.clone(),
        })
        .await;
    ctx.runtime
        .events
        .publish(ServiceEventKind::AgentMessage {
            agent_id: ctx.agent_id,
            session_id: Some(ctx.session_id),
            turn_id: Some(ctx.turn_id),
            role: MessageRole::Assistant,
            content: text,
        })
        .await;
    Ok(())
}

async fn record_context_compacted(ctx: &PureCoreTurnContext, summary: &str, tokens_before: u64) {
    ctx.runtime
        .events
        .publish(ServiceEventKind::ContextCompacted {
            agent_id: ctx.agent_id,
            session_id: ctx.session_id,
            turn_id: ctx.turn_id,
            tokens_before,
            summary_preview: pl_core::text_preview_chars(
                summary,
                crate::COMPACT_SUMMARY_PREVIEW_CHARS,
            ),
        })
        .await;
}

fn raw_instruction_snapshot(instructions: String) -> InstructionSnapshot {
    InstructionSnapshot::profile_base_override("mai-team instructions", instructions)
}

fn instructions_with_workspace_overlay(base: &str, workspace_instructions: Option<&str>) -> String {
    let Some(workspace_instructions) = workspace_instructions else {
        return base.to_string();
    };
    format!("{base}\n\n# Project Workspace Instructions\n\n{workspace_instructions}")
}

fn todo_item_from_pl(item: pl_protocol::TodoItem) -> TodoItem {
    TodoItem {
        step: item.step,
        status: match item.status {
            pl_protocol::TodoStatus::Pending => mai_protocol::TodoListStatus::Pending,
            pl_protocol::TodoStatus::InProgress => mai_protocol::TodoListStatus::InProgress,
            pl_protocol::TodoStatus::Completed => mai_protocol::TodoListStatus::Completed,
        },
    }
}
