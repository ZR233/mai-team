use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentStatus, MessageRole, ServiceEventKind, SessionId, ToolDefinition, TurnId,
    TurnStatus,
};
use pl_core::{
    CompileMode, ContextCompactionConfig, ContextCompactionReplacement, CoreRuntimeProfile,
    CoreSession, InstructionBlock, InstructionSnapshot, InstructionSource, InstructionSourceKind,
    OutputTruncation, PureCoreBuilder, ReasoningEffort, RecentInteractionTailConfig, Tool,
    ToolContext, ToolInput, ToolOutput, ToolRuntimeEvent, TraceRecorder, TurnOptions, TurnRequest,
    TurnResultStatus,
};
use pl_protocol::PureError;
use serde_json::Value;
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
    pub(crate) history: Vec<pl_protocol::Message>,
    pub(crate) cancellation_token: CancellationToken,
}

pub(crate) async fn run_pure_core_turn(ctx: PureCoreTurnContext) -> Result<()> {
    let provider = ModelClient::provider_for_selection(&ctx.provider_selection)?;
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let runtime_profile = CoreRuntimeProfile::host_provided(workspace_root)
        .with_context_compaction(
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
    let mut builder = PureCoreBuilder::new(provider).with_runtime_profile(runtime_profile);
    if let Some(effort) = ctx.reasoning_effort.as_deref() {
        builder = builder.with_reasoning_effort(ReasoningEffort::new(effort));
    }
    let mut core = builder.build();
    for definition in ctx.tools.iter().cloned() {
        core.register_tool(MaiRuntimeTool::new(
            ctx.runtime.clone(),
            ctx.agent.clone(),
            ctx.agent_id,
            ctx.session_id,
            ctx.turn_id,
            definition,
            ctx.cancellation_token.clone(),
        ));
    }

    let mut session = CoreSession::from_messages(ctx.history.clone());
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel(64);
    let mut recorder = TraceRecorder::new(ctx.session_id.to_string(), event_tx, 0);
    let request = TurnRequest::new(ctx.message.clone(), CompileMode::Auto)
        .with_turn_id(ctx.turn_id.to_string())
        .with_instruction_snapshot(raw_instruction_snapshot(ctx.instructions.clone()));
    let options = TurnOptions::default()
        .with_cancellation(ctx.cancellation_token.clone())
        .with_prompt_cache_key(format!("agent:{}:session:{}", ctx.agent_id, ctx.session_id));
    let result = core
        .run_turn_with_trace(&mut session, request, &mut recorder, options)
        .await?;
    let compacted_summary = new_compaction_summary(&ctx.history, session.messages());

    super::history::replace_session_history(
        ctx.runtime.deps.store.as_ref(),
        &ctx.agent,
        ctx.agent_id,
        ctx.session_id,
        session.messages().to_vec(),
    )
    .await?;
    if let Some(summary) = compacted_summary {
        record_context_compacted(&ctx, &summary, result.usage.total_tokens).await;
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

#[derive(Clone)]
struct MaiRuntimeTool {
    runtime: Arc<AgentRuntime>,
    agent: Arc<AgentRecord>,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    definition: ToolDefinition,
    cancellation_token: CancellationToken,
}

impl std::fmt::Debug for MaiRuntimeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaiRuntimeTool")
            .field("agent_id", &self.agent_id)
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("tool", &self.definition.name)
            .finish()
    }
}

impl MaiRuntimeTool {
    fn new(
        runtime: Arc<AgentRuntime>,
        agent: Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        definition: ToolDefinition,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            runtime,
            agent,
            agent_id,
            session_id,
            turn_id,
            definition,
            cancellation_token,
        }
    }
}

impl Tool for MaiRuntimeTool {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn input_schema(&self) -> Value {
        self.definition.parameters.clone()
    }

    fn execute<'a>(
        &'a self,
        input: ToolInput,
        context: ToolContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<ToolOutput, PureError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let execution = super::tools::run_tool_call(
                &super::tools::ToolCallContext {
                    store: self.runtime.deps.store.as_ref(),
                    events: &self.runtime.events,
                    agent: &self.agent,
                    agent_id: self.agent_id,
                    session_id: self.session_id,
                    turn_id: self.turn_id,
                },
                super::tools::ToolCallInfo {
                    call_id: context
                        .provider_call_id
                        .as_deref()
                        .unwrap_or(input.tool_id.as_str()),
                    name: &self.definition.name,
                    arguments: input.arguments,
                },
                |arguments| {
                    let runtime = self.runtime.clone();
                    let agent = self.agent.clone();
                    let name = self.definition.name.clone();
                    let cancellation_token = self.cancellation_token.clone();
                    async move {
                        runtime
                            .execute_tool(
                                &agent,
                                self.agent_id,
                                self.turn_id,
                                &name,
                                arguments,
                                cancellation_token,
                            )
                            .await
                    }
                },
            )
            .await
            .map_err(pure_error_from_runtime)?;
            Ok(ToolOutput {
                description: execution.model_output,
                truncated: OutputTruncation::empty(),
                output_file: std::path::PathBuf::new(),
                exit_code: None,
                timed_out: false,
                runtime_events: if execution.ends_turn {
                    vec![ToolRuntimeEvent::EndTurn]
                } else {
                    Vec::new()
                },
            })
        })
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

fn new_compaction_summary(
    before: &[pl_protocol::Message],
    after: &[pl_protocol::Message],
) -> Option<String> {
    let before_summaries = before
        .iter()
        .filter_map(compaction_summary_text)
        .collect::<std::collections::BTreeSet<_>>();
    after
        .iter()
        .filter_map(compaction_summary_text)
        .find(|summary| !before_summaries.contains(summary))
        .map(str::to_string)
}

fn compaction_summary_text(message: &pl_protocol::Message) -> Option<&str> {
    super::history::user_message_text(message).filter(|text| {
        super::history::is_compact_summary(text.trim(), crate::COMPACT_SUMMARY_PREFIX)
    })
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

fn pure_error_from_runtime(error: RuntimeError) -> PureError {
    PureError::ToolExecutionFailed {
        tool: "mai_runtime_tool".to_string(),
        error: error.to_string(),
    }
}
