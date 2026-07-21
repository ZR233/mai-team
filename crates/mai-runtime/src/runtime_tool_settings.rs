use std::collections::BTreeMap;

use futures::stream::{FuturesUnordered, StreamExt};
use mai_protocol::{
    BuiltinMcpServersRequest, McpServerAggregate, McpServerConfig, McpServerDescriptor,
    McpServerPublicConfig, McpServerScope, McpServerSecretClearRequest, McpServersConfigRequest,
    McpServersResponse, WebSearchLocationSettings, WebSearchSettings, WebSearchSettingsResponse,
};
use pl_core::{AgentRoleId, McpServerSourceKind, McpServerStatusKind};
use pl_model::{WebSearchContextSize, WebSearchLocation, WebSearchMode};

use crate::{AgentRuntime, Result, RuntimeError, config, mcp};

#[derive(Clone, Copy)]
enum ActiveMcpReconcileMode {
    Changed,
    Force,
}

impl AgentRuntime {
    pub async fn web_search_settings(&self) -> Result<WebSearchSettingsResponse> {
        let config = self.mai_config.read().await.clone();
        let mut roles = BTreeMap::new();
        for role in ["planner", "explorer", "executor", "reviewer"] {
            let role_id = AgentRoleId::new(role)?;
            let route = config.models.resolve(&role_id)?;
            let plan = pl_core::plan_web_search(&config.models, &route, &config.web_search)?;
            roles.insert(role.to_string(), plan.resolution.descriptor());
        }
        Ok(WebSearchSettingsResponse {
            config: web_search_to_api(&config.web_search),
            roles,
        })
    }

    pub async fn update_web_search_settings(
        &self,
        request: WebSearchSettings,
    ) -> Result<WebSearchSettingsResponse> {
        let web_search = web_search_from_api(request)?;
        {
            let mut config = self.mai_config.write().await;
            let mut next = config.clone();
            next.web_search = web_search;
            config::save(&self.deps.store, &next).await?;
            *config = next;
        }
        self.web_search_settings().await
    }

