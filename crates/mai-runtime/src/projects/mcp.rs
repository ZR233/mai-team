use std::collections::BTreeMap;

use mai_protocol::{McpServerConfig, McpServerScope, McpServerTransport};

pub(crate) const PROJECT_WORKSPACE_PATH: &str = "/workspace/repo";
pub(crate) const PROJECT_GITHUB_MCP_SERVER: &str = "github";
pub(crate) const PROJECT_GIT_MCP_SERVER: &str = "git";

pub(crate) fn project_mcp_configs(token: &str) -> BTreeMap<String, McpServerConfig> {
    let mut configs = BTreeMap::new();
    configs.insert(
        PROJECT_GITHUB_MCP_SERVER.to_string(),
        McpServerConfig {
            scope: McpServerScope::Project,
            transport: McpServerTransport::Stdio,
            command: Some("github-mcp-server".to_string()),
            args: vec!["stdio".to_string()],
            env: BTreeMap::from([
                (
                    "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    token.to_string(),
                ),
                (
                    "GITHUB_TOOLSETS".to_string(),
                    "context,repos,issues,pull_requests".to_string(),
                ),
            ]),
            enabled: true,
            startup_timeout_secs: Some(20),
            ..McpServerConfig::default()
        },
    );
    configs.insert(
        PROJECT_GIT_MCP_SERVER.to_string(),
        McpServerConfig {
            scope: McpServerScope::Project,
            transport: McpServerTransport::Stdio,
            command: Some("uvx".to_string()),
            args: vec![
                "mcp-server-git".to_string(),
                "--repository".to_string(),
                PROJECT_WORKSPACE_PATH.to_string(),
            ],
            env: BTreeMap::new(),
            enabled: true,
            startup_timeout_secs: Some(20),
            ..McpServerConfig::default()
        },
    );
    configs
}
