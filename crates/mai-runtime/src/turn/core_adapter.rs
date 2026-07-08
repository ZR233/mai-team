use std::collections::HashSet;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentStatus, MessageRole, ServiceEventKind, SessionId, ToolDefinition, TurnId,
    TurnStatus,
};
use pl_core::{
    AgentKernel, CompileMode, ContextCompactionConfig, ContextCompactionReplacement,
    CoreAgentProfile, CoreSession, InstructionBlock, InstructionSnapshot, InstructionSource,
    InstructionSourceKind, PureCoreBuilder, ReasoningEffort, RecentInteractionTailConfig,
    TraceRecorder, TurnOptions, TurnRequest, TurnResultStatus,
};
use pl_trace::AgentEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::state::AgentRecord;
use crate::turn::completion::TurnResult;
use crate::{AgentRuntime, ModelClient, Result, RuntimeError, completion_response_usage};

pub(crate) struct PureCoreTurnContext {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) agent: Arc<AgentRecord>,
    pub(crate) agent_id: AgentId,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) message: String,
    pub(crate) provider_selection: mai_store::ProviderSelection,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) instructions: String,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) product_tools: Vec<ToolDefinition>,
    pub(crate) history: Vec<pl_protocol::Message>,
    pub(crate) cancellation_token: CancellationToken,
}

