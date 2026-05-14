use crate::*;

#[derive(Debug, Clone)]
pub struct ProviderSelection {
    pub provider: ProviderSecret,
    pub model: ModelConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct ProvidersCache {
    stamp: ProvidersCacheStamp,
    file: ProvidersToml,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvidersCacheStamp {
    exists: bool,
    modified: Option<SystemTime>,
    len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProvidersToml {
    #[serde(default)]
    default_provider_id: Option<String>,
    #[serde(default)]
    providers: BTreeMap<String, ProviderToml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderToml {
    kind: ProviderKind,
    name: String,
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    default_model: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    models: BTreeMap<String, ModelToml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelToml {
    #[serde(default)]
    name: Option<String>,
    context_tokens: u64,
    output_tokens: u64,
    #[serde(default = "default_true")]
    supports_tools: bool,
    #[serde(default)]
    wire_api: ModelWireApi,
    #[serde(default)]
    capabilities: ModelCapabilities,
    #[serde(default)]
    request_policy: ModelRequestPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<ModelReasoningConfig>,
    #[serde(default, skip_serializing_if = "is_null")]
    options: serde_json::Value,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
}

impl ConfigStore {
    pub async fn seed_default_provider_from_env(
        &self,
        api_key: Option<String>,
        base_url: String,
        model: String,
    ) -> Result<()> {
        if self.provider_count().await? > 0 {
            return Ok(());
        }
        let Some(api_key) = api_key.filter(|value| !value.trim().is_empty()) else {
            return Ok(());
        };
        let mut provider = builtin_provider(ProviderKind::Openai);
        provider.base_url = base_url;
        provider.api_key = Some(api_key);
        if provider.models.iter().all(|item| item.id != model) {
            provider.models.insert(0, fallback_model(&model));
        }
        provider.default_model = model;
        self.save_providers(ProvidersConfigRequest {
            default_provider_id: Some("openai".to_string()),
            providers: vec![provider],
        })
        .await
    }

    pub async fn provider_count(&self) -> Result<usize> {
        Ok(self.load_providers_toml().await?.providers.len())
    }

    pub fn provider_presets_response(&self) -> ProviderPresetsResponse {
        ProviderPresetsResponse {
            providers: vec![
                provider_preset(ProviderKind::Openai),
                provider_preset(ProviderKind::Deepseek),
                provider_preset_from_config(mimo_builtin_provider(
                    "mimo-api",
                    "MiMo API",
                    "https://api.xiaomimimo.com/v1",
                    "MIMO_API_KEY",
                )),
                provider_preset_from_config(mimo_builtin_provider(
                    "mimo-token-plan",
                    "MiMo Token Plan",
                    "https://token-plan-cn.xiaomimimo.com/v1",
                    "MIMO_TOKEN_PLAN_API_KEY",
                )),
            ],
        }
    }

    pub async fn providers_response(&self) -> Result<ProvidersResponse> {
        let providers = self.list_provider_secrets().await?;
        let file = self.load_providers_toml().await?;
        Ok(ProvidersResponse {
            providers: providers
                .into_iter()
                .map(|provider| ProviderSummary {
                    id: provider.id,
                    kind: provider.kind,
                    name: provider.name,
                    base_url: provider.base_url,
                    api_key_env: provider.api_key_env,
                    models: provider.models,
                    default_model: provider.default_model,
                    enabled: provider.enabled,
                    has_api_key: !provider.api_key.is_empty(),
                })
                .collect(),
            default_provider_id: file.default_provider_id,
        })
    }

    pub async fn save_providers(&self, request: ProvidersConfigRequest) -> Result<()> {
        validate_provider_request(&request)?;
        let existing = self
            .load_providers_toml()
            .await
            .unwrap_or_else(|_| ProvidersToml::default());
        let mut file = ProvidersToml::default();
        for provider in &request.providers {
            let provider_id = normalized_id(&provider.id);
            let existing_provider = existing.providers.get(&provider_id);
            let api_key = provider
                .api_key
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .or_else(|| existing_provider.and_then(|item| item.api_key.clone()));
            let mut models = BTreeMap::new();
            for model in normalized_models(provider) {
                models.insert(model.id.clone(), ModelToml::from_model(model));
            }
            file.providers.insert(
                provider_id,
                ProviderToml {
                    kind: provider.kind,
                    name: provider.name.trim().to_string(),
                    base_url: provider.base_url.trim().to_string(),
                    api_key,
                    api_key_env: provider
                        .api_key_env
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string),
                    default_model: provider.default_model.trim().to_string(),
                    enabled: provider.enabled,
                    models,
                },
            );
        }

        if let Some(default_provider_id) = request.default_provider_id.as_deref() {
            file.default_provider_id = Some(default_provider_id.trim().to_string());
        } else if let Some(first) = request.providers.first() {
            file.default_provider_id = Some(first.id.trim().to_string());
        }

        self.write_providers_toml(&file)?;
        self.clear_providers_cache().await;
        Ok(())
    }

    pub async fn resolve_provider(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        let provider = match provider_id.filter(|value| !value.trim().is_empty()) {
            Some(id) => self
                .get_provider_secret(id)
                .await?
                .ok_or_else(|| StoreError::InvalidConfig(format!("provider `{id}` not found")))?,
            None => self.default_provider_secret().await?.ok_or_else(|| {
                StoreError::InvalidConfig(
                    "no provider configured; add one in the Providers page".to_string(),
                )
            })?,
        };
        if !provider.enabled {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` is disabled",
                provider.id
            )));
        }
        if provider.api_key.trim().is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` has no API key",
                provider.id
            )));
        }
        let selected_model_id = model
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| provider.default_model.clone());
        let selected_model = provider
            .models
            .iter()
            .find(|item| item.id == selected_model_id)
            .cloned()
            .ok_or_else(|| {
                StoreError::InvalidConfig(format!(
                    "model `{selected_model_id}` is not configured for provider `{}`",
                    provider.id
                ))
            })?;
        if selected_model.context_tokens == 0 || selected_model.output_tokens == 0 {
            return Err(StoreError::InvalidConfig(format!(
                "model `{selected_model_id}` must configure context_tokens and output_tokens"
            )));
        }
        Ok(ProviderSelection {
            provider,
            model: selected_model,
        })
    }

