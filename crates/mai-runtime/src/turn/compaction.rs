use mai_protocol::{AgentId, ModelInputItem, ServiceEventKind, SessionId, ToolDefinition, TurnId};
use mai_store::ConfigStore;
use serde_json::json;

use crate::events::RuntimeEvents;

use super::context::{ContextBudgetPolicy, ContextCompactionDecision, ContextCompactionRequest};
use super::history;
use super::persistence::{self, AgentLogRecord};

pub(crate) struct ContextBudgetPlanner {
    policy: ContextBudgetPolicy,
}

impl ContextBudgetPlanner {
    pub(crate) fn new(
        context_tokens: u64,
        threshold_percent: u64,
        auto_compact_token_limit: Option<u64>,
    ) -> Self {
        Self {
            policy: ContextBudgetPolicy::new(
                context_tokens,
                threshold_percent,
                auto_compact_token_limit,
            ),
        }
    }

    pub(crate) fn decision(
        &self,
        last_context_tokens: Option<u64>,
        request: ContextCompactionRequest,
    ) -> Option<ContextCompactionDecision> {
        self.policy
            .decision(last_context_tokens, request.estimated_tokens)
    }
}

pub(crate) struct HistoryCompactor<'a> {
    history: &'a [ModelInputItem],
    summary_prefix: &'a str,
    prompt: &'a str,
    max_user_chars: usize,
}

impl<'a> HistoryCompactor<'a> {
    pub(crate) fn new(
        history: &'a [ModelInputItem],
        summary_prefix: &'a str,
        prompt: &'a str,
        max_user_chars: usize,
    ) -> Self {
        Self {
            history,
            summary_prefix,
            prompt,
            max_user_chars,
        }
    }

    pub(crate) fn compact_request_input(&self) -> Vec<ModelInputItem> {
        let mut input = history::build_compaction_request_history(
            self.history,
            self.max_user_chars,
            self.summary_prefix,
        );
        input.push(ModelInputItem::user_text(self.prompt));
        input
    }

    pub(crate) fn fallback_compact_request_input(&self) -> Vec<ModelInputItem> {
        let reduced_user_chars = (self.max_user_chars / 4).max(4_000);
        let mut input = history::build_compaction_request_history(
            self.history,
            reduced_user_chars,
            self.summary_prefix,
        );
        if input.len() > 12 {
            let keep_from = input.len().saturating_sub(12);
            input.drain(0..keep_from);
        }
        input.push(ModelInputItem::user_text(self.prompt));
        input
    }

    pub(crate) fn replacement_history(&self, summary: &str) -> Vec<ModelInputItem> {
        history::build_compacted_history(
            self.history,
            summary,
            self.max_user_chars,
            self.summary_prefix,
        )
    }
}

pub(crate) struct CompactionRecorder<'a> {
    store: &'a ConfigStore,
    events: &'a RuntimeEvents,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    preview_chars: usize,
}

pub(crate) struct CompactionRecord<'a> {
    pub(crate) decision: ContextCompactionDecision,
    pub(crate) last_context_tokens: Option<u64>,
    pub(crate) estimated_request_tokens: Option<u64>,
    pub(crate) context_tokens: u64,
    pub(crate) replacement_tokens: u64,
    pub(crate) summary: &'a str,
}

impl<'a> CompactionRecorder<'a> {
    pub(crate) fn new(
        store: &'a ConfigStore,
        events: &'a RuntimeEvents,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        preview_chars: usize,
    ) -> Self {
        Self {
            store,
            events,
            agent_id,
            session_id,
            turn_id,
            preview_chars,
        }
    }

    pub(crate) async fn record_success(&self, record: CompactionRecord<'_>) {
        self.events
            .publish(ServiceEventKind::ContextCompacted {
                agent_id: self.agent_id,
                session_id: self.session_id,
                turn_id: self.turn_id,
                tokens_before: record.decision.tokens_before,
                summary_preview: preview(record.summary, self.preview_chars),
            })
            .await;
        persistence::record_agent_log(
            self.store,
            AgentLogRecord {
                agent_id: self.agent_id,
                session_id: Some(self.session_id),
                turn_id: Some(self.turn_id),
                level: "info",
                category: "context",
                message: "context compacted",
                details: json!({
                    "trigger": record.decision.trigger.as_str(),
                    "tokens_before": record.decision.tokens_before,
                    "last_context_tokens": record.last_context_tokens,
                    "estimated_request_tokens": record.estimated_request_tokens,
                    "context_tokens": record.context_tokens,
                    "replacement_tokens": record.replacement_tokens,
                    "summary_preview": preview(record.summary, self.preview_chars),
                }),
            },
        )
        .await;
    }
}

pub(crate) fn estimate_history_tokens(
    instructions: &str,
    history: &[ModelInputItem],
    tools: &[ToolDefinition],
) -> u64 {
    super::context::estimate_model_request_tokens(instructions, history, tools)
}

pub(crate) fn compaction_error_allows_fallback(err: &crate::RuntimeError) -> bool {
    let message = err.to_string().to_lowercase();
    message.contains("context")
        && (message.contains("window")
            || message.contains("length")
            || message.contains("too large"))
        || message.contains("response.incomplete")
        || message.contains("max_output_tokens")
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
