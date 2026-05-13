pub mod chat_completions;
pub mod responses;

use crate::error::Result;
use mai_protocol::{ModelInputItem, ModelResponse, ToolDefinition};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Debug;

pub(crate) trait WireProtocol: Debug + Send + Sync {
    fn path(&self) -> &'static str;
    fn build_body(&self, req: &WireRequest<'_>) -> Result<Vec<u8>>;
    fn parse_response(&self, body: &str) -> Result<ModelResponse>;
}

pub(crate) struct WireRequest<'a> {
    pub(crate) model_id: &'a str,
    pub(crate) instructions: &'a str,
    pub(crate) input: &'a [ModelInputItem],
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) tool_choice: Option<&'a str>,
    pub(crate) stream: bool,
    pub(crate) store: Option<bool>,
    pub(crate) previous_response_id: Option<&'a str>,
    pub(crate) max_output_tokens: u64,
    pub(crate) extra_body: BTreeMap<String, Value>,
    pub(crate) supports_tools: bool,
}

pub(crate) fn parse_usage(value: Option<&Value>) -> Option<mai_protocol::TokenUsage> {
    use mai_protocol::TokenUsage;
    let value = value?;
    let input_tokens = value
        .get("input_tokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = value
        .get("output_tokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}
