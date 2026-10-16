//! Runtime 装配与 fixture provider 配置。
//!
//! 装配顺序与产品服务一致：打开前一领域驱动 seed 的产品库 → 用部署策略文件创建
//! `AgentRuntime`（恢复准入、恢复收据、启动参考 worker）→ 写入 fixture provider 与
//! 四个角色路由 → 逐个场景驱动真实 turn。
//!
//! 驱动**只改两处持久化状态**：provider/角色模型（本文件）与 `lifecycle` 在启动前给
//! 父 Agent 冻结的被授权 Profile。其余夹具记录、源码与容器只读复用。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use mai_docker::{ContainerEnvironmentFacts, DockerClient};
use mai_protocol::{
    AgentConfigRequest, AgentConfigResponse, AgentDetail, AgentId, AgentSummary,
    ProjectReferenceStatus, ProviderConfig, ProviderConfigSource, ProvidersConfigRequest,
    ReferenceSource, ReferenceStatusQuery,
};
use mai_runtime::{AgentRuntime, RuntimeConfig};
use mai_store::MaiStore;
use pl_model::config::{ModelRouteConfig, ProviderConfig as PlProviderConfig, ProviderId};
use pl_model::model::{ModelInfo, ModelTransportProfile};
use pl_model::provider::{ProviderConnectionMode, ProviderEndpoint, ProviderWireProtocol};
use serde_json::{Value, json};

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::runtime_config::{DriverConfig, MODEL_SLUG, PROVIDER_ID, PROVIDER_NAME, PROVIDER_TOKEN};

/// 已装配好的真实 Runtime 与它的产品库/Docker 客户端。
pub struct RuntimeHost {
    pub config: DriverConfig,
    pub docker: DockerClient,
    pub store: Arc<MaiStore>,
    pub runtime: Arc<AgentRuntime>,
}

/// 打开前一领域驱动 seed 的产品库（`root/data`）。
pub async fn open_store(config: &DriverConfig) -> Result<Arc<MaiStore>> {
    let store = MaiStore::open_in_data_dir(&config.data_dir)
        .await
        .map_err(|cause| {
            error(
                "store_open_failed",
                format!(
                    "无法打开产品库 {}（前一领域驱动应已 seed）：{cause}",
                    config.data_dir.display()
                ),
            )
        })?;
    let store = Arc::new(store);
    crate::fixture_setup::prepare(config, &store).await?;
    Ok(store)
}

/// 装配真实 `AgentRuntime`；不写 provider 配置（由 [`RuntimeHost::configure_models`] 显式做）。
pub async fn start(config: &DriverConfig, store: Arc<MaiStore>) -> Result<RuntimeHost> {
    for (role, dir) in [
        ("项目目录", &config.projects_root),
        ("缓存目录", &config.cache_root),
        ("产物目录", &config.artifacts_root),
        ("会话目录", &config.sessions_root),
    ] {
        std::fs::create_dir_all(dir).map_err(|cause| {
            error(
                "runtime_dir_unwritable",
                format!("无法创建{role} {}：{cause}", dir.display()),
            )
        })?;
    }
    let docker = DockerClient::new(config.agent_image.clone());
    let runtime = AgentRuntime::new(
        docker.clone(),
        Arc::clone(&store),
        RuntimeConfig {
            references_policy_path: Some(config.policy_path.clone()),
            reference_application_observer: None,
            repo_root: config.root.clone(),
            projects_root: config.projects_root.clone(),
            sessions_root: config.sessions_root.clone(),
            cache_root: config.cache_root.clone(),
            artifact_files_root: config.artifacts_root.clone(),
            sidecar_image: config.sidecar_image.clone(),
            github_api_base_url: None,
            git_binary: None,
            system_skills_root: Some(config.system_skills_root.clone()),
            system_agents_root: Some(config.system_agents_root.clone()),
        },
    )
    .await
    .map_err(|cause| {
        error(
            "runtime_bootstrap_failed",
            format!("AgentRuntime 装配失败（部署策略/镜像/库状态）：{cause}"),
        )
    })?;
    Ok(RuntimeHost {
        config: config.clone(),
        docker,
        store,
        runtime,
    })
}

