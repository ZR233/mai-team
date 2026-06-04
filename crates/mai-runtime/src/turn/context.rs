use mai_protocol::{ModelInputItem, ToolDefinition};
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
}

impl ContextCompactionRequest {
    pub(crate) fn from_estimate(estimated_tokens: u64) -> Self {
        Self {
            estimated_tokens: Some(estimated_tokens),
        }
    }

    pub(crate) fn last_context_only() -> Self {
        Self {
            estimated_tokens: None,
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

pub(crate) fn context_compaction_decision(
    last_context_tokens: Option<u64>,
    estimated_request_tokens: Option<u64>,
    context_tokens: u64,
    threshold_percent: u64,
) -> Option<ContextCompactionDecision> {
    if context_tokens == 0 {
        return None;
    }
    let last_tokens = last_context_tokens?;
    let estimated_tokens = estimated_request_tokens.unwrap_or_default();
    let (trigger, tokens_before) = if estimated_tokens > last_tokens {
        (ContextCompactionTrigger::RequestEstimate, estimated_tokens)
    } else {
        (ContextCompactionTrigger::LastContextTokens, last_tokens)
    };
    should_compact(tokens_before, context_tokens, threshold_percent).then_some(
        ContextCompactionDecision {
            trigger,
            tokens_before,
        },
    )
}

pub(crate) fn estimate_model_request_tokens(
    instructions: &str,
    history: &[ModelInputItem],
    tools: &[ToolDefinition],
) -> u64 {
    let bytes = instructions.len() as u64 + serialized_len(history) + serialized_len(tools);
    bytes.div_ceil(TOKEN_ESTIMATE_BYTES)
}

fn should_compact(tokens_before: u64, context_tokens: u64, threshold_percent: u64) -> bool {
    if tokens_before == 0 || context_tokens == 0 {
        return false;
    }
    tokens_before.saturating_mul(100) >= context_tokens.saturating_mul(threshold_percent)
}

fn serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .map(|value| value.len() as u64)
        .unwrap_or_else(|_| json!(value).to_string().len() as u64)
}
