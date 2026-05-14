use super::*;
use crate::events::event_session_id;
use crate::schema::{SCHEMA_VERSION, SETTING_SCHEMA_VERSION};
use mai_protocol::{
    AgentStatus, McpServerScope, McpServerTransport, MessageRole, ModelContentItem, ModelToolCall,
    ProjectCloneStatus, ProjectReviewOutcome, ProjectReviewRunStatus, ProjectReviewStatus,
    ProjectStatus, ServiceEventKind, TurnStatus,
};
use serde_json::json;
use tempfile::{TempDir, tempdir};

async fn store() -> (TempDir, ConfigStore) {
    let dir = tempdir().expect("tempdir");
    let store = ConfigStore::open_with_config_and_artifact_index_path(
        dir.path().join("config.sqlite3"),
        dir.path().join("config.toml"),
        dir.path().join("artifacts/index"),
    )
    .await
    .expect("open store");
    (dir, store)
}

#[tokio::test]
async fn open_in_data_dir_uses_standard_layout() {
    let dir = tempdir().expect("tempdir");
    let data_dir = dir.path().join(".mai-team");

    let store = ConfigStore::open_in_data_dir(&data_dir)
        .await
        .expect("open store");

    assert_eq!(store.path(), data_dir.join("mai-team.sqlite3"));
    assert_eq!(store.config_path(), data_dir.join("config.toml"));
    assert_eq!(
        store.artifact_index_dir(),
        data_dir.join("artifacts").join("index")
    );
}

fn provider(api_key: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        id: "openai".to_string(),
        kind: ProviderKind::Openai,
        name: "OpenAI".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        api_key: api_key.map(str::to_string),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        models: vec![test_model("gpt-5.5"), test_model("gpt-5.4")],
        default_model: "gpt-5.5".to_string(),
        enabled: true,
    }
}

fn test_model(id: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 400_000,
        output_tokens: 128_000,
        supports_tools: true,
        reasoning: Some(ModelReasoningConfig {
            default_variant: Some("medium".to_string()),
            variants: ["minimal", "low", "medium", "high"]
                .into_iter()
                .map(|id| ModelReasoningVariant {
                    id: id.to_string(),
                    label: None,
                    request: json!({
                        "reasoning": {
                            "effort": id,
                        },
                    }),
                })
                .collect(),
        }),
        options: serde_json::Value::Null,
        headers: BTreeMap::new(),
        wire_api: ModelWireApi::Responses,
        capabilities: ModelCapabilities::default(),
        request_policy: ModelRequestPolicy::default(),
    }
}

#[tokio::test]
async fn provider_response_is_redacted_and_preserves_empty_key() {
    let (_dir, store) = store().await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider(Some("secret"))],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save");
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider(Some(""))],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save preserve");

    let response = store.providers_response().await.expect("providers");
    assert!(response.providers[0].has_api_key);
    let resolved = store
        .resolve_provider(Some("openai"), Some("gpt-5.4"))
        .await
        .expect("resolve");
    assert_eq!(resolved.provider.api_key, "secret");
    assert_eq!(resolved.model.id, "gpt-5.4");
}

#[tokio::test]
async fn provider_cache_reloads_when_config_file_changes() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
        .await
        .expect("open");
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider(Some("first-secret"))],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save");
    assert_eq!(
        store
            .resolve_provider(Some("openai"), Some("gpt-5.4"))
            .await
            .expect("resolve")
            .provider
            .api_key,
        "first-secret"
    );

    let text = std::fs::read_to_string(&config_path)
        .expect("read config")
        .replace("first-secret", "second-secret-longer");
    std::fs::write(&config_path, text).expect("write config");
    assert_eq!(
        store
            .resolve_provider(Some("openai"), Some("gpt-5.4"))
            .await
            .expect("resolve changed")
            .provider
            .api_key,
        "second-secret-longer"
    );
}

#[tokio::test]
async fn artifacts_use_configured_index_dir() {
    let dir = tempdir().expect("tempdir");
    let index_dir = dir.path().join("artifact-index");
    let store = ConfigStore::open_with_config_and_artifact_index_path(
        dir.path().join("config.sqlite3"),
        dir.path().join("config.toml"),
        &index_dir,
    )
    .await
    .expect("open store");
    let task_id = Uuid::new_v4();
    let artifact = ArtifactInfo {
        id: "artifact-1".to_string(),
        agent_id: Uuid::new_v4(),
        task_id,
        name: "report.txt".to_string(),
        path: "/workspace/report.txt".to_string(),
        size_bytes: 7,
        created_at: Utc::now(),
    };

    store.save_artifact(&artifact).expect("save artifact");

    assert!(index_dir.join("artifact-1.json").exists());
    assert!(!dir.path().join("artifacts/index/artifact-1.json").exists());
    let artifacts = store.load_artifacts(&task_id).expect("load artifacts");
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].id, artifact.id);
    assert_eq!(artifacts[0].task_id, artifact.task_id);
    assert_eq!(artifacts[0].name, artifact.name);

    let all_artifacts = store.load_all_artifacts().expect("load all artifacts");
    assert_eq!(all_artifacts.len(), 1);
    assert_eq!(all_artifacts[0].id, artifact.id);
}