impl RuntimeHost {
    /// 写入 fixture provider 与四个角色路由，并核对生效路由的 transport 是 Responses HTTP。
    pub async fn configure_models(&self) -> Result<Value> {
        // 先加入 fixture provider，再切角色路由，避免替换 catalog 时留下悬空路由。
        let current = self.runtime.providers_response().await.map_err(|cause| {
            error(
                "provider_read_failed",
                format!("读取 provider 失败：{cause}"),
            )
        })?;
        let mut entries = current
            .providers
            .into_iter()
            .filter(|provider| provider.id != PROVIDER_ID)
            .map(|provider| {
                let source = match provider.config.preset_id() {
                    Some(preset) => ProviderConfigSource::Preset {
                        preset_id: preset.to_string(),
                        name: provider.config.name.clone(),
                        base_url: provider.config.base_url.clone(),
                        bearer_token: None,
                        bearer_token_env: provider.config.bearer_token_env.clone(),
                        http_headers: None,
                    },
                    None => ProviderConfigSource::Custom {
                        config: provider.config,
                    },
                };
                ProviderConfig {
                    id: provider.id,
                    source,
                }
            })
            .collect::<Vec<_>>();
        entries.push(acceptance_provider(&self.config)?);
        let providers = self
            .runtime
            .update_providers(ProvidersConfigRequest { providers: entries })
            .await
            .map_err(|cause| {
                error(
                    "provider_update_failed",
                    format!("写入 provider 失败：{cause}"),
                )
            })?;
        let agent_config = self
            .runtime
            .update_agent_config(acceptance_routes()?)
            .await
            .map_err(|cause| {
                error(
                    "agent_model_update_failed",
                    format!("写入角色模型失败：{cause}"),
                )
            })?;
        ensure_responses_http(&agent_config)?;
        let transport = agent_config
            .effective_executor
            .as_ref()
            .map(|effective| transport_label(effective.transport.clone()));
        Ok(json!({
            "provider_id": PROVIDER_ID,
            "provider_count": providers.providers.len(),
            "base_url": &self.config.provider_base_url,
            "model": MODEL_SLUG,
            "effective_executor_transport": transport,
            "roles": {
                "planner": agent_config.effective_planner.as_ref().map(|effective| effective.provider_id.clone()),
                "explorer": agent_config.effective_explorer.as_ref().map(|effective| effective.provider_id.clone()),
                "executor": agent_config.effective_executor.as_ref().map(|effective| effective.provider_id.clone()),
                "reviewer": agent_config.effective_reviewer.as_ref().map(|effective| effective.provider_id.clone()),
            },
            "validation_error": agent_config.validation_error,
        }))
    }

    /// 内存中的 Agent 摘要（不启动 Thread、不动容器）。
    pub async fn agent(&self, agent_id: AgentId) -> Result<AgentSummary> {
        self.runtime
            .list_agents()
            .await
            .into_iter()
            .find(|summary| summary.id == agent_id)
            .ok_or_else(|| {
                error(
                    "agent_missing",
                    format!("产品库里没有 Agent {agent_id}（前一领域驱动的夹具缺失）"),
                )
            })
    }

    /// 产品投影的 Agent 详情（会确保驻留 Thread，必要时启动真实容器）。
    pub async fn agent_detail(&self, agent_id: AgentId) -> Result<AgentDetail> {
        self.runtime.get_agent(agent_id).await.map_err(|cause| {
            error(
                "agent_detail_failed",
                format!("读取 Agent {agent_id} 的 Thread 投影失败：{cause}"),
            )
        })
    }