    pub async fn get_provider_secret(&self, id: &str) -> Result<Option<ProviderSecret>> {
        let providers = self.list_provider_secrets().await?;
        Ok(providers.into_iter().find(|provider| provider.id == id))
    }

    pub async fn default_provider_secret(&self) -> Result<Option<ProviderSecret>> {
        let providers = self.list_provider_secrets().await?;
        if providers.is_empty() {
            return Ok(None);
        }
        if let Some(default_provider_id) = self.load_providers_toml().await?.default_provider_id
            && let Some(provider) = providers
                .iter()
                .find(|provider| provider.id == default_provider_id && provider.enabled)
        {
            return Ok(Some(provider.clone()));
        }
        Ok(providers.into_iter().find(|provider| provider.enabled))
    }

    pub async fn list_provider_secrets(&self) -> Result<Vec<ProviderSecret>> {
        let file = self.load_providers_toml().await?;
        let mut out = Vec::with_capacity(file.providers.len());
        for (id, provider) in file.providers {
            out.push(ProviderSecret {
                models: provider
                    .models
                    .into_iter()
                    .map(|(id, model)| model.into_model(id, provider.kind))
                    .collect(),
                id,
                kind: provider.kind,
                name: provider.name,
                base_url: provider.base_url,
                api_key: resolve_api_key(provider.api_key, provider.api_key_env.as_deref()),
                api_key_env: provider.api_key_env,
                default_model: provider.default_model,
                enabled: provider.enabled,
            });
        }
        Ok(out)
    }

    async fn load_providers_toml(&self) -> Result<ProvidersToml> {
        let stamp = providers_cache_stamp(&self.config_path)?;
        if let Some(cache) = self.providers_cache.lock().await.as_ref()
            && cache.stamp == stamp
        {
            return Ok(cache.file.clone());
        }

        let (file, stamp) = self.read_providers_toml_with_stamp(stamp)?;
        *self.providers_cache.lock().await = Some(ProvidersCache {
            stamp,
            file: file.clone(),
        });
        Ok(file)
    }