pub(crate) async fn run_pure_core_turn(ctx: PureCoreTurnContext) -> Result<()> {
    let provider = ModelClient::provider_for_selection(&ctx.provider_selection)?;
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let runtime_profile = CoreAgentProfile::host_provided(workspace_root).with_context_compaction(
        ContextCompactionConfig::new(
            ctx.instructions.clone(),
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
        )),
    );
    let mut builder = PureCoreBuilder::new(provider);
    if let Some(effort) = ctx.reasoning_effort.as_deref() {
        builder = builder.with_reasoning_effort(ReasoningEffort::new(effort));
    }
    let product_tool_registry = super::product_tools::MaiProductToolRegistry::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.product_tools.clone(),
        ctx.cancellation_token.clone(),
    );
    let mut kernel = AgentKernel::builder(builder)
        .with_profile(runtime_profile)
        .with_registered_tools(product_tool_registry.registered_tools())
        .build()
        .await;
    register_native_shared_tools(&mut kernel, &ctx).await?;

    let mut session = CoreSession::from_messages(ctx.history.clone());
    let (event_tx, event_rx) = tokio::sync::broadcast::channel(64);
    let event_projector = tokio::spawn(project_agent_events(
        ctx.runtime.clone(),
        ctx.agent_id,
        ctx.session_id,
        ctx.turn_id,
        event_rx,
    ));
    let mut recorder = TraceRecorder::new(ctx.session_id.to_string(), event_tx, 0);
    let request = TurnRequest::new(ctx.message.clone(), CompileMode::Auto)
        .with_turn_id(ctx.turn_id.to_string())
        .with_instruction_snapshot(raw_instruction_snapshot(ctx.instructions.clone()));
    let options = TurnOptions::default()
        .with_cancellation(ctx.cancellation_token.clone())
        .with_prompt_cache_key(format!("agent:{}:session:{}", ctx.agent_id, ctx.session_id))
        .with_interaction_callback(mai_user_input_interaction_callback(
            ctx.runtime.clone(),
            ctx.agent_id,
            ctx.session_id,
            ctx.turn_id,
        ));
    let turn_result = kernel
        .run_turn_with_trace(&mut session, request, &mut recorder, options)
        .await;
    let result = match turn_result {
        Ok(result) => result,
        Err(error) => {
            event_projector.abort();
            return Err(error.into());
        }
    };
    drop(recorder);
    let _ = event_projector.await;
    super::kernel_tools::project_tool_trace_events(
        &ctx.runtime,
        ctx.agent_id,
        ctx.session_id,
        ctx.turn_id,
        &result.trace_events,
    )
    .await;
    let compaction_snapshot = result.context_compactions.last().cloned();
    super::history::replace_session_history(
        ctx.runtime.deps.store.as_ref(),
        &ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        session.messages().to_vec(),
    )
    .await?;
    if let Some(snapshot) = compaction_snapshot {
        record_context_compacted(&ctx, &snapshot.summary, snapshot.tokens_before).await;
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
    if !result.content.trim().is_empty() {
        record_assistant_message(&ctx, result.content.clone()).await?;
    }

    let return_error = match result.status {
        TurnResultStatus::Completed => None,
        TurnResultStatus::Aborted
            if result.abort_reason == Some(pl_core::TurnAbortReason::Interrupted) =>
        {
            Some(RuntimeError::TurnCancelled)
        }
        TurnResultStatus::Aborted | TurnResultStatus::Errored => Some(RuntimeError::InvalidInput(
            result
                .error
                .clone()
                .unwrap_or_else(|| "pl-core turn failed".to_string()),
        )),
    };
    let (turn_status, agent_status, error) = match result.status {
        TurnResultStatus::Completed => (TurnStatus::Completed, AgentStatus::Completed, None),
        TurnResultStatus::Aborted => match result.abort_reason {
            Some(pl_core::TurnAbortReason::Interrupted) => (
                TurnStatus::Cancelled,
                AgentStatus::Cancelled,
                result.error.clone(),
            ),
            Some(pl_core::TurnAbortReason::BudgetLimited)
            | Some(pl_core::TurnAbortReason::Shutdown)
            | Some(pl_core::TurnAbortReason::ProviderError)
            | Some(pl_core::TurnAbortReason::ToolError)
            | None => (
                TurnStatus::Failed,
                AgentStatus::Failed,
                result.error.clone(),
            ),
        },
        TurnResultStatus::Errored => (
            TurnStatus::Failed,
            AgentStatus::Failed,
            result.error.clone(),
        ),
    };
    super::completion::finish_turn(
        ctx.runtime.deps.store.as_ref(),
        &ctx.runtime.events,
        &ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        TurnResult {
            turn_id: ctx.turn_id,
            status: turn_status,
            agent_status,
            final_text: (!result.content.trim().is_empty()).then_some(result.content),
            error,
        },
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

pub(crate) fn mai_user_input_interaction_callback(
    runtime: Arc<AgentRuntime>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
) -> pl_core::InteractionCallback {
    Arc::new(move |interaction| {
        let runtime = runtime.clone();
        Box::pin(async move {
            match interaction.payload {
                pl_protocol::InteractionPayload::UserInput { questions } => {
                    let (header, questions) = user_input_questions_from_pl(questions);
                    runtime
                        .events
                        .publish(ServiceEventKind::UserInputRequested {
                            agent_id,
                            session_id: Some(session_id),
                            turn_id,
                            header,
                            questions,
                        })
                        .await;
                    pl_protocol::InteractionResolution::UserInput {
                        answers: Default::default(),
                    }
                }
                pl_protocol::InteractionPayload::ToolApproval { .. } => {
                    pl_protocol::InteractionResolution::ToolApproval {
                        decision: pl_protocol::ToolApprovalResolution::Denied,
                        reason: Some(
                            "mai-team user input callback does not approve tools".to_string(),
                        ),
                    }
                }
                pl_protocol::InteractionPayload::PlanConfirmation { .. } => {
                    pl_protocol::InteractionResolution::PlanConfirmation {
                        decision: pl_protocol::PlanConfirmationResolution::Dismiss,
                        content: None,
                        reason: Some(
                            "mai-team user input callback does not confirm plans".to_string(),
                        ),
                    }
                }
            }
        })
    })
}

fn user_input_questions_from_pl(
    questions: Vec<pl_protocol::UserQuestion>,
) -> (String, Vec<mai_protocol::UserInputQuestion>) {
    let mut header = None;
    let mut projected = Vec::with_capacity(questions.len());
    for question in questions {
        if header.is_none() {
            let value = question.header.trim();
            if !value.is_empty() {
                header = Some(value.to_string());
            }
        }
        projected.push(mai_protocol::UserInputQuestion {
            id: question.id,
            question: question.question,
            options: question
                .options
                .unwrap_or_default()
                .into_iter()
                .map(|option| mai_protocol::UserInputOption {
                    label: option.label,
                    description: Some(option.description),
                })
                .collect(),
        });
    }
    (header.unwrap_or_else(|| "Input".to_string()), projected)
}

async fn register_native_shared_tools(
    kernel: &mut AgentKernel,
    ctx: &PureCoreTurnContext,
) -> Result<()> {
    let backend = Arc::new(super::container::MaiContainerBackend::new(
        ctx.runtime.clone(),
        ctx.agent_id,
    ));
    let git_runtime =
        crate::tools::git::native_git_tool_runtime(ctx.runtime.clone(), &ctx.agent, |name| {
            ctx.tools.iter().any(|tool| tool.name == name)
        })
        .await?;
    let capabilities = pl_core::ToolCapabilityConfig {
        bash: false,
        workspace_files: true,
        skills: false,
        mcp: true,
        lsp: false,
        subagents: true,
        ask_user: true,
        git: git_runtime.is_some(),
        docker: false,
        container: true,
    };
    let visible_tools = ctx
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<HashSet<_>>();
    let mcp_backend = Arc::new(super::mcp_resources::MaiMcpResourceBackend::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.cancellation_token.clone(),
    ));
    let agent_control_backend = Arc::new(super::agent_control::MaiAgentControlBackend::new(
        ctx.runtime.clone(),
        ctx.agent.clone(),
        ctx.agent_id,
        ctx.cancellation_token.clone(),
    ));
    let tool_set = pl_core::ToolSetBuilder::from_capabilities(capabilities)
        .with_allowed_tools(visible_tools.iter().cloned())
        .with_container_tools(backend)
        .with_mcp_resource_tools(mcp_backend)
        .with_agent_control_tools(agent_control_backend);
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if let Some(git_runtime) = git_runtime {
        tool_set
            .with_git_tools(
                git_runtime.config,
                git_runtime.backend,
                git_runtime.credential_provider,
            )
            .register(kernel.core_mut(), workspace_root, None)
            .await;
    } else {
        tool_set
            .register(kernel.core_mut(), workspace_root, None)
            .await;
    }
    Ok(())
}

pub(crate) async fn project_agent_events(
    runtime: Arc<AgentRuntime>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    mut event_rx: broadcast::Receiver<AgentEvent>,
) {
    loop {
        match event_rx.recv().await {
            Ok(AgentEvent::TodoListUpdated { snapshot }) => {
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
            Ok(AgentEvent::Done | AgentEvent::Error { .. }) => break,
            Ok(
                AgentEvent::TracePartStarted { .. }
                | AgentEvent::TracePartDelta { .. }
                | AgentEvent::TracePartCompleted { .. }
                | AgentEvent::TracePartFailed { .. }
                | AgentEvent::InteractionChanged { .. }
                | AgentEvent::AgentStateChanged { .. }
                | AgentEvent::AgentRuntimeUpdated { .. }
                | AgentEvent::SkillActivated { .. }
                | AgentEvent::SubAgentActivity { .. }
                | AgentEvent::TurnInterrupted { .. }
                | AgentEvent::TurnBudgetLimited { .. },
            ) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

fn todo_item_from_pl(item: pl_protocol::TodoItem) -> mai_protocol::TodoItem {
    mai_protocol::TodoItem {
        step: item.step,
        status: match item.status {
            pl_protocol::TodoStatus::Pending => mai_protocol::TodoListStatus::Pending,
            pl_protocol::TodoStatus::InProgress => mai_protocol::TodoListStatus::InProgress,
            pl_protocol::TodoStatus::Completed => mai_protocol::TodoListStatus::Completed,
        },
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
            summary_preview: preview(summary, crate::COMPACT_SUMMARY_PREVIEW_CHARS),
        })
        .await;
}

fn preview(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut chars = trimmed.chars().take(max_chars).collect::<String>();
    chars.push_str("...");
    chars
}

fn raw_instruction_snapshot(instructions: String) -> InstructionSnapshot {
    InstructionSnapshot {
        base: InstructionBlock {
            source: InstructionSource {
                kind: InstructionSourceKind::ProfileBaseOverride,
                label: "mai-team instructions".to_string(),
                path: None,
            },
            content: instructions,
        },
        developer: Vec::new(),
        user: Vec::new(),
    }
}
