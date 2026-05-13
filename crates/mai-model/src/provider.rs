use crate::http::HeaderMerge;
use crate::wire::chat_completions::ChatCompletionsApi;
use crate::wire::responses::ResponsesApi;
use crate::wire::WireProtocol;
use mai_protocol::{ModelConfig, ModelWireApi, ProviderKind, ProviderSecret};
use reqwest::header::HeaderMap;
use std::fmt::Debug;
use std::sync::Arc;

pub struct ResolvedProvider {
    pub endpoint: String,
    pub api_key: String,
    pub headers: HeaderMap,
    pub wire_protocol: Arc<dyn WireProtocol>,
    pub max_output_tokens: u64,
}

pub trait ProviderResolver: Debug + Send + Sync {
    fn resolve(&self, provider: &ProviderSecret, model: &ModelConfig) -> ResolvedProvider;
}

#[derive(Debug)]
pub struct DefaultProviderResolver {
    responses: Arc<ResponsesApi>,
    chat_completions: Arc<ChatCompletionsApi>,
}

impl DefaultProviderResolver {
    pub fn new() -> Self {
        Self {
            responses: Arc::new(ResponsesApi),
            chat_completions: Arc::new(ChatCompletionsApi),
        }
    }
}

impl Default for DefaultProviderResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderResolver for DefaultProviderResolver {
    fn resolve(&self, provider: &ProviderSecret, model: &ModelConfig) -> ResolvedProvider {
        let wire_api = effective_wire_api(provider, model);
        let wire_protocol: Arc<dyn WireProtocol> = match wire_api {
            ModelWireApi::Responses => Arc::clone(&self.responses) as Arc<dyn WireProtocol>,
            ModelWireApi::ChatCompletions => {
                Arc::clone(&self.chat_completions) as Arc<dyn WireProtocol>
            }
        };
        let endpoint = format!(
            "{}{}",
            provider.base_url.trim_end_matches('/'),
            wire_protocol.path()
        );
        let headers =
            crate::http::headers(&provider.headers(&crate::http::request_headers(model)));
        let max_output_tokens = resolve_max_tokens(provider, model);

        ResolvedProvider {
            endpoint,
            api_key: provider.api_key.clone(),
            headers,
            wire_protocol,
            max_output_tokens,
        }
    }
}

fn effective_wire_api(provider: &ProviderSecret, model: &ModelConfig) -> ModelWireApi {
    match provider.kind {
        ProviderKind::Openai => model.wire_api,
        ProviderKind::Deepseek | ProviderKind::Mimo => ModelWireApi::ChatCompletions,
    }
}

fn resolve_max_tokens(provider: &ProviderSecret, model: &ModelConfig) -> u64 {
    match provider.kind {
        ProviderKind::Deepseek => model.output_tokens.clamp(1, 64_000),
        ProviderKind::Mimo => model.output_tokens.clamp(1, 131_072),
        _ => model.output_tokens,
    }
}