    fn read_providers_toml_with_stamp(
        &self,
        stamp: ProvidersCacheStamp,
    ) -> Result<(ProvidersToml, ProvidersCacheStamp)> {
        if !stamp.exists {
            return Ok((ProvidersToml::default(), stamp));
        }
        let text = match std::fs::read_to_string(&self.config_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok((
                    ProvidersToml::default(),
                    providers_cache_stamp(&self.config_path)?,
                ));
            }
            Err(err) => return Err(err.into()),
        };
        match toml::from_str::<ProvidersToml>(&text) {
            Ok(file) => match validate_providers_toml(&file) {
                Ok(()) => Ok((file, providers_cache_stamp(&self.config_path)?)),
                Err(_) => {
                    let _ = std::fs::remove_file(&self.config_path);
                    Ok((
                        ProvidersToml::default(),
                        providers_cache_stamp(&self.config_path)?,
                    ))
                }
            },
            Err(_) => {
                let _ = std::fs::remove_file(&self.config_path);
                Ok((
                    ProvidersToml::default(),
                    providers_cache_stamp(&self.config_path)?,
                ))
            }
        }
    }

    fn write_providers_toml(&self, file: &ProvidersToml) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(file)?;
        std::fs::write(&self.config_path, text)?;
        Ok(())
    }

    async fn clear_providers_cache(&self) {
        *self.providers_cache.lock().await = None;
    }
}

impl ModelToml {
    fn from_model(model: ModelConfig) -> Self {
        Self {
            name: model.name,
            context_tokens: model.context_tokens,
            output_tokens: model.output_tokens,
            supports_tools: model.supports_tools,
            wire_api: model.wire_api,
            capabilities: model.capabilities,
            request_policy: model.request_policy,
            reasoning: model.reasoning,
            options: model.options,
            headers: model.headers,
        }
    }

    fn into_model(self, id: String, provider_kind: ProviderKind) -> ModelConfig {
        let wire_api = migrated_wire_api(provider_kind, self.wire_api);
        let capabilities = migrated_capabilities(provider_kind, &id, self.capabilities);
        let request_policy = migrated_request_policy(provider_kind, self.request_policy);
        ModelConfig {
            id,
            name: self.name,
            context_tokens: self.context_tokens,
            output_tokens: self.output_tokens,
            supports_tools: self.supports_tools,
            wire_api,
            capabilities,
            request_policy,
            reasoning: self.reasoning,
            options: self.options,
            headers: self.headers,
        }
    }
}

fn providers_cache_stamp(path: &Path) -> Result<ProvidersCacheStamp> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(ProvidersCacheStamp {
            exists: true,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ProvidersCacheStamp {
            exists: false,
            modified: None,
            len: 0,
        }),
        Err(err) => Err(err.into()),
    }
}

fn migrated_wire_api(provider_kind: ProviderKind, wire_api: ModelWireApi) -> ModelWireApi {
    match provider_kind {
        ProviderKind::Openai => wire_api,
        ProviderKind::Deepseek | ProviderKind::Mimo => ModelWireApi::ChatCompletions,
    }
}

fn migrated_capabilities(
    provider_kind: ProviderKind,
    model_id: &str,
    mut capabilities: ModelCapabilities,
) -> ModelCapabilities {
    match provider_kind {
        ProviderKind::Openai => {
            capabilities.continuation = true;
            capabilities.parallel_tools = true;
        }
        ProviderKind::Deepseek => {
            capabilities.continuation = false;
            capabilities.reasoning_replay = capabilities.reasoning_replay
                || model_id.contains("reasoner")
                || model_id.contains("pro");
        }
        ProviderKind::Mimo => {
            capabilities.continuation = false;
        }
    }
    capabilities
}

fn migrated_request_policy(
    provider_kind: ProviderKind,
    mut request_policy: ModelRequestPolicy,
) -> ModelRequestPolicy {
    match provider_kind {
        ProviderKind::Openai => {
            request_policy.store = request_policy.store.or(Some(true));
            if request_policy.max_tokens_field == "max_tokens" {
                request_policy.max_tokens_field = "max_output_tokens".to_string();
            }
        }
        ProviderKind::Deepseek | ProviderKind::Mimo => {
            if request_policy.store == Some(true) {
                request_policy.store = None;
            }
            if request_policy.max_tokens_field.trim().is_empty() {
                request_policy.max_tokens_field = "max_tokens".to_string();
            }
            if provider_kind == ProviderKind::Mimo
                && request_policy.max_tokens_field == "max_tokens"
            {
                request_policy.max_tokens_field = "max_completion_tokens".to_string();
            }
        }
    }
    request_policy
}

