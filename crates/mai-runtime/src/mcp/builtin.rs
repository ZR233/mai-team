//! mai 产品内置 MCP server 目录与可用性计算。
//!
//! PL 只提供连接配置值对象、校验与运行时；内置目录、凭据选择与产品开关属于 mai
//! 策略，因此在此维护固定的 Zhipu Coding Plan 四个 server，并复用
//! `pl_tool::mcp::config` 的数据类型与校验，不引用产品外部模块。

use std::collections::BTreeMap;

use pl_model::config::{AgentModelConfig, ProviderConfig};
use pl_protocol::{PureError, Result};
use pl_tool::approval::ToolEffect;
use pl_tool::mcp::config::{
    EffectiveMcpServerConfig, McpServerConfig, McpServerMutationPolicy, McpServerSourceKind,
    McpServerStatusKind, McpServerTransport,
};

use crate::config::MaiBuiltinMcpServerState;

/// mai 固定的内置 MCP server 定义。
struct BuiltinMcpServerDefinition {
    id: &'static str,
    transport: McpServerTransport,
    url: Option<&'static str>,
    command: Option<&'static str>,
    args: &'static [&'static str],
    source_detail: &'static str,
    tool_effect: Option<ToolEffect>,
    env: &'static [(&'static str, &'static str)],
    credential_env_var: Option<&'static str>,
    startup_timeout_secs: Option<u64>,
    tool_timeout_secs: Option<u64>,
}

/// 优先按 preset id 识别 Zhipu Coding Plan 凭据。
const ZHIPU_PRESET_IDS: &[&str] = &["zhipu-coding-plan", "zhipu"];

/// 兜底按 base_url 端点主机识别 Zhipu 凭据。
const ZHIPU_ENDPOINT_HOSTS: &[&str] = &["open.bigmodel.cn"];

const BUILTIN_MCP_SERVERS: &[BuiltinMcpServerDefinition] = &[
    BuiltinMcpServerDefinition {
        id: "zhipu_search",
        transport: McpServerTransport::StreamableHttp,
        url: Some("https://open.bigmodel.cn/api/mcp/web_search_prime/mcp"),
        command: None,
        args: &[],
        source_detail: "Zhipu Coding Plan",
        tool_effect: Some(ToolEffect::Read),
        env: &[],
        credential_env_var: None,
        startup_timeout_secs: None,
        tool_timeout_secs: None,
    },
    BuiltinMcpServerDefinition {
        id: "zhipu_reader",
        transport: McpServerTransport::StreamableHttp,
        url: Some("https://open.bigmodel.cn/api/mcp/web_reader/mcp"),
        command: None,
        args: &[],
        source_detail: "Zhipu Coding Plan",
        tool_effect: Some(ToolEffect::Read),
        env: &[],
        credential_env_var: None,
        startup_timeout_secs: None,
        tool_timeout_secs: None,
    },
    BuiltinMcpServerDefinition {
        id: "zhipu_zread",
        transport: McpServerTransport::StreamableHttp,
        url: Some("https://open.bigmodel.cn/api/mcp/zread/mcp"),
        command: None,
        args: &[],
        source_detail: "Zhipu Coding Plan",
        tool_effect: Some(ToolEffect::Read),
        env: &[],
        credential_env_var: None,
        startup_timeout_secs: None,
        tool_timeout_secs: None,
    },
    BuiltinMcpServerDefinition {
        id: "zhipu_vision",
        transport: McpServerTransport::Stdio,
        url: None,
        command: Some("npx"),
        args: &["-y", "@z_ai/mcp-server"],
        source_detail: "Zhipu Coding Plan",
        tool_effect: Some(ToolEffect::Read),
        env: &[
            ("Z_AI_MODE", "ZHIPU"),
            // npm server 默认 32768，会显著放大简单图片冒烟的延迟和上下文。
            ("Z_AI_VISION_MODEL_MAX_TOKENS", "4096"),
        ],
        credential_env_var: Some("Z_AI_API_KEY"),
        // 首次运行可能需要由 npx 下载内置 server；后续 generation 会复用 npm cache。
        startup_timeout_secs: Some(60),
        // 上游 vision server 自身默认请求超时为 300 秒；仅对该 server 放宽，
        // 不改变其他内置 MCP 的快速失败语义。
        tool_timeout_secs: Some(360),
    },
];

/// 判断 id 是否属于 mai 固定的内置 MCP server。
pub(super) fn is_builtin_server_id(server_id: &str) -> bool {
    BUILTIN_MCP_SERVERS
        .iter()
        .any(|definition| definition.id == server_id)
}