    /// Agent 当前发布的物理容器：绑定优先，其次 Agent 摘要投影。
    pub async fn container_id(&self, agent_id: AgentId) -> Result<Option<String>> {
        if let Some(binding) = self
            .store
            .reference_binding(agent_id)
            .await
            .map_err(|cause| error("binding_unreadable", format!("读取环境绑定失败：{cause}")))?
        {
            return Ok(Some(binding.container_id));
        }
        Ok(self.agent(agent_id).await?.container_id)
    }

    /// 真实容器事实（挂载/运行态/镜像/换代标识）。
    pub async fn container_facts(&self, container_id: &str) -> Result<ContainerEnvironmentFacts> {
        self.docker
            .inspect_container_environment(container_id)
            .await
            .map_err(|cause| {
                error(
                    "container_inspect_failed",
                    format!("无法 inspect 容器 {container_id}：{cause}"),
                )
            })
    }

    /// `/references/**` 的容器内目标集合（真实挂载事实，不看数据库）。
    pub async fn reference_targets(&self, agent_id: AgentId) -> Result<Vec<String>> {
        let Some(container_id) = self.container_id(agent_id).await? else {
            return Ok(Vec::new());
        };
        let facts = self.container_facts(&container_id).await?;
        Ok(reference_targets_of(&facts))
    }

    /// 项目参考应用状态。
    pub async fn reference_status(&self, project: ProjectKind) -> Result<ProjectReferenceStatus> {
        self.runtime
            .project_reference_status(project.id(), ReferenceStatusQuery::default())
            .await
            .map_err(|cause| {
                error(
                    "reference_status_failed",
                    format!("读取 {} 的参考应用状态失败：{cause}", project.slug()),
                )
            })
    }

    /// 已登记参考源。
    pub async fn reference_sources(&self) -> Result<Vec<ReferenceSource>> {
        self.runtime
            .reference_catalog()
            .sources()
            .await
            .map_err(|cause| {
                error(
                    "reference_sources_failed",
                    format!("读取登记源失败：{cause}"),
                )
            })
    }

    /// 关闭驻留 Thread 与会话 writer；失败只记录，不掩盖前面的验收结论。
    pub async fn shutdown(&self) {
        if let Err(cause) = self.runtime.shutdown().await {
            eprintln!("Runtime shutdown 未完全收敛：{cause}");
        }
    }
}

/// `/references` 命名空间下的真实挂载目标。
pub fn reference_targets_of(facts: &ContainerEnvironmentFacts) -> Vec<String> {
    facts
        .mounts
        .iter()
        .filter(|mount| mount.target == "/references" || mount.target.starts_with("/references/"))
        .map(|mount| mount.target.clone())
        .collect()
}

/// `config` 命令：只做 provider/角色模型配置与核对。
pub async fn configure_only(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    let store = open_store(config).await?;
    let host = start(config, store).await?;
    let fact = host.configure_models().await?;
    evidence.fact("provider", fact);
    evidence.pass(
        "provider.configured",
        format!(
            "provider `{PROVIDER_ID}`（{}）与四个角色路由已写入并核对，模型 `{MODEL_SLUG}` 使用 Responses HTTP",
            config.provider_base_url
        ),
    );
    host.shutdown().await;
    Ok(())
}

fn acceptance_provider(config: &DriverConfig) -> Result<ProviderConfig> {
    let endpoint = ProviderEndpoint::compatible(PROVIDER_NAME, config.provider_base_url.clone());
    let mut model = ModelInfo::compatible(MODEL_SLUG);
    // 多轮验收共用历史；本地确定性 provider 不使用真实供应商的上下文配额。
    model.context_window = Some(1_000_000);
    model.max_context_window = Some(1_000_000);
    // 默认连接方式是 WebSocket；HTTP SSE fixture 必须显式声明 responses_http。
    model
        .binding
        .set_transport(ModelTransportProfile::responses_http());
    let mut provider = PlProviderConfig::from_explicit_models(endpoint, vec![model]);
    provider.bearer_token = Some(PROVIDER_TOKEN.to_string());
    Ok(ProviderConfig {
        id: PROVIDER_ID.to_string(),
        source: ProviderConfigSource::Custom { config: provider },
    })
}

