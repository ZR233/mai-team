use mai_protocol::{ModelInputItem, ModelOutputItem, ModelResponse, TokenUsage, ToolDefinition};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("model returned {status}: {body}")]
    Api { status: StatusCode, body: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ModelError>;

#[derive(Debug, Clone, Default)]
pub struct ResponsesClient {
    http: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ModelInputItem],
    tools: &'a [ToolDefinition],
    tool_choice: &'a str,
    stream: bool,
}

impl ResponsesClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn create_response(
        &self,
        base_url: &str,
        api_key: &str,
        model: &str,
        instructions: &str,
        input: &[ModelInputItem],
        tools: &[ToolDefinition],
    ) -> Result<ModelResponse> {
        let endpoint = format!("{}/responses", base_url.trim_end_matches('/'));
        let request = ResponsesRequest {
            model,
            instructions,
            input,
            tools,
            tool_choice: "auto",
            stream: false,
        };
        let response = self
            .http
            .post(endpoint)
            .bearer_auth(api_key)
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(ModelError::Api { status, body });
        }

        parse_response(serde_json::from_str(&body)?)
    }
}

fn parse_response(value: Value) -> Result<ModelResponse> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(parse_output_item)
        .collect::<Vec<_>>();
    let usage = parse_usage(value.get("usage"));
    Ok(ModelResponse { id, output, usage })
}

fn parse_output_item(value: Value) -> ModelOutputItem {
    match value.get("type").and_then(Value::as_str) {
        Some("message") => {
            let text = value
                .get("content")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            item.get("text")
                                .or_else(|| item.get("output_text"))
                                .and_then(Value::as_str)
                        })
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            ModelOutputItem::Message { text }
        }
        Some("function_call") => {
            let call_id = value
                .get("call_id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let raw_arguments = value
                .get("arguments")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "{}".to_string());
            let arguments = serde_json::from_str(&raw_arguments)
                .unwrap_or_else(|_| json!({ "raw": raw_arguments.clone() }));
            ModelOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            }
        }
        _ => ModelOutputItem::Other { raw: value },
    }
}

fn parse_usage(value: Option<&Value>) -> Option<TokenUsage> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_message_and_function_call() {
        let response = parse_response(json!({
            "id": "resp_1",
            "output": [
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "hello" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "container_exec",
                    "arguments": "{\"command\":\"pwd\"}"
                }
            ],
            "usage": { "input_tokens": 1, "output_tokens": 2, "total_tokens": 3 }
        }))
        .expect("parse");
        assert_eq!(response.output.len(), 2);
        assert_eq!(response.usage.expect("usage").total_tokens, 3);
    }
}