fn provider_preset(kind: ProviderKind) -> ProviderPreset {
    let provider = builtin_provider(kind);
    provider_preset_from_config(provider)
}

fn provider_preset_from_config(provider: ProviderConfig) -> ProviderPreset {
    ProviderPreset {
        id: provider.id,
        kind: provider.kind,
        name: provider.name,
        base_url: provider.base_url,
        default_model: provider.default_model,
        models: provider.models,
    }
}

fn builtin_provider(kind: ProviderKind) -> ProviderConfig {
    match kind {
        ProviderKind::Openai => ProviderConfig {
            id: "openai".to_string(),
            kind,
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: None,
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            default_model: "gpt-5.5".to_string(),
            enabled: true,
            models: vec![
                openai_reasoning_model("gpt-5.5", 400_000, 128_000),
                openai_reasoning_model("gpt-5.4", 400_000, 128_000),
                openai_reasoning_model("gpt-5.4-mini", 400_000, 128_000),
                openai_reasoning_model("gpt-5.4-nano", 400_000, 128_000),
                openai_reasoning_model("gpt-5", 400_000, 128_000),
                ModelConfig {
                    id: "gpt-4.1".to_string(),
                    name: Some("GPT-4.1".to_string()),
                    context_tokens: 1_047_576,
                    output_tokens: 32_768,
                    supports_tools: true,
                    wire_api: ModelWireApi::Responses,
                    capabilities: openai_capabilities(),
                    request_policy: openai_request_policy(),
                    reasoning: None,
                    options: serde_json::Value::Null,
                    headers: BTreeMap::new(),
                },
            ],
        },
        ProviderKind::Deepseek => ProviderConfig {
            id: "deepseek".to_string(),
            kind,
            name: "DeepSeek".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            api_key: None,
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            default_model: "deepseek-v4-flash".to_string(),
            enabled: true,
            models: vec![
                deepseek_model("deepseek-v4-flash", false),
                deepseek_model("deepseek-v4-pro", true),
                deepseek_model("deepseek-chat", false),
                deepseek_model("deepseek-reasoner", true),
            ],
        },
        ProviderKind::Mimo => mimo_builtin_provider(
            "mimo-api",
            "MiMo API",
            "https://api.xiaomimimo.com/v1",
            "MIMO_API_KEY",
        ),
    }
}

fn mimo_builtin_provider(
    id: &str,
    name: &str,
    base_url: &str,
    api_key_env: &str,
) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        kind: ProviderKind::Mimo,
        name: name.to_string(),
        base_url: base_url.to_string(),
        api_key: None,
        api_key_env: Some(api_key_env.to_string()),
        default_model: "mimo-v2.5-pro".to_string(),
        enabled: true,
        models: vec![
            mimo_model("mimo-v2.5-pro", true),
            mimo_model("mimo-v2.5", true),
            mimo_model("mimo-v2-pro", true),
            mimo_model("mimo-v2-omni", true),
            mimo_model("mimo-v2-flash", false),
        ],
    }
}

fn openai_reasoning_model(id: &str, context_tokens: u64, output_tokens: u64) -> ModelConfig {
    let mut variants = vec!["minimal", "low", "medium", "high"];
    if id.contains("5.4") || id.contains("5.5") {
        variants.push("xhigh");
    }
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens,
        output_tokens,
        supports_tools: true,
        wire_api: ModelWireApi::Responses,
        capabilities: openai_capabilities(),
        request_policy: openai_request_policy(),
        reasoning: Some(openai_reasoning_config(variants, "medium")),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn deepseek_model(id: &str, with_reasoning: bool) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: deepseek_context_tokens(id),
        output_tokens: deepseek_output_tokens(id),
        supports_tools: true,
        wire_api: ModelWireApi::ChatCompletions,
        capabilities: deepseek_capabilities(with_reasoning),
        request_policy: chat_request_policy("max_tokens"),
        reasoning: with_reasoning.then(|| deepseek_reasoning_config(vec!["high", "max"], "high")),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn openai_reasoning_config(variants: Vec<&str>, default_variant: &str) -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some(default_variant.to_string()),
        variants: variants
            .into_iter()
            .map(|id| ModelReasoningVariant {
                id: id.to_string(),
                label: None,
                request: serde_json::json!({
                    "reasoning": {
                        "effort": id,
                    },
                }),
            })
            .collect(),
    }
}