#[tokio::test]
async fn git_account_save_enters_verifying_and_clears_previous_error() {
    let (_dir, store) = store().await;
    let saved = store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    assert_eq!(saved.status, GitAccountStatus::Verifying);
    assert_eq!(saved.last_error, None);
    assert_eq!(saved.last_verified_at, None);

    let failed = store
        .update_git_account_verification(
            "account-1",
            None,
            GitTokenKind::Unknown,
            Vec::new(),
            GitAccountStatus::Failed,
            Some("bad token".to_string()),
        )
        .await
        .expect("mark failed");
    assert_eq!(failed.status, GitAccountStatus::Failed);
    assert!(failed.last_verified_at.is_some());

    let resaved = store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("new-secret".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("resave account");
    assert_eq!(resaved.status, GitAccountStatus::Verifying);
    assert_eq!(resaved.last_error, None);
    assert_eq!(resaved.last_verified_at, None);
}

#[tokio::test]
async fn github_app_relay_account_has_installation_metadata_without_token() {
    let (_dir, store) = store().await;
    let saved = store
        .upsert_github_app_relay_account(42, "octo-org", "relay-main", true)
        .await
        .expect("save relay account");

    assert_eq!(saved.id, "github-app-installation-42");
    assert_eq!(saved.provider, GitProvider::GithubAppRelay);
    assert_eq!(saved.status, GitAccountStatus::Verified);
    assert!(!saved.has_token);
    assert_eq!(saved.installation_id, Some(42));
    assert_eq!(saved.installation_account.as_deref(), Some("octo-org"));
    assert_eq!(saved.relay_id.as_deref(), Some("relay-main"));

    let loaded = store
        .git_account("github-app-installation-42")
        .await
        .expect("load")
        .expect("account");
    assert_eq!(loaded.provider, GitProvider::GithubAppRelay);
    assert_eq!(loaded.installation_id, Some(42));
}

#[tokio::test]
async fn git_account_delete_wins_over_late_verification_update() {
    let (_dir, store) = store().await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");

    let response = store
        .delete_git_account("account-1")
        .await
        .expect("delete account");
    assert!(response.accounts.is_empty());
    assert_eq!(response.default_account_id, None);

    let late_update = store
        .update_git_account_verification(
            "account-1",
            Some("octo".to_string()),
            GitTokenKind::Classic,
            vec!["repo".to_string()],
            GitAccountStatus::Verified,
            None,
        )
        .await;
    assert!(late_update.is_err());

    let response = store.list_git_accounts().await.expect("list accounts");
    assert!(response.accounts.is_empty());
    assert_eq!(response.default_account_id, None);
}

#[tokio::test]
async fn rejects_unknown_model() {
    let (_dir, store) = store().await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider(Some("secret"))],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save");
    assert!(
        store
            .resolve_provider(Some("openai"), Some("unknown"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn agent_config_defaults_when_missing_and_clears_invalid_json() {
    let (_dir, store) = store().await;
    assert_eq!(
        store.load_agent_config().await.expect("missing config"),
        AgentConfigRequest::default()
    );
    store
        .set_setting(SETTING_AGENT_CONFIG, "{not json")
        .await
        .expect("write invalid");
    assert_eq!(
        store.load_agent_config().await.expect("invalid config"),
        AgentConfigRequest::default()
    );
    assert_eq!(
        store
            .get_setting(SETTING_AGENT_CONFIG)
            .await
            .expect("setting"),
        None
    );
    store
        .set_setting(
            SETTING_AGENT_CONFIG,
            r#"{"research_agent":{"provider_id":"openai","model":"gpt-5.4"}}"#,
        )
        .await
        .expect("write old config");
    assert_eq!(
        store.load_agent_config().await.expect("old config"),
        AgentConfigRequest::default()
    );
    assert_eq!(
        store
            .get_setting(SETTING_AGENT_CONFIG)
            .await
            .expect("setting"),
        None
    );
}

#[tokio::test]
async fn agent_config_persists_and_reloads() {
    let (dir, store) = store().await;
    let config = AgentConfigRequest {
        planner: None,
        explorer: None,
        executor: Some(mai_protocol::AgentModelPreference {
            provider_id: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            reasoning_effort: Some("high".to_string()),
        }),
        reviewer: None,
    };
    store.save_agent_config(&config).await.expect("save config");
    drop(store);

    let reopened = ConfigStore::open_with_config_path(
        dir.path().join("config.sqlite3"),
        dir.path().join("config.toml"),
    )
    .await
    .expect("reopen");
    assert_eq!(
        reopened.load_agent_config().await.expect("load config"),
        config
    );
}

#[tokio::test]
async fn provider_presets_include_builtin_metadata() {
    let (_dir, store) = store().await;
    let presets = store.provider_presets_response();
    let openai = presets
        .providers
        .iter()
        .find(|provider| provider.kind == ProviderKind::Openai)
        .expect("openai preset");
    let deepseek = presets
        .providers
        .iter()
        .find(|provider| provider.kind == ProviderKind::Deepseek)
        .expect("deepseek preset");
    assert_eq!(openai.default_model, "gpt-5.5");
    assert!(openai.models.iter().any(|model| model.id == "gpt-5.4-mini"));
    assert_eq!(deepseek.default_model, "deepseek-v4-flash");
    let v4_pro = deepseek
        .models
        .iter()
        .find(|model| model.id == "deepseek-v4-pro")
        .expect("deepseek v4 pro");
    assert_eq!(v4_pro.context_tokens, DEEPSEEK_V4_CONTEXT_TOKENS);
    assert_eq!(v4_pro.output_tokens, DEEPSEEK_V4_OUTPUT_TOKENS);
    let reasoning = v4_pro.reasoning.as_ref().expect("reasoning variants");
    assert_eq!(reasoning.default_variant.as_deref(), Some("high"));
    assert_eq!(
        reasoning
            .variants
            .iter()
            .map(|variant| variant.id.as_str())
            .collect::<Vec<_>>(),
        vec!["high", "max"]
    );
    for id in ["deepseek-v4-flash", "deepseek-v4-pro"] {
        let model = deepseek
            .models
            .iter()
            .find(|model| model.id == id)
            .expect("deepseek v4 model");
        assert_eq!(model.context_tokens, DEEPSEEK_V4_CONTEXT_TOKENS);
        assert_eq!(model.output_tokens, DEEPSEEK_V4_OUTPUT_TOKENS);
    }
    let v4_flash = deepseek
        .models
        .iter()
        .find(|model| model.id == "deepseek-v4-flash")
        .expect("deepseek v4 flash");
    assert!(v4_flash.reasoning.is_some());
    assert!(v4_flash.capabilities.reasoning_replay);
    assert_eq!(deepseek.models.len(), 2);
    let mimo_presets: Vec<_> = presets
        .providers
        .iter()
        .filter(|provider| provider.kind == ProviderKind::Mimo)
        .collect();
    assert_eq!(
        mimo_presets.len(),
        2,
        "expected mimo-api and mimo-token-plan presets"
    );
    let mimo_api = mimo_presets
        .iter()
        .find(|p| p.id == "mimo-api")
        .expect("mimo-api preset");
    let mimo_tp = mimo_presets
        .iter()
        .find(|p| p.id == "mimo-token-plan")
        .expect("mimo-token-plan preset");
    assert_eq!(mimo_api.base_url, "https://api.xiaomimimo.com/v1");
    assert_eq!(mimo_tp.base_url, "https://token-plan-cn.xiaomimimo.com/v1");
    assert_eq!(mimo_api.default_model, "mimo-v2.5-pro");

    for (id, context_tokens, output_tokens) in [
        ("mimo-v2.5-pro", 1_000_000, 131_072),
        ("mimo-v2.5", 1_000_000, 131_072),
        ("mimo-v2-pro", 1_000_000, 131_072),
        ("mimo-v2-omni", 256_000, 131_072),
        ("mimo-v2-flash", 256_000, 65_536),
    ] {
        let model = mimo_api
            .models
            .iter()
            .find(|model| model.id == id)
            .expect("mimo model");
        assert_eq!(model.context_tokens, context_tokens);
        assert_eq!(model.output_tokens, output_tokens);
    }

    let mimo_pro = mimo_api
        .models
        .iter()
        .find(|model| model.id == "mimo-v2.5-pro")
        .expect("mimo-v2.5-pro");
    assert!(mimo_pro.reasoning.is_some());
    assert_eq!(
        mimo_pro.request_policy.max_tokens_field,
        "max_completion_tokens"
    );
    let mimo_flash = mimo_api
        .models
        .iter()
        .find(|model| model.id == "mimo-v2-flash")
        .expect("mimo-v2-flash");
    assert!(mimo_flash.reasoning.is_none());
}

#[tokio::test]
async fn provider_toml_preserves_custom_model_metadata() {
    let (_dir, store) = store().await;
    let mut provider = provider(Some("secret"));
    let mut custom = test_model("custom-chat");
    custom.context_tokens = 123_456;
    custom.output_tokens = 4_096;
    custom.supports_tools = false;
    custom.reasoning = None;
    custom.options = json!({ "temperature": 0.2 });
    custom
        .headers
        .insert("X-Test-Model".to_string(), "custom".to_string());
    provider.models.push(custom);
    provider.default_model = "custom-chat".to_string();
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save");

    let response = store.providers_response().await.expect("providers");
    let model = response.providers[0]
        .models
        .iter()
        .find(|model| model.id == "custom-chat")
        .expect("custom model");
    assert_eq!(model.context_tokens, 123_456);
    assert!(!model.supports_tools);
    assert_eq!(model.options["temperature"], json!(0.2));
    assert_eq!(model.headers["X-Test-Model"], "custom");
}

#[tokio::test]
async fn legacy_deepseek_models_migrate_to_chat_policy() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
            default_provider_id = "deepseek"

            [providers.deepseek]
            kind = "deepseek"
            name = "DeepSeek"
            base_url = "https://api.deepseek.com"
            api_key = "secret"
            default_model = "deepseek-v4-pro"
            enabled = true

            [providers.deepseek.models.deepseek-v4-pro]
            name = "deepseek-v4-pro"
            context_tokens = 1000000
            output_tokens = 384000
            supports_tools = true
        "#,
    )
    .expect("write config");
    let store = ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
        .await
        .expect("open");

    let response = store.providers_response().await.expect("providers");
    let model = response.providers[0].models.first().expect("model");
    assert_eq!(model.wire_api, ModelWireApi::ChatCompletions);
    assert!(!model.capabilities.continuation);
    assert!(model.capabilities.reasoning_replay);
    assert_eq!(model.request_policy.store, None);
    assert_eq!(model.request_policy.max_tokens_field, "max_tokens");
}

#[tokio::test]
async fn legacy_mimo_models_migrate_to_official_chat_policy() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
            default_provider_id = "mimo-token-plan"

            [providers.mimo-token-plan]
            kind = "mimo"
            name = "MiMo Token Plan"
            base_url = "https://token-plan-cn.xiaomimimo.com/v1"
            api_key = "secret"
            default_model = "mimo-v2.5-pro"
            enabled = true

            [providers.mimo-token-plan.models."mimo-v2.5-pro"]
            name = "mimo-v2.5-pro"
            context_tokens = 1000000
            output_tokens = 131072
            supports_tools = true
            wire_api = "chat_completions"

            [providers.mimo-token-plan.models."mimo-v2.5-pro".request_policy]
            max_tokens_field = "max_tokens"
        "#,
    )
    .expect("write config");
    let store = ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
        .await
        .expect("open");

    let response = store.providers_response().await.expect("providers");
    let model = response.providers[0].models.first().expect("model");
    assert_eq!(model.wire_api, ModelWireApi::ChatCompletions);
    assert_eq!(
        model.request_policy.max_tokens_field,
        "max_completion_tokens"
    );
}

