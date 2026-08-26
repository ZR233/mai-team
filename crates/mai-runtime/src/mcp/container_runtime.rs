use std::collections::{BTreeMap, BTreeSet};

use mai_docker::DockerClient;
use mai_protocol::{McpServerConfig, McpStartupStatus};
use pl_core::{
    AgentModelConfig, BuiltinMcpServerState, EffectiveMcpServerConfig, McpAvailabilityKind,
    McpConnector, McpResetScope, McpRuntime, McpRuntimeHandle, McpServerTransport,
};
use tokio::sync::RwLock;

use super::McpServerStatus;
use crate::{Result, RuntimeError};

pub(crate) struct ContainerMcpSettings {
    pub(crate) enabled: bool,
    pub(crate) user_servers: BTreeMap<String, McpServerConfig>,
    pub(crate) builtin_servers: BTreeMap<String, BuiltinMcpServerState>,
    pub(crate) models: AgentModelConfig,
}

/// 单个 agent 容器对应的 PL MCP runtime 产品包装。
///
/// PL 的 `McpConnector` 负责在宿主侧 spawn 进程与 rmcp 握手；stdio server 通过
/// 改写为 `docker exec -i` 在 agent 专属 sidecar 内执行。每次模型调用前由
/// `McpTurnLease` 冻结本 generation 的工具集合。
pub(crate) struct ContainerMcpRuntime {
    docker: DockerClient,
    sidecar_container_id: String,
    handle: McpRuntimeHandle,
    required_servers: RwLock<BTreeSet<String>>,
}

impl ContainerMcpRuntime {
    pub(crate) async fn start(
        docker: DockerClient,
        agent_id: mai_protocol::AgentId,
        agent_container_id: String,
        sidecar_image: &str,
        settings: ContainerMcpSettings,
    ) -> Result<Self> {
        let sidecar = docker
            .create_agent_mcp_sidecar_container(
                &agent_id.to_string(),
                &agent_container_id,
                sidecar_image,
            )
            .await?;
        let handle = McpRuntime::new(McpConnector::default()).handle();
        let runtime = Self {
            docker,
            sidecar_container_id: sidecar.id,
            handle,
            required_servers: RwLock::new(BTreeSet::new()),
        };
        if let Err(error) = runtime
            .reconcile(
                settings.enabled,
                &settings.user_servers,
                &settings.builtin_servers,
                &settings.models,
            )
            .await
        {
            runtime.shutdown().await;
            return Err(error);
        }
        Ok(runtime)
    }

    pub(crate) fn handle(&self) -> &McpRuntimeHandle {
        &self.handle
    }

    pub(crate) async fn reconcile(
        &self,
        enabled: bool,
        user_servers: &BTreeMap<String, McpServerConfig>,
        builtin_states: &BTreeMap<String, BuiltinMcpServerState>,
        models: &AgentModelConfig,
    ) -> Result<()> {
        self.handle
            .reconcile(
                self.effective_servers(enabled, user_servers, builtin_states, models)
                    .await?,
            )
            .await
            .map_err(RuntimeError::Model)
    }

    pub(crate) async fn recheck(
        &self,
        enabled: bool,
        user_servers: &BTreeMap<String, McpServerConfig>,
        builtin_states: &BTreeMap<String, BuiltinMcpServerState>,
        models: &AgentModelConfig,
    ) -> Result<()> {
        self.handle
            .reset(
                McpResetScope::All,
                self.effective_servers(enabled, user_servers, builtin_states, models)
                    .await?,
            )
            .await
            .map_err(RuntimeError::Model)
    }

    async fn effective_servers(
        &self,
        enabled: bool,
        user_servers: &BTreeMap<String, McpServerConfig>,
        builtin_states: &BTreeMap<String, BuiltinMcpServerState>,
        models: &AgentModelConfig,
    ) -> Result<BTreeMap<String, EffectiveMcpServerConfig>> {
        self.update_required_servers(enabled, user_servers).await;
        let mut servers = if enabled {
            effective_servers(user_servers, builtin_states, models)?
        } else {
            BTreeMap::new()
        };
        rewrite_sidecar_stdio(&self.docker, &self.sidecar_container_id, &mut servers);
        Ok(servers)
    }

    async fn update_required_servers(
        &self,
        enabled: bool,
        user_servers: &BTreeMap<String, McpServerConfig>,
    ) {
        *self.required_servers.write().await = user_servers
            .iter()
            .filter_map(|(id, config)| {
                (enabled && config.enabled && config.required).then_some(id.clone())
            })
            .collect();
    }

