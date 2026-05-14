use std::collections::HashSet;

use mai_protocol::{RelayError, RelayResponse};
use mai_runtime::RuntimeError;
use serde_json::Value;

pub(crate) fn relay_connect_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let websocket = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        trimmed.to_string()
    };
    if websocket.ends_with("/relay/v1/connect") {
        websocket
    } else {
        format!("{websocket}/relay/v1/connect")
    }
}

pub(crate) fn associated_pull_requests(payload: &Value) -> Vec<u64> {
    let mut prs = HashSet::new();
    for key in ["check_run", "check_suite"] {
        if let Some(items) = payload
            .get(key)
            .and_then(|value| value.get("pull_requests"))
            .and_then(Value::as_array)
        {
            for item in items {
                if let Some(number) = item.get("number").and_then(Value::as_u64) {
                    prs.insert(number);
                }
            }
        }
    }
    prs.into_iter().collect()
}

pub(crate) fn relay_response(id: String, result: Result<Value, RuntimeError>) -> RelayResponse {
    match result {
        Ok(result) => RelayResponse {
            id,
            result: Some(result),
            error: None,
        },
        Err(error) => RelayResponse {
            id,
            result: None,
            error: Some(RelayError {
                code: "runtime".to_string(),
                message: error.to_string(),
            }),
        },
    }
}