#[tokio::test]
async fn old_provider_toml_schema_is_rebuilt() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
            [mcp_servers.demo]
            command = "demo-mcp"
        "#,
    )
    .expect("write old config");
    let store = ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
        .await
        .expect("open");

    assert_eq!(store.provider_count().await.expect("count"), 0);
    assert!(!config_path.exists());
}

#[tokio::test]
async fn old_reasoning_toml_schema_is_rebuilt() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
            default_provider_id = "deepseek"

            [providers.deepseek]
            kind = "deepseek"
            name = "DeepSeek"
            base_url = "https://api.deepseek.com"
            api_key = "secret"
            default_model = "deepseek-v4-pro"
            enabled = true

            [providers.deepseek.models.deepseek-v4-pro]
            name = "deepseek-v4-pro"
            context_tokens = 128000
            output_tokens = 8192
            supports_tools = true
            supports_reasoning = true
            reasoning_efforts = ["high", "max"]
            default_reasoning_effort = "high"
        "#,
    )
    .expect("write old config");
    let store = ConfigStore::open_with_config_path(dir.path().join("config.sqlite3"), &config_path)
        .await
        .expect("open");

    assert_eq!(store.provider_count().await.expect("count"), 0);
    assert!(!config_path.exists());
}

#[tokio::test]
async fn runtime_snapshot_survives_reopen() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let store = ConfigStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
        .await
        .expect("open");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let now = Utc::now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "agent-test".to_string(),
        status: AgentStatus::Completed,
        container_id: Some("container".to_string()),
        docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.2".to_string(),
        reasoning_effort: Some("high".to_string()),
        created_at: now,
        updated_at: now,
        current_turn: Some(turn_id),
        last_error: None,
        token_usage: TokenUsage {
            input_tokens: 1,
            cached_input_tokens: 4,
            output_tokens: 2,
            reasoning_output_tokens: 5,
            total_tokens: 3,
        },
    };
    let session = AgentSessionSummary {
        id: session_id,
        title: "Chat 1".to_string(),
        created_at: now,
        updated_at: now,
        message_count: 0,
    };
    let message = AgentMessage {
        role: MessageRole::User,
        content: "hello".to_string(),
        created_at: now,
    };
    let history = [
        ModelInputItem::Message {
            role: "user".to_string(),
            content: vec![ModelContentItem::InputText {
                text: "hello".to_string(),
            }],
        },
        ModelInputItem::AssistantTurn {
            content: None,
            reasoning_content: Some("thinking".to_string()),
            tool_calls: vec![ModelToolCall {
                call_id: "call_1".to_string(),
                name: "container_exec".to_string(),
                arguments: "{\"command\":\"pwd\"}".to_string(),
            }],
        },
    ];
    let event = ServiceEvent {
        sequence: 7,
        timestamp: now,
        kind: ServiceEventKind::AgentMessage {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            role: MessageRole::User,
            content: "hello".to_string(),
        },
    };

    store
        .save_agent(&summary, Some("system"))
        .await
        .expect("save agent");
    store
        .save_agent_session(agent_id, &session)
        .await
        .expect("save session");
    store
        .append_agent_message(agent_id, session_id, 0, &message)
        .await
        .expect("message");
    store
        .append_agent_history_item(agent_id, session_id, 0, &history[0])
        .await
        .expect("history");
    store
        .append_agent_history_item(agent_id, session_id, 1, &history[1])
        .await
        .expect("history");
    store.append_service_event(&event).await.expect("event");
    drop(store);

    let reopened = ConfigStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
        .await
        .expect("reopen");
    let snapshot = reopened.load_runtime_snapshot(500).await.expect("snapshot");
    assert_eq!(snapshot.next_sequence, 8);
    assert_eq!(snapshot.agents.len(), 1);
    assert_eq!(snapshot.agents[0].summary.name, "agent-test");
    assert_eq!(snapshot.agents[0].summary.token_usage.input_tokens, 1);
    assert_eq!(
        snapshot.agents[0].summary.token_usage.cached_input_tokens,
        4
    );
    assert_eq!(snapshot.agents[0].summary.token_usage.output_tokens, 2);
    assert_eq!(
        snapshot.agents[0]
            .summary
            .token_usage
            .reasoning_output_tokens,
        5
    );
    assert_eq!(snapshot.agents[0].summary.token_usage.total_tokens, 3);
    assert_eq!(
        snapshot.agents[0].summary.docker_image,
        "ghcr.io/rcore-os/tgoskits-container:latest"
    );
    assert_eq!(snapshot.agents[0].system_prompt.as_deref(), Some("system"));
    assert_eq!(snapshot.agents[0].sessions.len(), 1);
    assert_eq!(snapshot.agents[0].sessions[0].summary.title, "Chat 1");
    assert_eq!(snapshot.agents[0].sessions[0].summary.message_count, 1);
    assert_eq!(snapshot.agents[0].sessions[0].history.len(), 2);
    assert_eq!(snapshot.agents[0].sessions[0].last_context_tokens, None);
    assert!(matches!(
        &snapshot.agents[0].sessions[0].history[1],
        ModelInputItem::AssistantTurn {
            reasoning_content: Some(reasoning),
            tool_calls,
            ..
        } if reasoning == "thinking"
            && tool_calls.len() == 1
            && tool_calls[0].call_id == "call_1"
    ));
    assert_eq!(snapshot.recent_events.len(), 1);
    assert_eq!(
        event_session_id(&snapshot.recent_events[0]),
        Some(session_id)
    );

    reopened.delete_agent(agent_id).await.expect("delete agent");
    let snapshot = reopened.load_runtime_snapshot(500).await.expect("snapshot");
    assert!(snapshot.agents.is_empty());
    assert!(
        reopened
            .load_agent_sessions(agent_id)
            .await
            .expect("sessions")
            .is_empty()
    );
    assert!(
        reopened
            .load_agent_messages(agent_id, session_id)
            .await
            .expect("messages")
            .is_empty()
    );
    assert!(
        reopened
            .load_agent_history(agent_id, session_id)
            .await
            .expect("history")
            .is_empty()
    );
}