    pub(crate) async fn statuses(&self) -> Vec<McpServerStatus> {
        let required_servers = self.required_servers.read().await;
        self.handle
            .snapshots()
            .await
            .into_iter()
            .map(|(server, snapshot)| McpServerStatus {
                required: required_servers.contains(&server),
                server,
                status: startup_status(snapshot.availability_kind),
                error: snapshot.availability_message,
            })
            .collect()
    }

    pub(crate) async fn required_failures(&self) -> Vec<McpServerStatus> {
        self.statuses()
            .await
            .into_iter()
            .filter(|status| status.required && status.status == McpStartupStatus::Failed)
            .collect()
    }

    pub(crate) async fn shutdown(&self) {
        self.handle.shutdown().await;
        if let Err(error) = self
            .docker
            .delete_container(&self.sidecar_container_id)
            .await
        {
            tracing::warn!(
                container_id = %self.sidecar_container_id,
                "failed to delete agent MCP sidecar: {error}"
            );
        }
    }
}

/// 把 sidecar 内执行的 stdio server 改写为宿主侧 `docker exec -i` 命令。
///
/// PL 在宿主 spawn 改写后的命令并注入 `config.env`；argv 里的 `-e KEY` 负责把
/// 宿主环境中的值透传进容器，`-w` 承载容器内工作目录，因此改写后 `cwd` 清空。
/// StreamableHttp server 保持宿主直连。
fn rewrite_sidecar_stdio(
    docker: &DockerClient,
    sidecar_container_id: &str,
    servers: &mut BTreeMap<String, EffectiveMcpServerConfig>,
) {
    for server in servers.values_mut() {
        if server.config.transport != McpServerTransport::Stdio {
            continue;
        }
        let Some(command) = server.config.command.clone() else {
            continue;
        };
        let env = server
            .config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        let argv = docker.exec_argv(
            sidecar_container_id,
            &command,
            &server.config.args,
            server.config.cwd.as_deref(),
            &env,
        );
        let (binary, args) = argv.split_first().expect("exec argv is non-empty");
        server.config.command = Some(binary.clone());
        server.config.args = args.to_vec();
        server.config.cwd = None;
    }
}

pub(crate) fn effective_servers(
    user_servers: &BTreeMap<String, McpServerConfig>,
    builtin_states: &BTreeMap<String, BuiltinMcpServerState>,
    models: &AgentModelConfig,
) -> Result<BTreeMap<String, EffectiveMcpServerConfig>> {
    let user = user_servers
        .iter()
        .map(|(id, config)| (id.clone(), core_config(config)))
        .collect();
    pl_core::validate_mcp_servers(&user).map_err(RuntimeError::Model)?;
    let mut effective = pl_core::effective_mcp_servers(&user, builtin_states, models);
    for (id, config) in user_servers {
        if let Some(server) = effective.get_mut(id) {
            server.bearer_token = config
                .bearer_token
                .clone()
                .filter(|token| !token.trim().is_empty());
        }
    }
    for server in effective.values() {
        server
            .config
            .validate(&server.id)
            .map_err(RuntimeError::Model)?;
    }
    Ok(effective)
}

fn core_config(config: &McpServerConfig) -> pl_core::McpServerConfig {
    pl_core::McpServerConfig {
        enabled: config.enabled,
        transport: match config.transport {
            mai_protocol::McpServerTransport::Stdio => McpServerTransport::Stdio,
            mai_protocol::McpServerTransport::StreamableHttp => McpServerTransport::StreamableHttp,
        },
        command: config.command.clone(),
        args: config.args.clone(),
        env: config.env.clone(),
        cwd: config.cwd.clone(),
        url: config.url.clone(),
        bearer_token_env_var: config.bearer_token_env.clone(),
        headers: config.headers.clone(),
        startup_timeout_secs: config.startup_timeout_secs,
        tool_timeout_secs: config.tool_timeout_secs,
        enabled_tools: config.enabled_tools.clone(),
        disabled_tools: config.disabled_tools.clone(),
    }
}