fn acceptance_routes() -> Result<AgentConfigRequest> {
    let provider = ProviderId::new(PROVIDER_ID)
        .map_err(|cause| error("provider_id_invalid", format!("provider id 非法：{cause}")))?;
    let route = ModelRouteConfig {
        provider,
        model: MODEL_SLUG.to_string(),
        effort: None,
    };
    Ok(AgentConfigRequest {
        planner: Some(route.clone()),
        explorer: Some(route.clone()),
        executor: Some(route.clone()),
        reviewer: Some(route),
    })
}

fn ensure_responses_http(config: &AgentConfigResponse) -> Result<()> {
    for (role, effective) in [
        ("planner", config.effective_planner.as_ref()),
        ("explorer", config.effective_explorer.as_ref()),
        ("executor", config.effective_executor.as_ref()),
        ("reviewer", config.effective_reviewer.as_ref()),
    ] {
        let Some(effective) = effective else {
            return Err(error(
                "agent_route_missing",
                format!("角色 `{role}` 没有生效模型路由"),
            ));
        };
        if effective.provider_id != PROVIDER_ID {
            return Err(error(
                "agent_route_provider_mismatch",
                format!(
                    "角色 `{role}` 生效 provider 是 {}，期望 {PROVIDER_ID}",
                    effective.provider_id
                ),
            ));
        }
        if effective.model != MODEL_SLUG {
            return Err(error(
                "agent_route_model_mismatch",
                format!(
                    "角色 `{role}` 生效模型是 {}，期望 {MODEL_SLUG}",
                    effective.model
                ),
            ));
        }
        if effective.transport.protocol != ProviderWireProtocol::Responses
            || effective.transport.default_connection_mode != ProviderConnectionMode::Http
        {
            return Err(error(
                "agent_route_transport_not_http",
                format!(
                    "角色 `{role}` 的连接方式不是 Responses HTTP（{:?}/{:?}）；WebSocket 会让本地 fixture 失败",
                    effective.transport.protocol, effective.transport.default_connection_mode
                ),
            ));
        }
    }
    Ok(())
}

fn transport_label(transport: ModelTransportProfile) -> String {
    format!(
        "{:?}/{:?}",
        transport.protocol, transport.default_connection_mode
    )
}

/// 两类参考源的宿主内容摘要（与真实验证器同一套扫描与摘要域）。
pub fn host_source_digests(
    config: &DriverConfig,
    evidence: &mut Evidence,
) -> BTreeMap<String, Option<String>> {
    let mut digests = BTreeMap::new();
    digests.insert(
        fixtures::ALIAS_PRIVATE.to_string(),
        host_source_digest(config, &config.fixture_repo),
    );
    if config.digest_scope.includes_linux() {
        digests.insert(
            fixtures::ALIAS_LINUX.to_string(),
            host_source_digest(config, &config.linux_source),
        );
    } else {
        evidence.limitation(
            "Linux 快照的宿主摘要未在本轮采集（--digest fixture）；夹具源摘要已覆盖写入尝试"
                .to_string(),
        );
    }
    evidence.fact("host_digests", json!(&digests));
    digests
}

/// 单个参考源的宿主内容摘要；部署策略或信任模式不允许扫描时返回 `None`。
pub fn host_source_digest(config: &DriverConfig, path: &Path) -> Option<String> {
    let policy = mai_references::load_reference_deployment_policy(&config.policy_path).ok()?;
    let root = mai_references::validate_source_root(&policy, &path.to_string_lossy()).ok()?;
    let scan = mai_references::scan_source_tree(&root, &policy).ok()?;
    Some(scan.digest().to_string())
}
