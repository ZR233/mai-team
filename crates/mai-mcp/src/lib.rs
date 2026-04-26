use mai_docker::{DockerClient, DockerError};
use mai_protocol::McpServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Error)]
pub enum McpError {
    #[error("docker error: {0}")]
    Docker(#[from] DockerError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mcp server `{0}` failed: {1}")]
    Server(String, String),
    #[error("mcp tool `{0}` not found")]
    ToolNotFound(String),
    #[error("mcp session missing stdio")]
    MissingStdio,
}

pub type Result<T> = std::result::Result<T, McpError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub model_name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct McpAgentManager {
    sessions: RwLock<BTreeMap<String, Arc<McpSession>>>,
    tools: RwLock<BTreeMap<String, McpTool>>,
}

impl McpAgentManager {
    pub async fn start(
        docker: DockerClient,
        container_id: String,
        configs: BTreeMap<String, McpServerConfig>,
    ) -> Self {
        let manager = Self {
            sessions: RwLock::new(BTreeMap::new()),
            tools: RwLock::new(BTreeMap::new()),
        };

        for (server_name, config) in configs.into_iter().filter(|(_, config)| config.enabled) {
            match McpSession::start(&docker, &container_id, server_name.clone(), config).await {
                Ok(session) => {
                    let session = Arc::new(session);
                    match session.initialize_and_list_tools().await {
                        Ok(tools) => {
                            manager.sessions.write().await.insert(server_name, session);
                            let mut tool_map = manager.tools.write().await;
                            for tool in tools {
                                tool_map.insert(tool.model_name.clone(), tool);
                            }
                        }
                        Err(err) => tracing::warn!("failed to initialize MCP server: {err}"),
                    }
                }
                Err(err) => tracing::warn!("failed to start MCP server: {err}"),
            }
        }

        manager
    }

    pub async fn tools(&self) -> Vec<McpTool> {
        self.tools.read().await.values().cloned().collect()
    }

    pub async fn call_model_tool(&self, model_name: &str, arguments: Value) -> Result<Value> {
        let tool = self
            .tools
            .read()
            .await
            .get(model_name)
            .cloned()
            .ok_or_else(|| McpError::ToolNotFound(model_name.to_string()))?;
        let session = self
            .sessions
            .read()
            .await
            .get(&tool.server)
            .cloned()
            .ok_or_else(|| {
                McpError::Server(tool.server.clone(), "session not found".to_string())
            })?;
        session.call_tool(&tool.name, arguments).await
    }
}

struct McpSession {
    server_name: String,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<Lines<BufReader<ChildStdout>>>,
    _child: Mutex<Child>,
    request_lock: Mutex<()>,
    next_id: AtomicU64,
}

impl McpSession {
    async fn start(
        docker: &DockerClient,
        container_id: &str,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<Self> {
        let env = config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        let mut child = docker.spawn_exec(
            container_id,
            &config.command,
            &config.args,
            config.cwd.as_deref(),
            &env,
        )?;
        let stdin = child.stdin.take().ok_or(McpError::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(McpError::MissingStdio)?;
        Ok(Self {
            server_name,
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout).lines()),
            _child: Mutex::new(child),
            request_lock: Mutex::new(()),
            next_id: AtomicU64::new(1),
        })
    }

    async fn initialize_and_list_tools(&self) -> Result<Vec<McpTool>> {
        let _ = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "mai-team", "version": "0.1.0" }
                }),
            )
            .await?;
        self.notify("notifications/initialized", json!({})).await?;

        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| parse_tool(&self.server_name, value))
            .collect();
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let _guard = self.request_lock.lock().await;
        let mut stdin = self.stdin.lock().await;
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        stdin
            .write_all(serde_json::to_string(&payload)?.as_bytes())
            .await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let _guard = self.request_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(serde_json::to_string(&payload)?.as_bytes())
                .await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }

        let mut stdout = self.stdout.lock().await;
        while let Some(line) = stdout.next_line().await? {
            let value: Value = serde_json::from_str(&line)?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(McpError::Server(
                    self.server_name.clone(),
                    error.to_string(),
                ));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }

        Err(McpError::Server(
            self.server_name.clone(),
            "stdout closed".to_string(),
        ))
    }
}

fn parse_tool(server: &str, value: Value) -> Option<McpTool> {
    let name = value.get("name")?.as_str()?.to_string();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let input_schema = value
        .get("inputSchema")
        .or_else(|| value.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    Some(McpTool {
        model_name: model_tool_name(server, &name),
        server: server.to_string(),
        name,
        description,
        input_schema,
    })
}

pub fn model_tool_name(server: &str, tool: &str) -> String {
    let base = format!("mcp__{}__{}", sanitize_name(server), sanitize_name(tool));
    if base.len() <= 64 {
        return base;
    }
    let hash = fnv1a_hex(&base);
    let keep = 64usize.saturating_sub(hash.len() + 2);
    format!("{}__{}", &base[..keep], hash)
}

fn sanitize_name(value: &str) -> String {
    let mut out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if out.is_empty() {
        out = "tool".to_string();
    }
    if out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    out
}

fn fnv1a_hex(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_tool_names_are_sanitized() {
        assert_eq!(
            model_tool_name("fs.server", "read file"),
            "mcp__fs_server__read_file"
        );
        assert!(model_tool_name("1", "2").starts_with("mcp___1___2"));
    }

    #[test]
    fn long_model_tool_names_are_limited() {
        let name = model_tool_name(&"a".repeat(80), &"b".repeat(80));
        assert!(name.len() <= 64);
    }
}