#[tokio::test]
async fn replace_agent_history_only_replaces_target_session() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let store = ConfigStore::open_with_config_path(&db_path, dir.path().join("config.toml"))
        .await
        .expect("open");
    let agent_id = Uuid::new_v4();
    let first_session_id = Uuid::new_v4();
    let second_session_id = Uuid::new_v4();
    let now = Utc::now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "agent-test".to_string(),
        status: AgentStatus::Completed,
        container_id: None,
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.2".to_string(),
        reasoning_effort: None,
        created_at: now,
        updated_at: now,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    store.save_agent(&summary, None).await.expect("save agent");
    for session_id in [first_session_id, second_session_id] {
        store
            .save_agent_session(
                agent_id,
                &AgentSessionSummary {
                    id: session_id,
                    title: "Chat".to_string(),
                    created_at: now,
                    updated_at: now,
                    message_count: 0,
                },
            )
            .await
            .expect("save session");
    }
    store
        .append_agent_history_item(
            agent_id,
            first_session_id,
            0,
            &ModelInputItem::user_text("old first"),
        )
        .await
        .expect("first history");
    store
        .append_agent_history_item(
            agent_id,
            second_session_id,
            0,
            &ModelInputItem::user_text("old second"),
        )
        .await
        .expect("second history");

    store
        .replace_agent_history(
            agent_id,
            first_session_id,
            &[ModelInputItem::user_text("new")],
        )
        .await
        .expect("replace");
    let first = store
        .load_agent_history(agent_id, first_session_id)
        .await
        .expect("first");
    let second = store
        .load_agent_history(agent_id, second_session_id)
        .await
        .expect("second");
    assert!(matches!(
        &first[0],
        ModelInputItem::Message { content, .. }
            if matches!(&content[0], ModelContentItem::InputText { text } if text == "new")
    ));
    assert!(matches!(
        &second[0],
        ModelInputItem::Message { content, .. }
            if matches!(&content[0], ModelContentItem::InputText { text } if text == "old second")
    ));
}