fn deepseek_reasoning_config(variants: Vec<&str>, default_variant: &str) -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some(default_variant.to_string()),
        variants: variants
            .into_iter()
            .map(|id| ModelReasoningVariant {
                id: id.to_string(),
                label: None,
                request: serde_json::json!({
                    "thinking": {
                        "type": "enabled",
                    },
                    "reasoning_effort": id,
                }),
            })
            .collect(),
    }
}

fn deepseek_context_tokens(id: &str) -> u64 {
    if is_deepseek_v4_model(id) {
        DEEPSEEK_V4_CONTEXT_TOKENS
    } else {
        128_000
    }
}

fn deepseek_output_tokens(id: &str) -> u64 {
    if is_deepseek_v4_model(id) {
        DEEPSEEK_V4_OUTPUT_TOKENS
    } else {
        8_192
    }
}

fn is_deepseek_v4_model(id: &str) -> bool {
    matches!(
        id,
        "deepseek-v4-flash" | "deepseek-v4-pro" | "deepseek-chat" | "deepseek-reasoner"
    )
}

fn mimo_model(id: &str, with_reasoning: bool) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: mimo_context_tokens(id),
        output_tokens: mimo_output_tokens(id),
        supports_tools: true,
        wire_api: ModelWireApi::ChatCompletions,
        capabilities: mimo_capabilities(with_reasoning),
        request_policy: chat_request_policy("max_completion_tokens"),
        reasoning: with_reasoning.then(mimo_reasoning_config),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn mimo_context_tokens(id: &str) -> u64 {
    match id {
        "mimo-v2.5-pro" | "mimo-v2-pro" | "mimo-v2.5" => 1_000_000,
        "mimo-v2-omni" | "mimo-v2-flash" => 256_000,
        _ => 128_000,
    }
}

fn mimo_output_tokens(id: &str) -> u64 {
    match id {
        "mimo-v2.5-pro" | "mimo-v2-pro" | "mimo-v2.5" | "mimo-v2-omni" => 131_072,
        "mimo-v2-flash" => 65_536,
        _ => 32_768,
    }
}

fn mimo_reasoning_config() -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some("high".to_string()),
        variants: vec![ModelReasoningVariant {
            id: "high".to_string(),
            label: None,
            request: serde_json::json!({
                "thinking": {
                    "type": "enabled",
                },
            }),
        }],
    }
}

fn openai_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        parallel_tools: true,
        reasoning_replay: false,
        strict_schema: false,
        continuation: true,
    }
}

fn deepseek_capabilities(with_reasoning: bool) -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        parallel_tools: false,
        reasoning_replay: with_reasoning,
        strict_schema: false,
        continuation: false,
    }
}

fn mimo_capabilities(with_reasoning: bool) -> ModelCapabilities {
    ModelCapabilities {
        tools: true,
        parallel_tools: false,
        reasoning_replay: with_reasoning,
        strict_schema: false,
        continuation: false,
    }
}

fn openai_request_policy() -> ModelRequestPolicy {
    ModelRequestPolicy {
        max_tokens_field: "max_output_tokens".to_string(),
        store: Some(true),
        ..ModelRequestPolicy::default()
    }
}

fn chat_request_policy(max_tokens_field: &str) -> ModelRequestPolicy {
    ModelRequestPolicy {
        max_tokens_field: max_tokens_field.to_string(),
        ..ModelRequestPolicy::default()
    }
}

fn fallback_model(id: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 128_000,
        output_tokens: 8_192,
        supports_tools: true,
        wire_api: ModelWireApi::Responses,
        capabilities: ModelCapabilities::default(),
        request_policy: ModelRequestPolicy::default(),
        reasoning: None,
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
    }
}

fn resolve_api_key(api_key: Option<String>, api_key_env: Option<&str>) -> String {
    api_key
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            api_key_env
                .filter(|name| !name.trim().is_empty())
                .and_then(|name| std::env::var(name).ok())
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_default()
}

fn normalized_id(value: &str) -> String {
    value.trim().to_string()
}

fn is_null(value: &serde_json::Value) -> bool {
    value.is_null()
}