    pub async fn mcp_servers_response(&self) -> Result<McpServersResponse> {
        let user_servers = self.deps.store.list_mcp_servers().await?;
        let config = self.mai_config.read().await.clone();
        let effective =
            mcp::effective_servers(&user_servers, &config.mcp.builtin_servers, &config.models)?;
        let mut aggregate = effective
            .into_iter()
            .map(|(id, server)| {
                let public = user_servers.get(&id).map(public_mcp_config);
                let availability = match server.status_kind {
                    McpServerStatusKind::Enabled => "configured",
                    McpServerStatusKind::Disabled => "disabled",
                    McpServerStatusKind::MissingCredential => "missingCredential",
                };
                (
                    id,
                    McpServerAggregate {
                        descriptor: McpServerDescriptor {
                            id: server.id,
                            source: server.source_kind.as_str().to_string(),
                            transport: server.config.transport.as_str().to_string(),
                            endpoint: server.config.endpoint_summary(),
                            built_in: server.source_kind == McpServerSourceKind::BuiltIn,
                        },
                        enabled: server.status_kind == McpServerStatusKind::Enabled,
                        availability: availability.to_string(),
                        ready_agents: 0,
                        failed_agents: 0,
                        checking_agents: 0,
                        total_agents: 0,
                        tool_count: 0,
                        config: public,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let agents = self
            .state
            .agents
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for agent in agents {
            let Some(runtime) = agent.mcp.read().await.clone() else {
                continue;
            };
            for health in runtime.handle().health_snapshot().await?.servers {
                let item = aggregate
                    .entry(health.server.id.clone())
                    .or_insert_with(|| empty_aggregate(health.server.clone()));
                item.total_agents += 1;
                item.tool_count += health.tool_count.unwrap_or_default();
                match health.availability.as_str() {
                    "available" => item.ready_agents += 1,
                    "checking" => item.checking_agents += 1,
                    "disabled" => {}
                    "missingCredential" | "unavailable" => {
                        item.failed_agents += 1;
                    }
                    _ => item.failed_agents += 1,
                }
            }
        }
        for item in aggregate.values_mut() {
            if item.failed_agents > 0 {
                item.availability = "unavailable".to_string();
            } else if item.checking_agents > 0 {
                item.availability = "checking".to_string();
            } else if item.ready_agents > 0 {
                item.availability = "available".to_string();
            }
        }
        Ok(McpServersResponse {
            servers: aggregate.into_values().collect(),
        })
    }

    pub async fn update_mcp_servers(
        &self,
        request: McpServersConfigRequest,
    ) -> Result<McpServersResponse> {
        let current = self.deps.store.list_mcp_servers().await?;
        let servers = preserve_mcp_secrets(&current, request.servers, &request.clear_secrets)?;
        let config = self.mai_config.read().await.clone();
        mcp::effective_servers(&servers, &config.mcp.builtin_servers, &config.models)?;
        self.deps.store.save_mcp_servers(&servers).await?;
        self.reconcile_active_mcp_runtimes().await?;
        self.mcp_servers_response().await
    }

    pub async fn update_builtin_mcp_servers(
        &self,
        request: BuiltinMcpServersRequest,
    ) -> Result<McpServersResponse> {
        let states = request
            .servers
            .into_iter()
            .map(|(id, enabled)| (id, pl_core::BuiltinMcpServerState { enabled }))
            .collect();
        pl_core::validate_builtin_mcp_server_states(&states)?;
        {
            let mut config = self.mai_config.write().await;
            let mut next = config.clone();
            next.mcp.builtin_servers = states;
            config::save(&self.deps.store, &next).await?;
            *config = next;
        }
        self.reconcile_active_mcp_runtimes().await?;
        self.mcp_servers_response().await
    }

    pub async fn recheck_mcp_servers(&self) -> Result<McpServersResponse> {
        self.recheck_active_mcp_runtimes().await?;
        self.mcp_servers_response().await
    }

    pub(crate) async fn reconcile_active_mcp_runtimes(&self) -> Result<()> {
        self.apply_active_mcp_configuration(ActiveMcpReconcileMode::Changed)
            .await
    }

    async fn recheck_active_mcp_runtimes(&self) -> Result<()> {
        self.apply_active_mcp_configuration(ActiveMcpReconcileMode::Force)
            .await
    }

    async fn apply_active_mcp_configuration(&self, mode: ActiveMcpReconcileMode) -> Result<()> {
        let user_servers = self.deps.store.list_mcp_servers().await?;
        let config = self.mai_config.read().await.clone();
        let agents = self
            .state
            .agents
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut reconciles = FuturesUnordered::new();
        for agent in agents {
            let user_servers = &user_servers;
            let config = &config;
            reconciles.push(async move {
                let Some(runtime) = agent.mcp.read().await.clone() else {
                    return Ok(());
                };
                let has_project = agent.summary.read().await.project_id.is_some();
                let visible = visible_user_servers(user_servers, has_project);
                match mode {
                    ActiveMcpReconcileMode::Changed => {
                        runtime
                            .reconcile(
                                config.mcp.enabled,
                                &visible,
                                &config.mcp.builtin_servers,
                                &config.models,
                            )
                            .await
                    }
                    ActiveMcpReconcileMode::Force => {
                        runtime
                            .recheck(
                                config.mcp.enabled,
                                &visible,
                                &config.mcp.builtin_servers,
                                &config.models,
                            )
                            .await
                    }
                }
            });
        }
        while let Some(result) = reconciles.next().await {
            result?;
        }
        Ok(())
    }
}

fn visible_user_servers(
    servers: &BTreeMap<String, McpServerConfig>,
    has_project: bool,
) -> BTreeMap<String, McpServerConfig> {
    servers
        .iter()
        .filter(|(_, server)| match server.scope {
            McpServerScope::Agent | McpServerScope::System => true,
            McpServerScope::Project => has_project,
        })
        .map(|(id, server)| (id.clone(), server.clone()))
        .collect()
}

fn preserve_mcp_secrets(
    current: &BTreeMap<String, McpServerConfig>,
    mut next: BTreeMap<String, McpServerConfig>,
    clear: &BTreeMap<String, McpServerSecretClearRequest>,
) -> Result<BTreeMap<String, McpServerConfig>> {
    for server_id in clear.keys() {
        if !next.contains_key(server_id) {
            return Err(RuntimeError::InvalidInput(format!(
                "secret clear references missing MCP server: {server_id}"
            )));
        }
    }
    for (id, server) in &mut next {
        let Some(existing) = current.get(id) else {
            continue;
        };
        let clear = clear.get(id).cloned().unwrap_or_default();
        if clear.bearer_token
            && server
                .bearer_token
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(RuntimeError::InvalidInput(format!(
                "MCP server '{id}' cannot replace and clear its bearer token in one request"
            )));
        }
        if clear.bearer_token {
            server.bearer_token = None;
        } else if server
            .bearer_token
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            server.bearer_token = existing.bearer_token.clone();
        }
        preserve_empty_values(&existing.headers, &mut server.headers, &clear.headers);
        preserve_empty_values(&existing.env, &mut server.env, &clear.env);
    }
    Ok(next)
}

fn preserve_empty_values(
    current: &BTreeMap<String, String>,
    next: &mut BTreeMap<String, String>,
    clear: &[String],
) {
    for (key, existing) in current {
        if clear.contains(key) {
            next.remove(key);
            continue;
        }
        match next.get_mut(key) {
            Some(value) if value.is_empty() => *value = existing.clone(),
            Some(_) => {}
            None => {
                next.insert(key.clone(), existing.clone());
            }
        }
    }
}

fn public_mcp_config(config: &McpServerConfig) -> McpServerPublicConfig {
    McpServerPublicConfig {
        scope: config.scope,
        enabled: config.enabled,
        required: config.required,
        command: config.command.clone(),
        args: config.args.clone(),
        env_keys: config.env.keys().cloned().collect(),
        cwd: config.cwd.clone(),
        url: config.url.clone(),
        header_names: config.headers.keys().cloned().collect(),
        bearer_token_env: config.bearer_token_env.clone(),
        has_bearer_token: config
            .bearer_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty()),
        startup_timeout_secs: config.startup_timeout_secs,
        tool_timeout_secs: config.tool_timeout_secs,
        enabled_tools: config.enabled_tools.clone(),
        disabled_tools: config.disabled_tools.clone(),
    }
}

fn empty_aggregate(descriptor: McpServerDescriptor) -> McpServerAggregate {
    McpServerAggregate {
        descriptor,
        enabled: true,
        availability: "checking".to_string(),
        ready_agents: 0,
        failed_agents: 0,
        checking_agents: 0,
        total_agents: 0,
        tool_count: 0,
        config: None,
    }
}

fn web_search_to_api(config: &pl_model::WebSearchConfig) -> WebSearchSettings {
    WebSearchSettings {
        mode: mode_name(config.mode).to_string(),
        context_size: config
            .context_size
            .map(context_size_name)
            .map(str::to_string),
        allowed_domains: config.allowed_domains.clone(),
        location: config
            .location
            .as_ref()
            .map(|location| WebSearchLocationSettings {
                country: location.country.clone(),
                region: location.region.clone(),
                city: location.city.clone(),
                timezone: location.timezone.clone(),
            }),
    }
}

fn web_search_from_api(config: WebSearchSettings) -> Result<pl_model::WebSearchConfig> {
    Ok(pl_model::WebSearchConfig {
        mode: match config.mode.as_str() {
            "disabled" => WebSearchMode::Disabled,
            "cached" => WebSearchMode::Cached,
            "indexed" => WebSearchMode::Indexed,
            "live" => WebSearchMode::Live,
            value => {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid web search mode: {value}"
                )));
            }
        },
        context_size: config
            .context_size
            .as_deref()
            .map(|value| match value {
                "low" => Ok(WebSearchContextSize::Low),
                "medium" => Ok(WebSearchContextSize::Medium),
                "high" => Ok(WebSearchContextSize::High),
                value => Err(RuntimeError::InvalidInput(format!(
                    "invalid web search context size: {value}"
                ))),
            })
            .transpose()?,
        allowed_domains: config.allowed_domains,
        location: config.location.map(|location| WebSearchLocation {
            country: location.country,
            region: location.region,
            city: location.city,
            timezone: location.timezone,
        }),
    })
}

