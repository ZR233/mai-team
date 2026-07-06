use mai_protocol::ToolDefinition;
use pl_protocol::Message;
use serde_json::json;

const TOKEN_ESTIMATE_BYTES: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextCompactionTrigger {
    LastContextTokens,
    RequestEstimate,
}

impl ContextCompactionTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LastContextTokens => "last_context_tokens",
            Self::RequestEstimate => "request_estimate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextCompactionRequest {
    pub(crate) estimated_tokens: Option<u64>,
    pub(crate) last_context_tokens_override: Option<u64>,
}

impl ContextCompactionRequest {
    pub(crate) fn from_estimate(estimated_tokens: u64) -> Self {
        Self {
            estimated_tokens: Some(estimated_tokens),
            last_context_tokens_override: None,
        }
    }

    pub(crate) fn last_context_only() -> Self {
        Self {
            estimated_tokens: None,
            last_context_tokens_override: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextCompactionOutcome {
    Skipped,
    Compacted {
        trigger: ContextCompactionTrigger,
        tokens_before: u64,
        last_context_tokens: Option<u64>,
        estimated_request_tokens: Option<u64>,
        context_tokens: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextCompactionDecision {
    pub(crate) trigger: ContextCompactionTrigger,
    pub(crate) tokens_before: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextBudgetPolicy {
    context_tokens: u64,
    threshold_percent: u64,
    auto_compact_token_limit: Option<u64>,
}

impl ContextBudgetPolicy {
    pub(crate) fn new(
        context_tokens: u64,
        threshold_percent: u64,
        auto_compact_token_limit: Option<u64>,
    ) -> Self {
        Self {
            context_tokens,
            threshold_percent,
            auto_compact_token_limit,
        }
    }

    pub(crate) fn decision(
        self,
        last_context_tokens: Option<u64>,
        estimated_request_tokens: Option<u64>,
    ) -> Option<ContextCompactionDecision> {
        let compact_limit = self.compact_limit()?;
        let (trigger, tokens_before) = match (last_context_tokens, estimated_request_tokens) {
            (Some(last_tokens), Some(estimated_tokens)) if estimated_tokens > last_tokens => {
                (ContextCompactionTrigger::RequestEstimate, estimated_tokens)
            }
            (Some(last_tokens), _) => (ContextCompactionTrigger::LastContextTokens, last_tokens),
            (None, Some(estimated_tokens)) => {
                (ContextCompactionTrigger::RequestEstimate, estimated_tokens)
            }
            (None, None) => return None,
        };
        (tokens_before >= compact_limit).then_some(ContextCompactionDecision {
            trigger,
            tokens_before,
        })
    }

    fn compact_limit(self) -> Option<u64> {
        if self.context_tokens == 0 {
            return None;
        }
        let percent_limit = self
            .context_tokens
            .saturating_mul(self.threshold_percent)
            .div_ceil(100);
        match self.auto_compact_token_limit {
            Some(limit) if limit > 0 => Some(limit.min(percent_limit)),
            Some(_) | None => Some(percent_limit),
        }
    }
}

pub(crate) fn estimate_model_request_tokens(
    instructions: &str,
    history: &[Message],
    tools: &[ToolDefinition],
) -> u64 {
    let bytes = instructions.len() as u64 + serialized_len(history) + serialized_len(tools);
    bytes.div_ceil(TOKEN_ESTIMATE_BYTES)
}

fn serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .map(|value| value.len() as u64)
        .unwrap_or_else(|_| json!(value).to_string().len() as u64)
}
