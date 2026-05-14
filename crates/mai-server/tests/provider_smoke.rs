use std::fs;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(120);
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[tokio::test]
#[ignore = "requires Docker, network access, and real configured provider credentials"]
async fn saved_providers_complete_smoke_flow_with_default_models() -> Result<(), SmokeError> {
    let app = SmokeServer::start().await?;
    let providers = app.smoke_providers().await?;
    if providers.is_empty() {
        eprintln!(
            "skipping provider smoke test: no enabled providers with API keys and default models"
        );
        return Ok(());
    }

    for provider in providers {
        app.exercise_provider(&provider).await?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SmokeProvider {
    id: String,
    name: String,
    model: String,
}

struct SmokeServer {
    _data_dir: TempDir,
    child: Child,
    client: Client,
    base_url: String,
}

impl SmokeServer {
    async fn start() -> Result<Self, SmokeError> {
        let data_dir = TempDir::new()?;
        copy_provider_config(data_dir.path())?;
        let port = available_port()?;
        let bind_addr = format!("127.0.0.1:{port}");
        let base_url = format!("http://{bind_addr}");
        let mut child = Command::new(env!("CARGO_BIN_EXE_mai-server"))
            .arg("--data-path")
            .arg(data_dir.path())
            .env("MAI_BIND_ADDR", &bind_addr)
            .env("MAI_RELAY_ENABLED", "false")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
        wait_for_health(&client, &base_url, &mut child).await?;
        Ok(Self {
            _data_dir: data_dir,
            child,
            client,
            base_url,
        })
    }

    async fn smoke_providers(&self) -> Result<Vec<SmokeProvider>, SmokeError> {
        let response = self.get_json("/providers").await?;
        let providers = response
            .get("providers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|provider| {
                let enabled = provider
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let has_api_key = provider
                    .get("has_api_key")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let id = provider.get("id").and_then(Value::as_str)?;
                let model = provider.get("default_model").and_then(Value::as_str)?;
                if !enabled || !has_api_key || model.trim().is_empty() {
                    return None;
                }
                Some(SmokeProvider {
                    id: id.to_string(),
                    name: provider
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string(),
                    model: model.to_string(),
                })
            })
            .collect();
        Ok(providers)
    }

    async fn exercise_provider(&self, provider: &SmokeProvider) -> Result<(), SmokeError> {
        eprintln!(
            "running provider smoke test: {} ({}) / {}",
            provider.id, provider.name, provider.model
        );
        let parent = self
            .create_agent(provider, None, "mai-smoke-parent")
            .await?;
        let parent_id = required_string(&parent, "/agent/id", provider)?;

        let first_turn = self
            .send_message(
                parent_id,
                "Smoke turn 1. Reply with exactly: alpha-smoke-ok",
            )
            .await?;
        let first_turn_id = required_string(&first_turn, "/turn_id", provider)?;
        let first_detail = self
            .wait_for_agent_completed(provider, parent_id, first_turn_id)
            .await?;
        assert_assistant_messages(provider, &first_detail, 1)?;

        let second_turn = self
            .send_message(
                parent_id,
                "Smoke turn 2. Remember the previous marker and reply with exactly: beta-smoke-ok",
            )
            .await?;
        let second_turn_id = required_string(&second_turn, "/turn_id", provider)?;
        let second_detail = self
            .wait_for_agent_completed(provider, parent_id, second_turn_id)
            .await?;
        assert_assistant_messages(provider, &second_detail, 2)?;

        let tool_turn = self
            .send_message(
                parent_id,
                "Use exactly one harmless tool call to inspect the current workspace, preferably list_files with max_files 3. After the tool result, reply with exactly: tool-smoke-ok",
            )
            .await?;
        let tool_turn_id = required_string(&tool_turn, "/turn_id", provider)?;
        let _tool_detail = self
            .wait_for_agent_completed(provider, parent_id, tool_turn_id)
            .await?;
        self.assert_successful_tool_trace(provider, parent_id, tool_turn_id)
            .await?;

        let child = self
            .create_agent(provider, Some(parent_id), "mai-smoke-child")
            .await?;
        let child_agent = child.pointer("/agent").unwrap_or(&Value::Null);
        let child_id = required_string(&child, "/agent/id", provider)?;
        assert_json_string(provider, child_agent, "parent_id", parent_id)?;
        assert_json_string(provider, child_agent, "provider_id", &provider.id)?;
        assert_json_string(provider, child_agent, "model", &provider.model)?;

        let child_turn = self
            .send_message(
                child_id,
                "Child smoke turn. Reply with exactly: child-smoke-ok",
            )
            .await?;
        let child_turn_id = required_string(&child_turn, "/turn_id", provider)?;
        let child_detail = self
            .wait_for_agent_completed(provider, child_id, child_turn_id)
            .await?;
        assert_assistant_messages(provider, &child_detail, 1)?;
        Ok(())
    }

    async fn create_agent(
        &self,
        provider: &SmokeProvider,
        parent_id: Option<&str>,
        name: &str,
    ) -> Result<Value, SmokeError> {
        self.post_json(
            "/agents",
            json!({
                "name": format!("{name}-{}-{}", provider.id, provider.model),
                "provider_id": provider.id,
                "model": provider.model,
                "parent_id": parent_id,
                "system_prompt": "You are a deterministic mai-server smoke test agent. Be concise. When asked to use a tool, perform one harmless tool call before the final answer.",
            }),
        )
        .await
    }

    async fn send_message(&self, agent_id: &str, message: &str) -> Result<Value, SmokeError> {
        self.post_json(
            &format!("/agents/{agent_id}/messages"),
            json!({ "message": message }),
        )
        .await
    }

    async fn wait_for_agent_completed(
        &self,
        provider: &SmokeProvider,
        agent_id: &str,
        turn_id: &str,
    ) -> Result<Value, SmokeError> {
        let started = Instant::now();
        let mut last_detail = Value::Null;
        while started.elapsed() < TURN_TIMEOUT {
            last_detail = self.get_json(&format!("/agents/{agent_id}")).await?;
            let current_turn = last_detail.get("current_turn").and_then(Value::as_str);
            let status = last_detail.get("status").and_then(Value::as_str);
            match status {
                Some("completed") if current_turn.is_none() => return Ok(last_detail),
                Some("failed") | Some("cancelled") | Some("deleted") => {
                    return Err(self
                        .agent_failure(provider, agent_id, turn_id, last_detail)
                        .await);
                }
                Some(_) | None => sleep(POLL_INTERVAL).await,
            }
        }
        Err(self
            .agent_failure(provider, agent_id, turn_id, last_detail)
            .await)
    }

    async fn assert_successful_tool_trace(
        &self,
        provider: &SmokeProvider,
        agent_id: &str,
        turn_id: &str,
    ) -> Result<(), SmokeError> {
        let traces = self
            .get_json(&format!("/agents/{agent_id}/tool-calls?turn_id={turn_id}"))
            .await?;
        let calls = traces
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if calls.iter().any(|call| {
            call.get("success").and_then(Value::as_bool) == Some(true)
                && call
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| matches!(name, "list_files" | "read_file" | "search_files"))
        }) {
            return Ok(());
        }
        Err(SmokeError::Provider {
            provider_id: provider.id.clone(),
            model: provider.model.clone(),
            message: format!(
                "turn {turn_id} completed without a successful harmless tool trace: {calls:?}"
            ),
        })
    }

    async fn agent_failure(
        &self,
        provider: &SmokeProvider,
        agent_id: &str,
        turn_id: &str,
        detail: Value,
    ) -> SmokeError {
        let logs = self
            .get_json(&format!(
                "/agents/{agent_id}/logs?turn_id={turn_id}&limit=20"
            ))
            .await
            .unwrap_or_else(|err| json!({ "log_error": err.to_string() }));
        let traces = self
            .get_json(&format!("/agents/{agent_id}/tool-calls?turn_id={turn_id}"))
            .await
            .unwrap_or_else(|err| json!({ "trace_error": err.to_string() }));
        SmokeError::Provider {
            provider_id: provider.id.clone(),
            model: provider.model.clone(),
            message: format!(
                "agent {agent_id} turn {turn_id} did not complete; detail={detail}; logs={logs}; traces={traces}"
            ),
        }
    }

    async fn get_json(&self, path: &str) -> Result<Value, SmokeError> {
        let response = self.client.get(self.url(path)).send().await?;
        json_response(response).await
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, SmokeError> {
        let response = self.client.post(self.url(path)).json(&body).send().await?;
        json_response(response).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

impl Drop for SmokeServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn wait_for_health(
    client: &Client,
    base_url: &str,
    child: &mut Child,
) -> Result<(), SmokeError> {
    let started = Instant::now();
    while started.elapsed() < SERVER_READY_TIMEOUT {
        if let Some(status) = child.try_wait()? {
            return Err(SmokeError::ServerExited(status.to_string()));
        }
        match client.get(format!("{base_url}/health")).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(_) | Err(_) => sleep(Duration::from_millis(500)).await,
        }
    }
    Err(SmokeError::Timeout(format!(
        "server did not become healthy at {base_url}"
    )))
}

async fn json_response(response: reqwest::Response) -> Result<Value, SmokeError> {
    let status = response.status();
    let text = response.text().await?;
    if status != StatusCode::OK {
        return Err(SmokeError::Http { status, body: text });
    }
    Ok(serde_json::from_str(&text)?)
}

fn copy_provider_config(target_data_dir: &Path) -> Result<(), SmokeError> {
    let source_data_dir = source_data_dir()?;
    let source_config = source_data_dir.join("config.toml");
    if !source_config.exists() {
        return Err(SmokeError::MissingConfig(source_config));
    }
    fs::create_dir_all(target_data_dir)?;
    fs::copy(&source_config, target_data_dir.join("config.toml"))?;
    Ok(())
}

fn source_data_dir() -> Result<PathBuf, SmokeError> {
    match std::env::var_os("MAI_PROVIDER_SMOKE_SOURCE_DATA_PATH") {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("mai-server crate is under <repo>/crates/mai-server")
            .join(".mai-team")),
    }
}

fn available_port() -> Result<u16, SmokeError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn assert_assistant_messages(
    provider: &SmokeProvider,
    detail: &Value,
    minimum: usize,
) -> Result<(), SmokeError> {
    let count = detail
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .count();
    if count >= minimum {
        return Ok(());
    }
    Err(SmokeError::Provider {
        provider_id: provider.id.clone(),
        model: provider.model.clone(),
        message: format!("expected at least {minimum} assistant messages, found {count}: {detail}"),
    })
}

fn assert_json_string(
    provider: &SmokeProvider,
    value: &Value,
    key: &str,
    expected: &str,
) -> Result<(), SmokeError> {
    let actual = value.get(key).and_then(Value::as_str);
    if actual == Some(expected) {
        return Ok(());
    }
    Err(SmokeError::Provider {
        provider_id: provider.id.clone(),
        model: provider.model.clone(),
        message: format!("expected {key}={expected:?}, got {actual:?}: {value}"),
    })
}

fn required_string<'a>(
    value: &'a Value,
    pointer: &str,
    provider: &SmokeProvider,
) -> Result<&'a str, SmokeError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| SmokeError::Provider {
            provider_id: provider.id.clone(),
            model: provider.model.clone(),
            message: format!("missing string at {pointer}: {value}"),
        })
}

#[derive(Debug)]
enum SmokeError {
    Io(io::Error),
    Reqwest(reqwest::Error),
    Json(serde_json::Error),
    Http {
        status: StatusCode,
        body: String,
    },
    MissingConfig(PathBuf),
    ServerExited(String),
    Timeout(String),
    Provider {
        provider_id: String,
        model: String,
        message: String,
    },
}

impl std::fmt::Display for SmokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Reqwest(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::MissingConfig(path) => write!(
                f,
                "provider smoke source config does not exist: {}",
                path.display()
            ),
            Self::ServerExited(status) => write!(f, "mai-server exited before readiness: {status}"),
            Self::Timeout(message) => write!(f, "{message}"),
            Self::Provider {
                provider_id,
                model,
                message,
            } => write!(f, "provider {provider_id} model {model}: {message}"),
        }
    }
}

impl std::error::Error for SmokeError {}

impl From<io::Error> for SmokeError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<reqwest::Error> for SmokeError {
    fn from(value: reqwest::Error) -> Self {
        Self::Reqwest(value)
    }
}

impl From<serde_json::Error> for SmokeError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