#[tokio::test]
async fn session_context_tokens_survive_reopen_and_clear() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let now = Utc::now();
    store
        .save_agent(
            &AgentSummary {
                id: agent_id,
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                name: "agent-test".to_string(),
                status: AgentStatus::Completed,
                container_id: None,
                docker_image: "ubuntu:latest".to_string(),
                provider_id: "openai".to_string(),
                provider_name: "OpenAI".to_string(),
                model: "gpt-5.2".to_string(),
                reasoning_effort: None,
                created_at: now,
                updated_at: now,
                current_turn: None,
                last_error: None,
                token_usage: TokenUsage::default(),
            },
            None,
        )
        .await
        .expect("save agent");
    store
        .save_agent_session(
            agent_id,
            &AgentSessionSummary {
                id: session_id,
                title: "Chat".to_string(),
                created_at: now,
                updated_at: now,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
    store
        .save_session_context_tokens(agent_id, session_id, 1234)
        .await
        .expect("save tokens");
    drop(store);

    let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("reopen");
    let snapshot = reopened.load_runtime_snapshot(10).await.expect("snapshot");
    assert_eq!(
        snapshot.agents[0].sessions[0].last_context_tokens,
        Some(1234)
    );
    reopened
        .clear_session_context_tokens(agent_id, session_id)
        .await
        .expect("clear");
    let snapshot = reopened.load_runtime_snapshot(10).await.expect("snapshot");
    assert_eq!(snapshot.agents[0].sessions[0].last_context_tokens, None);
}

#[tokio::test]
async fn project_review_runs_round_trip_and_prune() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let reviewer_agent_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let started_at = Utc::now() - chrono::TimeDelta::days(1);
    let finished_at = started_at + chrono::TimeDelta::minutes(3);
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: Some(reviewer_agent_id),
                turn_id: Some(turn_id),
                started_at,
                finished_at: Some(finished_at),
                status: ProjectReviewRunStatus::Completed,
                outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
                pr: Some(42),
                summary: Some("approved".to_string()),
                error: None,
            },
            messages: vec![AgentMessage {
                role: MessageRole::Assistant,
                content: "done".to_string(),
                created_at: finished_at,
            }],
            events: vec![ServiceEvent {
                sequence: 1,
                timestamp: finished_at,
                kind: ServiceEventKind::TurnCompleted {
                    agent_id: reviewer_agent_id,
                    session_id: None,
                    turn_id,
                    status: TurnStatus::Completed,
                },
            }],
        })
        .await
        .expect("save run");

    let runs = store
        .load_project_review_runs(project_id, None, 0, 10)
        .await
        .expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].pr, Some(42));
    assert_eq!(runs[0].outcome, Some(ProjectReviewOutcome::ReviewSubmitted));
    let detail = store
        .load_project_review_run(project_id, run_id)
        .await
        .expect("detail")
        .expect("run exists");
    assert_eq!(detail.messages[0].content, "done");
    assert_eq!(detail.events.len(), 1);

    let removed = store
        .prune_project_review_runs_before(Utc::now() - chrono::TimeDelta::days(2))
        .await
        .expect("no prune");
    assert_eq!(removed, 0);
    let removed = store
        .prune_project_review_runs_before(Utc::now())
        .await
        .expect("prune");
    assert_eq!(removed, 1);
    assert!(
        store
            .load_project_review_run(project_id, run_id)
            .await
            .expect("load")
            .is_none()
    );
}

