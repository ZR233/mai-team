use mai_protocol::TokenUsage;
use serde_json::Value;

pub(crate) fn parse_responses_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let value = usage_object(value)?;
    let input_tokens = number_field(value, "input_tokens");
    let output_tokens = number_field(value, "output_tokens");
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens: value
            .get("input_tokens_details")
            .and_then(|details| number_field_opt(details, "cached_tokens"))
            .unwrap_or_default(),
        output_tokens,
        reasoning_output_tokens: value
            .get("output_tokens_details")
            .and_then(|details| number_field_opt(details, "reasoning_tokens"))
            .unwrap_or_default(),
        total_tokens: number_field_opt(value, "total_tokens")
            .unwrap_or(input_tokens + output_tokens),
    })
}

pub(crate) fn parse_chat_usage(value: Option<&Value>) -> Option<TokenUsage> {
    parse_chat_usage_with_cached_input(value, |value| {
        value
            .get("prompt_tokens_details")
            .and_then(|details| number_field_opt(details, "cached_tokens"))
            .unwrap_or_default()
    })
}

pub(crate) fn parse_deepseek_chat_usage(value: Option<&Value>) -> Option<TokenUsage> {
    parse_chat_usage_with_cached_input(value, |value| {
        number_field_opt(value, "prompt_cache_hit_tokens").unwrap_or_default()
    })
}

fn parse_chat_usage_with_cached_input(
    value: Option<&Value>,
    cached_input_tokens: impl FnOnce(&Value) -> u64,
) -> Option<TokenUsage> {
    let value = usage_object(value)?;
    let input_tokens = number_field(value, "prompt_tokens");
    let output_tokens = number_field(value, "completion_tokens");
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens: cached_input_tokens(value),
        output_tokens,
        reasoning_output_tokens: value
            .get("completion_tokens_details")
            .and_then(|details| number_field_opt(details, "reasoning_tokens"))
            .unwrap_or_default(),
        total_tokens: number_field_opt(value, "total_tokens")
            .unwrap_or(input_tokens + output_tokens),
    })
}

fn usage_object(value: Option<&Value>) -> Option<&Value> {
    let value = value?;
    value.as_object()?;
    Some(value)
}

fn number_field(value: &Value, key: &str) -> u64 {
    number_field_opt(value, key).unwrap_or_default()
}

fn number_field_opt(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_usage_parses_openai_cached_and_reasoning_tokens() {
        let usage = parse_responses_usage(Some(&json!({
            "input_tokens": 100,
            "input_tokens_details": { "cached_tokens": 40 },
            "output_tokens": 30,
            "output_tokens_details": { "reasoning_tokens": 10 },
            "total_tokens": 130
        })))
        .expect("usage");

        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 40,
                output_tokens: 30,
                reasoning_output_tokens: 10,
                total_tokens: 130,
            }
        );
    }

    #[test]
    fn chat_usage_parses_openai_cached_and_reasoning_tokens() {
        let usage = parse_chat_usage(Some(&json!({
            "prompt_tokens": 200,
            "prompt_tokens_details": { "cached_tokens": 64 },
            "completion_tokens": 20,
            "completion_tokens_details": { "reasoning_tokens": 8 },
            "total_tokens": 220
        })))
        .expect("usage");

        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 200,
                cached_input_tokens: 64,
                output_tokens: 20,
                reasoning_output_tokens: 8,
                total_tokens: 220,
            }
        );
    }

    #[test]
    fn deepseek_chat_usage_adapts_prompt_cache_hit_tokens() {
        let usage = parse_deepseek_chat_usage(Some(&json!({
            "prompt_tokens": 300,
            "prompt_cache_hit_tokens": 128,
            "prompt_cache_miss_tokens": 172,
            "completion_tokens": 25,
            "completion_tokens_details": { "reasoning_tokens": 11 },
            "total_tokens": 325
        })))
        .expect("usage");

        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 300,
                cached_input_tokens: 128,
                output_tokens: 25,
                reasoning_output_tokens: 11,
                total_tokens: 325,
            }
        );
    }

    #[test]
    fn missing_optional_usage_details_default_to_zero() {
        let usage = parse_chat_usage(Some(&json!({
            "prompt_tokens": 3,
            "completion_tokens": 2,
            "total_tokens": 5
        })))
        .expect("usage");

        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 3,
                cached_input_tokens: 0,
                output_tokens: 2,
                reasoning_output_tokens: 0,
                total_tokens: 5,
            }
        );
    }
}