fn validate_providers_toml(file: &ProvidersToml) -> Result<()> {
    let providers = file
        .providers
        .iter()
        .map(|(id, provider)| ProviderConfig {
            id: id.clone(),
            kind: provider.kind,
            name: provider.name.clone(),
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            api_key_env: provider.api_key_env.clone(),
            default_model: provider.default_model.clone(),
            enabled: provider.enabled,
            models: provider
                .models
                .iter()
                .map(|(model_id, model)| model.clone().into_model(model_id.clone(), provider.kind))
                .collect(),
        })
        .collect();
    validate_provider_request(&ProvidersConfigRequest {
        default_provider_id: file.default_provider_id.clone(),
        providers,
    })
}

fn validate_provider_request(request: &ProvidersConfigRequest) -> Result<()> {
    let mut ids = BTreeSet::new();
    for provider in &request.providers {
        if provider.id.trim().is_empty() {
            return Err(StoreError::InvalidConfig(
                "provider id is required".to_string(),
            ));
        }
        if provider.name.trim().is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` name is required",
                provider.id
            )));
        }
        if provider.base_url.trim().is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` base_url is required",
                provider.id
            )));
        }
        let models = normalized_models(provider);
        let default_model = provider.default_model.trim();
        if default_model.is_empty() {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` default_model is required",
                provider.id
            )));
        }
        if !models.iter().any(|model| model.id == default_model) {
            return Err(StoreError::InvalidConfig(format!(
                "provider `{}` default_model `{default_model}` is not in models",
                provider.id
            )));
        }
        for model in &models {
            if model.context_tokens == 0 || model.output_tokens == 0 {
                return Err(StoreError::InvalidConfig(format!(
                    "model `{}` must configure context_tokens and output_tokens",
                    model.id
                )));
            }
            if model.request_policy.max_tokens_field.trim().is_empty() {
                return Err(StoreError::InvalidConfig(format!(
                    "model `{}` request_policy.max_tokens_field is required",
                    model.id
                )));
            }
            if !(model.request_policy.extra_body.is_null()
                || model.request_policy.extra_body.is_object())
            {
                return Err(StoreError::InvalidConfig(format!(
                    "model `{}` request_policy.extra_body must be an object",
                    model.id
                )));
            }
            if let Some(reasoning) = &model.reasoning {
                if reasoning.variants.is_empty() {
                    return Err(StoreError::InvalidConfig(format!(
                        "reasoning model `{}` must configure reasoning variants",
                        model.id
                    )));
                }
                let mut variants = BTreeSet::new();
                for variant in &reasoning.variants {
                    let id = variant.id.trim();
                    if id.is_empty() {
                        return Err(StoreError::InvalidConfig(format!(
                            "model `{}` reasoning variant id is required",
                            model.id
                        )));
                    }
                    if !variant.request.is_object() {
                        return Err(StoreError::InvalidConfig(format!(
                            "model `{}` reasoning variant `{id}` request must be an object",
                            model.id
                        )));
                    }
                    if !variants.insert(id.to_string()) {
                        return Err(StoreError::InvalidConfig(format!(
                            "model `{}` has duplicate reasoning variant `{id}`",
                            model.id
                        )));
                    }
                }
                if let Some(default_variant) = reasoning.default_variant.as_deref()
                    && !variants.contains(default_variant.trim())
                {
                    return Err(StoreError::InvalidConfig(format!(
                        "model `{}` default reasoning variant is not in reasoning variants",
                        model.id
                    )));
                }
            }
        }
        if !ids.insert(provider.id.trim().to_string()) {
            return Err(StoreError::InvalidConfig(format!(
                "duplicate provider id `{}`",
                provider.id
            )));
        }
    }
    if let Some(default_provider_id) = request.default_provider_id.as_deref()
        && !default_provider_id.trim().is_empty()
        && !ids.contains(default_provider_id.trim())
    {
        return Err(StoreError::InvalidConfig(format!(
            "default provider `{default_provider_id}` is not in providers"
        )));
    }
    Ok(())
}

fn normalized_models(provider: &ProviderConfig) -> Vec<ModelConfig> {
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for model in provider.models.iter().cloned() {
        let id = model.id.trim().to_string();
        if !id.is_empty() && seen.insert(id.clone()) {
            models.push(ModelConfig { id, ..model });
        }
    }
    models
}