#[tokio::test]
async fn agent_logs_round_trip_filter_and_prune() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let old_time = Utc::now() - chrono::TimeDelta::days(6);
    let new_time = Utc::now();

    store
        .append_agent_log_entry(&AgentLogEntry {
            id: Uuid::new_v4(),
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: "info".to_string(),
            category: "tool".to_string(),
            message: "tool started".to_string(),
            details: json!({ "call_id": "call_1" }),
            timestamp: new_time,
        })
        .await
        .expect("save new log");
    store
        .append_agent_log_entry(&AgentLogEntry {
            id: Uuid::new_v4(),
            agent_id,
            session_id: None,
            turn_id: None,
            level: "warn".to_string(),
            category: "model".to_string(),
            message: "old".to_string(),
            details: json!({}),
            timestamp: old_time,
        })
        .await
        .expect("save old log");

    let logs = store
        .list_agent_logs(
            agent_id,
            AgentLogFilter {
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                level: Some("info".to_string()),
                category: Some("tool".to_string()),
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("list logs");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "tool started");
    assert_eq!(logs[0].details["call_id"], "call_1");

    let removed = store
        .prune_agent_logs_before(Utc::now() - chrono::TimeDelta::days(5))
        .await
        .expect("prune logs");
    assert_eq!(removed, 1);
    let remaining = store
        .list_agent_logs(
            agent_id,
            AgentLogFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("remaining logs");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].category, "tool");
}