/// 校验用户自建 MCP server：id 不得占用内置名，配置需通过 PL 校验。
pub(super) fn validate_user_servers(servers: &BTreeMap<String, McpServerConfig>) -> Result<()> {
    for (server_id, server) in servers {
        if is_builtin_server_id(server_id) {
            return Err(PureError::ConfigError(format!(
                "mcp server id '{server_id}' is reserved for a built-in server"
            )));
        }
        server.validate(server_id)?;
    }
    Ok(())
}

/// 合并用户 server 与内置 server，计算产品可见的有效配置与状态。
pub(super) fn effective_servers(
    user_servers: &BTreeMap<String, McpServerConfig>,
    builtin_states: &BTreeMap<String, MaiBuiltinMcpServerState>,
    models: &AgentModelConfig,
) -> BTreeMap<String, EffectiveMcpServerConfig> {
    let mut servers = BTreeMap::new();
    for (server_id, server) in user_servers {
        let status_kind = if server.enabled {
            McpServerStatusKind::Enabled
        } else {
            McpServerStatusKind::Disabled
        };
        servers.insert(
            server_id.clone(),
            EffectiveMcpServerConfig {
                id: server_id.clone(),
                config: server.clone(),
                source_kind: McpServerSourceKind::User,
                source_label: "User".to_string(),
                source_detail: None,
                status_kind,
                status_message: None,
                mutation_policy: McpServerMutationPolicy::UserEditable,
                bearer_token: None,
                tool_effect: None,
            },
        );
    }

    let token = resolve_zhipu_token(models);
    for definition in BUILTIN_MCP_SERVERS {
        let enabled = builtin_states
            .get(definition.id)
            .is_none_or(|state| state.enabled);
        let status_kind = if !enabled {
            McpServerStatusKind::Disabled
        } else if token.is_some() {
            McpServerStatusKind::Enabled
        } else {
            McpServerStatusKind::MissingCredential
        };
        servers.insert(
            definition.id.to_string(),
            EffectiveMcpServerConfig {
                id: definition.id.to_string(),
                config: definition.config(token.as_deref()),
                source_kind: McpServerSourceKind::BuiltIn,
                source_label: "Built-in".to_string(),
                source_detail: Some(definition.source_detail.to_string()),
                status_kind,
                status_message: status_message(status_kind),
                mutation_policy: McpServerMutationPolicy::LockedIdentity,
                bearer_token: token.clone(),
                tool_effect: definition.tool_effect,
            },
        );
    }
    servers
}

fn status_message(status_kind: McpServerStatusKind) -> Option<String> {
    match status_kind {
        McpServerStatusKind::Enabled => {
            Some("Using the configured Zhipu Coding Plan or Zhipu provider token".to_string())
        }
        McpServerStatusKind::MissingCredential => Some(
            "Configure a Zhipu Coding Plan or Zhipu provider token to enable this server"
                .to_string(),
        ),
        McpServerStatusKind::Disabled => None,
    }
}

/// 按 preset 优先、端点兜底的顺序选择唯一的 Zhipu 凭据。
fn resolve_zhipu_token(models: &AgentModelConfig) -> Option<String> {
    models
        .providers
        .values()
        .filter(|provider| {
            provider
                .preset_id()
                .is_some_and(|preset| ZHIPU_PRESET_IDS.contains(&preset.as_str()))
        })
        .find_map(ProviderConfig::resolved_bearer_token)
        .or_else(|| {
            models.providers.values().find_map(|provider| {
                let matches_endpoint = reqwest::Url::parse(&provider.base_url)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_string))
                    .is_some_and(|host| ZHIPU_ENDPOINT_HOSTS.contains(&host.as_str()));
                matches_endpoint
                    .then(|| provider.resolved_bearer_token())
                    .flatten()
            })
        })
}

impl BuiltinMcpServerDefinition {
    fn config(&self, token: Option<&str>) -> McpServerConfig {
        let mut env = self
            .env
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<BTreeMap<_, _>>();
        if let (Some(key), Some(token)) = (self.credential_env_var, token) {
            env.insert(key.to_string(), token.to_string());
        }
        McpServerConfig {
            enabled: token.is_some(),
            transport: self.transport,
            command: self.command.map(ToOwned::to_owned),
            args: self.args.iter().map(|arg| (*arg).to_string()).collect(),
            env,
            cwd: None,
            url: self.url.map(ToOwned::to_owned),
            bearer_token_env_var: None,
            headers: BTreeMap::new(),
            startup_timeout_secs: self.startup_timeout_secs,
            tool_timeout_secs: self.tool_timeout_secs,
            enabled_tools: None,
            disabled_tools: Vec::new(),
        }
    }
}