fn startup_status(kind: McpAvailabilityKind) -> McpStartupStatus {
    match kind {
        McpAvailabilityKind::Available => McpStartupStatus::Ready,
        McpAvailabilityKind::Checking => McpStartupStatus::Starting,
        McpAvailabilityKind::Disabled
        | McpAvailabilityKind::MissingCredential
        | McpAvailabilityKind::Unavailable => McpStartupStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn docker() -> DockerClient {
        DockerClient::new("mai-team/test:latest")
    }

    fn effective(id: &str, config: pl_core::McpServerConfig) -> EffectiveMcpServerConfig {
        EffectiveMcpServerConfig {
            id: id.to_string(),
            config,
            source_kind: pl_core::McpServerSourceKind::User,
            source_label: id.to_string(),
            source_detail: None,
            status_kind: pl_core::McpServerStatusKind::Enabled,
            status_message: None,
            mutation_policy: pl_core::McpServerMutationPolicy::UserEditable,
            bearer_token: None,
            tool_effect: None,
        }
    }

    #[test]
    fn stdio_servers_are_rewritten_to_sidecar_docker_exec() {
        let mut servers = BTreeMap::from([(
            "local".to_string(),
            effective(
                "local",
                pl_core::McpServerConfig {
                    transport: McpServerTransport::Stdio,
                    command: Some("npx".to_string()),
                    args: vec!["-y".to_string(), "server".to_string()],
                    env: BTreeMap::from([("TOKEN".to_string(), "secret".to_string())]),
                    cwd: Some("/work".to_string()),
                    ..Default::default()
                },
            ),
        )]);
        rewrite_sidecar_stdio(&docker(), "sidecar-1", &mut servers);

        let config = &servers["local"].config;
        assert_eq!(config.command.as_deref(), Some("docker"));
        assert_eq!(
            config.args,
            vec![
                "exec".to_string(),
                "-i".to_string(),
                "-w".to_string(),
                "/work".to_string(),
                "-e".to_string(),
                "TOKEN".to_string(),
                "sidecar-1".to_string(),
                "npx".to_string(),
                "-y".to_string(),
                "server".to_string(),
            ]
        );
        assert_eq!(config.cwd, None);
        // env 保留：PL 注入宿主进程环境后由 -e TOKEN 透传进容器。
        assert_eq!(config.env.get("TOKEN").map(String::as_str), Some("secret"));
    }

    #[test]
    fn http_servers_keep_host_direct_connection() {
        let mut servers = BTreeMap::from([(
            "remote".to_string(),
            effective(
                "remote",
                pl_core::McpServerConfig {
                    transport: McpServerTransport::StreamableHttp,
                    url: Some("https://mcp.example/stream".to_string()),
                    ..Default::default()
                },
            ),
        )]);
        rewrite_sidecar_stdio(&docker(), "sidecar-1", &mut servers);

        let config = &servers["remote"].config;
        assert_eq!(config.command, None);
        assert_eq!(config.url.as_deref(), Some("https://mcp.example/stream"));
    }

    #[test]
    fn product_config_maps_transport_timeouts_filters_and_explicit_token() {
        let source = McpServerConfig {
            transport: mai_protocol::McpServerTransport::StreamableHttp,
            url: Some("https://future.example/mcp".to_string()),
            bearer_token: Some("secret".to_string()),
            bearer_token_env: Some("FUTURE_TOKEN".to_string()),
            startup_timeout_secs: Some(17),
            tool_timeout_secs: Some(31),
            enabled_tools: Some(vec!["read".to_string()]),
            disabled_tools: vec!["write".to_string()],
            ..Default::default()
        };
        let models = AgentModelConfig {
            providers: BTreeMap::new(),
            routes: BTreeMap::new(),
        };

        let effective = effective_servers(
            &BTreeMap::from([("future".to_string(), source)]),
            &BTreeMap::new(),
            &models,
        )
        .unwrap();
        let server = &effective["future"];

        assert_eq!(server.bearer_token.as_deref(), Some("secret"));
        assert_eq!(server.config.startup_timeout_secs, Some(17));
        assert_eq!(server.config.tool_timeout_secs, Some(31));
        assert_eq!(
            server.config.enabled_tools.as_deref(),
            Some(&["read".to_string()][..])
        );
        assert_eq!(server.config.disabled_tools, vec!["write".to_string()]);
    }

    #[test]
    fn zhipu_provider_enables_all_builtin_servers_from_one_token() {
        let registry = pl_core::builtin_provider_catalog();
        let mut provider = registry
            .presets
            .into_iter()
            .find(|preset| preset.id.as_str() == "zhipu-coding-plan")
            .unwrap()
            .provider;
        provider.bearer_token = Some("coding-plan-token".to_string());
        let models = AgentModelConfig {
            providers: BTreeMap::from([(pl_core::ProviderId::new("zhipu").unwrap(), provider)]),
            routes: BTreeMap::new(),
        };

        let effective = effective_servers(&BTreeMap::new(), &BTreeMap::new(), &models).unwrap();

        assert_eq!(
            effective.keys().cloned().collect::<Vec<_>>(),
            vec![
                "zhipu_reader".to_string(),
                "zhipu_search".to_string(),
                "zhipu_vision".to_string(),
                "zhipu_zread".to_string(),
            ]
        );
        assert!(effective.values().all(|server| {
            server.status_kind == pl_core::McpServerStatusKind::Enabled
                && server.bearer_token.as_deref() == Some("coding-plan-token")
        }));
        assert_eq!(
            effective["zhipu_vision"]
                .config
                .env
                .get("Z_AI_API_KEY")
                .map(String::as_str),
            Some("coding-plan-token")
        );
    }
}