fn mode_name(mode: WebSearchMode) -> &'static str {
    match mode {
        WebSearchMode::Disabled => "disabled",
        WebSearchMode::Cached => "cached",
        WebSearchMode::Indexed => "indexed",
        WebSearchMode::Live => "live",
    }
}

fn context_size_name(size: WebSearchContextSize) -> &'static str {
    match size {
        WebSearchContextSize::Low => "low",
        WebSearchContextSize::Medium => "medium",
        WebSearchContextSize::High => "high",
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn write_only_secrets_are_preserved_or_explicitly_cleared() {
        let current = BTreeMap::from([(
            "demo".to_string(),
            McpServerConfig {
                bearer_token: Some("token".to_string()),
                headers: BTreeMap::from([
                    ("Keep".to_string(), "header-secret".to_string()),
                    ("Remove".to_string(), "old".to_string()),
                ]),
                env: BTreeMap::from([
                    ("KEEP_ENV".to_string(), "env-secret".to_string()),
                    ("REMOVE_ENV".to_string(), "old".to_string()),
                ]),
                ..Default::default()
            },
        )]);
        let next = BTreeMap::from([(
            "demo".to_string(),
            McpServerConfig {
                headers: BTreeMap::from([("Keep".to_string(), String::new())]),
                env: BTreeMap::from([("KEEP_ENV".to_string(), String::new())]),
                ..Default::default()
            },
        )]);
        let clear = BTreeMap::from([(
            "demo".to_string(),
            McpServerSecretClearRequest {
                bearer_token: false,
                env: vec!["REMOVE_ENV".to_string()],
                headers: vec!["Remove".to_string()],
            },
        )]);

        let preserved = preserve_mcp_secrets(&current, next, &clear).unwrap();

        assert_eq!(preserved["demo"].bearer_token.as_deref(), Some("token"));
        assert_eq!(
            preserved["demo"].headers,
            BTreeMap::from([("Keep".to_string(), "header-secret".to_string())])
        );
        assert_eq!(
            preserved["demo"].env,
            BTreeMap::from([("KEEP_ENV".to_string(), "env-secret".to_string())])
        );
    }

    #[test]
    fn replacing_and_clearing_bearer_token_is_rejected() {
        let current = BTreeMap::from([(
            "demo".to_string(),
            McpServerConfig {
                bearer_token: Some("old".to_string()),
                ..Default::default()
            },
        )]);
        let next = BTreeMap::from([(
            "demo".to_string(),
            McpServerConfig {
                bearer_token: Some("new".to_string()),
                ..Default::default()
            },
        )]);
        let clear = BTreeMap::from([(
            "demo".to_string(),
            McpServerSecretClearRequest {
                bearer_token: true,
                ..Default::default()
            },
        )]);

        assert!(preserve_mcp_secrets(&current, next, &clear).is_err());
    }

    #[test]
    fn public_mcp_projection_never_contains_secret_values() {
        let config = McpServerConfig {
            bearer_token: Some("token-value".to_string()),
            headers: BTreeMap::from([("Authorization".to_string(), "header-value".to_string())]),
            env: BTreeMap::from([("SECRET_ENV".to_string(), "env-value".to_string())]),
            ..Default::default()
        };

        let public = public_mcp_config(&config);
        let serialized = serde_json::to_string(&public).unwrap();

        assert_eq!(public.header_names, vec!["Authorization".to_string()]);
        assert_eq!(public.env_keys, vec!["SECRET_ENV".to_string()]);
        assert!(public.has_bearer_token);
        assert!(!serialized.contains("token-value"));
        assert!(!serialized.contains("header-value"));
        assert!(!serialized.contains("env-value"));
    }

    #[test]
    fn project_agents_keep_agent_system_and_project_scopes() {
        let servers = BTreeMap::from([
            (
                "agent".to_string(),
                McpServerConfig {
                    scope: McpServerScope::Agent,
                    ..Default::default()
                },
            ),
            (
                "project".to_string(),
                McpServerConfig {
                    scope: McpServerScope::Project,
                    ..Default::default()
                },
            ),
            (
                "system".to_string(),
                McpServerConfig {
                    scope: McpServerScope::System,
                    ..Default::default()
                },
            ),
        ]);

        assert_eq!(
            visible_user_servers(&servers, true)
                .into_keys()
                .collect::<Vec<_>>(),
            vec![
                "agent".to_string(),
                "project".to_string(),
                "system".to_string(),
            ]
        );
        assert_eq!(
            visible_user_servers(&servers, false)
                .into_keys()
                .collect::<Vec<_>>(),
            vec!["agent".to_string(), "system".to_string()]
        );
    }

    #[test]
    fn web_search_api_conversion_is_strict_and_round_trips() {
        let input = WebSearchSettings {
            mode: "cached".to_string(),
            context_size: Some("high".to_string()),
            allowed_domains: vec!["example.com".to_string()],
            location: Some(WebSearchLocationSettings {
                country: Some("CN".to_string()),
                region: None,
                city: None,
                timezone: Some("Asia/Shanghai".to_string()),
            }),
        };

        let internal = web_search_from_api(input.clone()).unwrap();

        assert_eq!(web_search_to_api(&internal), input);
        assert!(
            web_search_from_api(WebSearchSettings {
                mode: "future-unknown".to_string(),
                context_size: None,
                allowed_domains: Vec::new(),
                location: None,
            })
            .is_err()
        );
    }
}