#[tokio::test]
async fn tool_traces_round_trip_filter_and_prune() {
    let (_dir, store) = store().await;
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let old_time = Utc::now() - chrono::TimeDelta::days(6);
    let new_time = Utc::now();

    let trace = ToolTraceDetail {
        agent_id,
        session_id: Some(session_id),
        turn_id: Some(turn_id),
        call_id: "call_1".to_string(),
        tool_name: "container_exec".to_string(),
        arguments: json!({ "command": "printf hi" }),
        output: r#"{"status":0,"stdout":"hi","stderr":""}"#.to_string(),
        success: true,
        duration_ms: Some(42),
        started_at: Some(new_time),
        completed_at: Some(new_time),
        output_preview: "hi".to_string(),
        output_artifacts: vec![ToolOutputArtifactInfo {
            id: "artifact-1".to_string(),
            call_id: "call_1".to_string(),
            agent_id,
            name: "stdout.txt".to_string(),
            stream: "stdout".to_string(),
            size_bytes: 2,
            created_at: new_time,
        }],
    };
    store
        .save_tool_trace_started(&trace, new_time)
        .await
        .expect("save start");
    store
        .save_tool_trace_completed(&trace, new_time, new_time)
        .await
        .expect("save completed");
    store
        .save_tool_trace_completed(
            &ToolTraceDetail {
                call_id: "call_old".to_string(),
                started_at: Some(old_time),
                completed_at: Some(old_time),
                output_preview: "old".to_string(),
                ..trace.clone()
            },
            old_time,
            old_time,
        )
        .await
        .expect("save old");

    let summaries = store
        .list_tool_traces(
            agent_id,
            ToolTraceFilter {
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("list traces");
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].call_id, "call_1");
    assert_eq!(summaries[0].duration_ms, Some(42));

    let loaded = store
        .load_tool_trace(agent_id, Some(session_id), "call_1")
        .await
        .expect("load trace")
        .expect("trace");
    assert_eq!(loaded.arguments["command"], "printf hi");
    assert_eq!(loaded.output_preview, "hi");
    assert_eq!(loaded.output_artifacts.len(), 1);
    assert_eq!(loaded.output_artifacts[0].stream, "stdout");

    let removed = store
        .prune_tool_traces_before(Utc::now() - chrono::TimeDelta::days(5))
        .await
        .expect("prune traces");
    assert_eq!(removed, 1);
    let remaining = store
        .list_tool_traces(
            agent_id,
            ToolTraceFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("remaining traces");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].call_id, "call_1");
}

#[tokio::test]
async fn tool_traces_keep_same_call_id_for_different_agents() {
    let (_dir, store) = store().await;
    let first_agent_id = Uuid::new_v4();
    let second_agent_id = Uuid::new_v4();
    let first_session_id = Uuid::new_v4();
    let second_session_id = Uuid::new_v4();
    let timestamp = Utc::now();

    for (agent_id, session_id, command) in [
        (first_agent_id, first_session_id, "pwd"),
        (second_agent_id, second_session_id, "ls"),
    ] {
        store
            .save_tool_trace_completed(
                &ToolTraceDetail {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id: Some(Uuid::new_v4()),
                    call_id: "call_duplicate".to_string(),
                    tool_name: "container_exec".to_string(),
                    arguments: json!({ "command": command }),
                    output: format!("{{\"command\":\"{command}\"}}"),
                    success: true,
                    duration_ms: Some(1),
                    started_at: Some(timestamp),
                    completed_at: Some(timestamp),
                    output_preview: command.to_string(),
                    output_artifacts: Vec::new(),
                },
                timestamp,
                timestamp,
            )
            .await
            .expect("save trace");
    }

    let first = store
        .load_tool_trace(first_agent_id, Some(first_session_id), "call_duplicate")
        .await
        .expect("load first")
        .expect("first trace");
    let second = store
        .load_tool_trace(second_agent_id, Some(second_session_id), "call_duplicate")
        .await
        .expect("load second")
        .expect("second trace");

    assert_eq!(first.arguments["command"], "pwd");
    assert_eq!(second.arguments["command"], "ls");
}

