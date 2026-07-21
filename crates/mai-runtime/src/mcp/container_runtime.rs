use std::collections::{BTreeMap, BTreeSet};

use mai_docker::DockerClient;
use mai_protocol::{McpServerConfig, McpStartupStatus};
use pl_core::{
    AgentModelConfig, BuiltinMcpServerState, EffectiveMcpServerConfig, McpAvailabilityKind,
    McpRuntime, McpRuntimeHandle, McpServerTransport,
};
use tokio::sync::RwLock;

use super::{McpServerStatus, container_host::ContainerMcpRuntimeHost};
use crate::{Result, RuntimeError};

/// 单个 agent 容器对应的 PL MCP runtime 产品包装。
///
/// 包装只保留 Mai 的 required provisioning 语义；所有执行状态均由 PL handle 持有。
pub(crate) struct ContainerMcpRuntime {
    handle: McpRuntimeHandle,
    required_servers: RwLock<BTreeSet<String>>,
}

impl ContainerMcpRuntime {
    pub(crate) async fn start(
        docker: DockerClient,
        container_id: String,
        enabled: bool,
        user_servers: &BTreeMap<String, McpServerConfig>,
        builtin_states: &BTreeMap<String, BuiltinMcpServerState>,
        models: &AgentModelConfig,
    ) -> Result<Self> {
        let host = ContainerMcpRuntimeHost::new(docker, container_id);
        let handle = McpRuntime::new(host).handle();
        let runtime = Self {
            handle,
            required_servers: RwLock::new(BTreeSet::new()),
        };
        runtime
            .reconcile(enabled, user_servers, builtin_states, models)
            .await?;
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
        self.update_required_servers(enabled, user_servers).await;
        let servers = if enabled {
            effective_servers(user_servers, builtin_states, models)?
        } else {
            BTreeMap::new()
        };
        self.handle
            .reconcile(servers)
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
        self.update_required_servers(enabled, user_servers).await;
        let servers = if enabled {
            effective_servers(user_servers, builtin_states, models)?
        } else {
            BTreeMap::new()
        };
        self.handle
            .recheck(servers)
            .await
            .map_err(RuntimeError::Model)
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