#[tokio::test]
async fn delete_project_removes_review_runs() {
    let (_dir, store) = store().await;
    let project_id = Uuid::new_v4();
    let maintainer_agent_id = Uuid::new_v4();
    let timestamp = Utc::now();
    store
        .save_project(&ProjectSummary {
            id: project_id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account-1".to_string()),
            repository_id: 42,
            installation_id: 0,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "ubuntu:latest".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id,
            created_at: timestamp,
            updated_at: timestamp,
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Waiting,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        })
        .await
        .expect("save project");
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: Uuid::new_v4(),
                project_id,
                reviewer_agent_id: None,
                turn_id: None,
                started_at: timestamp,
                finished_at: None,
                status: ProjectReviewRunStatus::Syncing,
                outcome: None,
                pr: None,
                summary: None,
                error: None,
            },
            messages: Vec::new(),
            events: Vec::new(),
        })
        .await
        .expect("save run");
    store
        .delete_project(project_id)
        .await
        .expect("delete project");
    assert!(
        store
            .load_project_review_runs(project_id, None, 0, 10)
            .await
            .expect("runs")
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_sqlite_file_is_rebuilt() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config.sqlite3");
    std::fs::write(&path, b"not sqlite").expect("write invalid old db");
    let store = ConfigStore::open_with_config_path(&path, dir.path().join("config.toml"))
        .await
        .expect("rebuild");
    assert_eq!(store.provider_count().await.expect("count"), 0);
}

#[tokio::test]
async fn skills_config_persists_in_settings() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open");
    let config = SkillsConfigRequest {
        config: vec![mai_protocol::SkillConfigEntry {
            name: Some("demo".to_string()),
            path: None,
            enabled: false,
        }],
    };
    store
        .save_skills_config(&config)
        .await
        .expect("save skills config");
    drop(store);

    let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("reopen");
    assert_eq!(
        reopened
            .load_skills_config()
            .await
            .expect("load skills config"),
        config
    );
    assert!(
        !config_path.exists(),
        "provider config file should be untouched"
    );
}

#[tokio::test]
async fn schema_version_mismatch_rebuilds_database() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("config.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open");
    store
        .save_mcp_servers(&BTreeMap::from([(
            "demo".to_string(),
            McpServerConfig {
                command: Some("demo-mcp".to_string()),
                ..Default::default()
            },
        )]))
        .await
        .expect("save server");
    store
        .set_setting(SETTING_SCHEMA_VERSION, "4")
        .await
        .expect("mark old schema");
    drop(store);

    let reopened = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("reopen");
    assert_eq!(
        reopened
            .get_setting(SETTING_SCHEMA_VERSION)
            .await
            .expect("schema marker")
            .as_deref(),
        Some(SCHEMA_VERSION)
    );
    assert!(
        reopened
            .list_mcp_servers()
            .await
            .expect("servers")
            .is_empty()
    );
}

#[tokio::test]
async fn mcp_servers_round_trip_json_config() {
    let (_dir, store) = store().await;
    let servers = BTreeMap::from([
        (
            "stdio".to_string(),
            McpServerConfig {
                scope: McpServerScope::Project,
                command: Some("demo-mcp".to_string()),
                args: vec!["--stdio".to_string()],
                env: BTreeMap::from([("A".to_string(), "B".to_string())]),
                cwd: Some("/workspace".to_string()),
                enabled_tools: Some(vec!["echo".to_string()]),
                disabled_tools: vec!["danger".to_string()],
                startup_timeout_secs: Some(3),
                tool_timeout_secs: Some(7),
                ..Default::default()
            },
        ),
        (
            "http".to_string(),
            McpServerConfig {
                transport: McpServerTransport::StreamableHttp,
                url: Some("https://example.com/mcp".to_string()),
                headers: BTreeMap::from([("X-Test".to_string(), "yes".to_string())]),
                bearer_token_env: Some("MCP_TOKEN".to_string()),
                enabled: false,
                required: true,
                ..Default::default()
            },
        ),
    ]);

    store.save_mcp_servers(&servers).await.expect("save");
    let loaded = store.list_mcp_servers().await.expect("load");

    assert_eq!(loaded, servers);
    assert_eq!(
        loaded.get("stdio").map(|config| config.scope),
        Some(McpServerScope::Project)
    );
}
