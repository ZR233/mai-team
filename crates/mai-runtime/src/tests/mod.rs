use super::*;
use chrono::TimeDelta;
use mai_protocol::{
    GitProvider, ModelConfig, ModelReasoningConfig, ModelReasoningVariant, ProviderConfig,
    ProviderKind, ProvidersConfigRequest,
};
use pretty_assertions::assert_eq;
use state::TurnControl;
use std::fs;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turn::completion::TurnResult;

fn test_model(id: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(id.to_string()),
        context_tokens: 400_000,
        output_tokens: 128_000,
        supports_tools: true,
        reasoning: Some(openai_reasoning_config(&[
            "minimal", "low", "medium", "high",
        ])),
        options: serde_json::Value::Null,
        headers: Default::default(),
        wire_api: Default::default(),
        capabilities: Default::default(),
        request_policy: Default::default(),
    }
}

fn non_reasoning_model(id: &str) -> ModelConfig {
    ModelConfig {
        reasoning: None,
        ..test_model(id)
    }
}

fn test_provider() -> ProviderConfig {
    ProviderConfig {
        id: "openai".to_string(),
        kind: ProviderKind::Openai,
        name: "OpenAI".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        api_key: Some("secret".to_string()),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        models: vec![
            test_model("gpt-5.5"),
            test_model("gpt-5.4"),
            non_reasoning_model("gpt-4.1"),
        ],
        default_model: "gpt-5.5".to_string(),
        enabled: true,
    }
}

fn deepseek_test_provider() -> ProviderConfig {
    ProviderConfig {
        id: "deepseek".to_string(),
        kind: ProviderKind::Deepseek,
        name: "DeepSeek".to_string(),
        base_url: "https://api.deepseek.com".to_string(),
        api_key: Some("secret".to_string()),
        api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
        models: vec![ModelConfig {
            id: "deepseek-v4-pro".to_string(),
            name: Some("deepseek-v4-pro".to_string()),
            context_tokens: 1_000_000,
            output_tokens: 384_000,
            supports_tools: true,
            reasoning: Some(deepseek_reasoning_config()),
            options: serde_json::Value::Null,
            headers: Default::default(),
            wire_api: mai_protocol::ModelWireApi::ChatCompletions,
            capabilities: Default::default(),
            request_policy: Default::default(),
        }],
        default_model: "deepseek-v4-pro".to_string(),
        enabled: true,
    }
}

fn openai_reasoning_config(variants: &[&str]) -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some("medium".to_string()),
        variants: variants
            .iter()
            .map(|id| ModelReasoningVariant {
                id: (*id).to_string(),
                label: None,
                request: json!({
                    "reasoning": {
                        "effort": id,
                    },
                }),
            })
            .collect(),
    }
}

fn deepseek_reasoning_config() -> ModelReasoningConfig {
    ModelReasoningConfig {
        default_variant: Some("high".to_string()),
        variants: ["high", "max"]
            .into_iter()
            .map(|id| ModelReasoningVariant {
                id: id.to_string(),
                label: None,
                request: json!({
                    "thinking": {
                        "type": "enabled",
                    },
                    "reasoning_effort": id,
                }),
            })
            .collect(),
    }
}

fn alt_test_provider() -> ProviderConfig {
    ProviderConfig {
        id: "alt".to_string(),
        kind: ProviderKind::Openai,
        name: "Alt".to_string(),
        base_url: "https://alt.example/v1".to_string(),
        api_key: Some("secret".to_string()),
        api_key_env: None,
        models: vec![test_model("alt-default"), test_model("alt-research")],
        default_model: "alt-default".to_string(),
        enabled: true,
    }
}

async fn start_mock_responses(responses: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server_responses = Arc::clone(&responses);
    let server_requests = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let responses = Arc::clone(&server_responses);
            let requests = Arc::clone(&server_requests);
            tokio::spawn(async move {
                let request = read_mock_request(&mut stream).await;
                let is_model_request = request
                    .get("request_line")
                    .and_then(Value::as_str)
                    .is_some_and(|line| line.contains(" /responses "))
                    || request.get("model").is_some();
                requests.lock().await.push(request);
                let response = responses.lock().await.pop_front().unwrap_or_else(|| {
                    json!({
                        "id": "resp_empty",
                        "output": [],
                        "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                    })
                });
                if response
                    .get("__close_without_response")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return;
                }
                if let Some(delay_ms) = response.get("__delay_ms").and_then(Value::as_u64) {
                    sleep(Duration::from_millis(delay_ms)).await;
                }
                write_mock_response(&mut stream, response, is_model_request).await;
            });
        }
    });
    (format!("http://{addr}"), requests)
}

async fn wait_until<F, Fut>(mut condition: F, timeout: Duration)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if condition().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_agent_idle(runtime: Arc<AgentRuntime>, agent_id: AgentId, timeout: Duration) {
    wait_until(
        || {
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .get_agent(agent_id, None)
                    .await
                    .map(|agent| {
                        agent.summary.current_turn.is_none()
                            && agent.summary.status.can_start_turn()
                    })
                    .unwrap_or(false)
            }
        },
        timeout,
    )
    .await;
}

async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> Value {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "mock request closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_header_end(&buffer) {
            break header_end;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = content_length(&headers);
    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "mock request closed before body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    if content_length == 0 {
        let request_line = headers.lines().next().unwrap_or_default();
        return json!({ "request_line": request_line });
    }
    let request_line = headers.lines().next().unwrap_or_default();
    let mut value: Value = serde_json::from_slice(&buffer[header_end..header_end + content_length])
        .expect("request json");
    if let Some(object) = value.as_object_mut() {
        object.insert("request_line".to_string(), json!(request_line));
    }
    value
}

async fn write_mock_response(
    stream: &mut tokio::net::TcpStream,
    response: Value,
    is_model_request: bool,
) {
    let status = response
        .get("__status")
        .and_then(Value::as_u64)
        .unwrap_or(200);
    let headers = response
        .get("__headers")
        .and_then(Value::as_object)
        .cloned();
    let mut body_value = response;
    if let Some(object) = body_value.as_object_mut() {
        object.remove("__status");
        object.remove("__headers");
    }
    let body = if status == 200 && is_model_request {
        mock_sse_body(&body_value)
    } else {
        serde_json::to_string(&body_value).expect("response json")
    };
    let reason = if status == 200 { "OK" } else { "ERROR" };
    let extra_headers = headers
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, value)| value.as_str().map(|value| format!("{name}: {value}\r\n")))
        .collect::<String>();
    let content_type = if status == 200 && is_model_request {
        "text/event-stream"
    } else {
        "application/json"
    };
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(reply.as_bytes())
        .await
        .expect("write response");
}

fn mock_sse_body(response: &Value) -> String {
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_mock");
    let mut events = vec![json!({
        "type": "response.created",
        "response": { "id": response_id }
    })];
    for (index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": index,
            "item": item,
        }));
    }
    events.push(json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "usage": response.get("usage").cloned().unwrap_or(Value::Null),
        }
    }));
    events
        .into_iter()
        .map(|event| {
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message");
            format!("event: {kind}\ndata: {event}\n\n")
        })
        .collect()
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or_default()
}

fn compact_test_provider(base_url: String) -> ProviderConfig {
    let mut model = test_model("mock-model");
    model.context_tokens = 100;
    model.output_tokens = 32;
    ProviderConfig {
        id: "mock".to_string(),
        kind: ProviderKind::Openai,
        name: "Mock".to_string(),
        base_url,
        api_key: Some("secret".to_string()),
        api_key_env: None,
        models: vec![model],
        default_model: "mock-model".to_string(),
        enabled: true,
    }
}

fn compact_no_continuation_test_provider(base_url: String) -> ProviderConfig {
    let mut provider = compact_test_provider(base_url);
    provider.models[0].capabilities.continuation = false;
    provider
}

fn test_agent_summary(agent_id: AgentId, container_id: Option<&str>) -> AgentSummary {
    test_agent_summary_with_parent(agent_id, None, container_id)
}

fn test_agent_summary_with_parent(
    agent_id: AgentId,
    parent_id: Option<AgentId>,
    container_id: Option<&str>,
) -> AgentSummary {
    let timestamp = now();
    AgentSummary {
        id: agent_id,
        parent_id,
        task_id: None,
        project_id: None,
        role: None,
        name: "compact-agent".to_string(),
        status: AgentStatus::Idle,
        container_id: container_id.map(ToOwned::to_owned),
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "mock".to_string(),
        provider_name: "Mock".to_string(),
        model: "mock-model".to_string(),
        reasoning_effort: Some("medium".to_string()),
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    }
}

fn test_agent_summary_at(
    agent_id: AgentId,
    parent_id: Option<AgentId>,
    created_at: chrono::DateTime<Utc>,
) -> AgentSummary {
    AgentSummary {
        created_at,
        updated_at: created_at,
        ..test_agent_summary_with_parent(agent_id, parent_id, None)
    }
}

async fn test_store(dir: &tempfile::TempDir) -> Arc<ConfigStore> {
    Arc::new(
        ConfigStore::open_with_config_and_artifact_index_path(
            &dir.path().join("runtime.sqlite3"),
            &dir.path().join("config.toml"),
            &dir.path().join("data/artifacts/index"),
        )
        .await
        .expect("open store"),
    )
}

async fn save_agent_with_session(store: &ConfigStore, summary: &AgentSummary) {
    store.save_agent(summary, None).await.expect("save agent");
    save_test_session(store, summary.id, Uuid::new_v4()).await;
}

fn write_skill_at(base: PathBuf, name: &str, description: &str, body: &str) -> PathBuf {
    let skill_dir = base.join(name);
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    let path = skill_dir.join("SKILL.md");
    fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .expect("write skill");
    path
}

fn write_project_skill(
    runtime: &AgentRuntime,
    project_id: ProjectId,
    name: &str,
    description: &str,
    body: &str,
) -> PathBuf {
    write_skill_at(
        runtime.project_skill_cache_dir(project_id).join("claude"),
        name,
        description,
        body,
    )
}

fn write_workspace_project_skill(
    dir: &tempfile::TempDir,
    project_id: ProjectId,
    agent_id: AgentId,
    root: &str,
    name: &str,
    description: &str,
    body: &str,
) -> PathBuf {
    let repo_path = ensure_project_clone(dir, project_id, agent_id);
    write_skill_at(repo_path.join(root), name, description, body)
}

fn ensure_project_repo(dir: &tempfile::TempDir, project_id: ProjectId) -> PathBuf {
    let repo_path = dir
        .path()
        .join("data/projects")
        .join(project_id.to_string())
        .join("repo");
    fs::create_dir_all(&repo_path).expect("mkdir project repo");
    let git_dir = repo_path.join(".git");
    if !git_dir.exists() {
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(&repo_path)
            .output()
            .expect("git init");
        fs::write(repo_path.join("README.md"), "test\n").expect("write readme");
        std::process::Command::new("git")
            .args(["-C", &repo_path.to_string_lossy(), "add", "README.md"])
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args([
                "-C",
                &repo_path.to_string_lossy(),
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "init",
            ])
            .output()
            .expect("git commit");
    }
    let repo_cache_path = dir
        .path()
        .join("data/projects")
        .join(project_id.to_string())
        .join("repo.git");
    if !repo_cache_path.exists() {
        std::process::Command::new("git")
            .args(["clone", "--mirror"])
            .arg(&repo_path)
            .arg(&repo_cache_path)
            .output()
            .expect("git clone mirror");
    }
    repo_path
}

fn ensure_project_clone(
    dir: &tempfile::TempDir,
    project_id: ProjectId,
    agent_id: AgentId,
) -> PathBuf {
    ensure_project_repo(dir, project_id);
    let projects_root = dir.path().join("data/projects");
    let repo_cache_path = projects_root.join(project_id.to_string()).join("repo.git");
    let clone_path =
        projects::workspace::paths::agent_clone_path(&projects_root, project_id, agent_id);
    if !clone_path.exists() {
        fs::create_dir_all(clone_path.parent().expect("clone parent")).expect("mkdir clone parent");
        std::process::Command::new("git")
            .args(["clone", "--local"])
            .arg(&repo_cache_path)
            .arg(&clone_path)
            .output()
            .expect("git clone local");
        if !clone_path.join(".git").exists() {
            fs::create_dir_all(clone_path.join(".git")).expect("mkdir clone git");
        }
        fs::write(clone_path.join("README.md"), "test\n").expect("write clone readme");
    }
    clone_path
}

async fn test_runtime(dir: &tempfile::TempDir, store: Arc<ConfigStore>) -> Arc<AgentRuntime> {
    AgentRuntime::new(
        DockerClient::new_with_binary("unused", fake_docker_path(dir)),
        ModelClient::new(),
        store,
        test_runtime_config(dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime")
}

async fn test_runtime_with_sidecar_image_and_git(
    dir: &tempfile::TempDir,
    store: Arc<ConfigStore>,
    sidecar_image: &str,
) -> Arc<AgentRuntime> {
    AgentRuntime::new(
        DockerClient::new_with_binary("unused-agent", fake_docker_path(dir)),
        ModelClient::new(),
        store,
        RuntimeConfig {
            git_binary: Some(fake_git_path(dir)),
            ..test_runtime_config(dir, sidecar_image)
        },
    )
    .await
    .expect("runtime")
}

async fn test_runtime_with_github_api(
    dir: &tempfile::TempDir,
    store: Arc<ConfigStore>,
    github_api_base_url: String,
) -> Arc<AgentRuntime> {
    AgentRuntime::new(
        DockerClient::new_with_binary("unused", fake_docker_path(dir)),
        ModelClient::new(),
        store,
        RuntimeConfig {
            github_api_base_url: Some(github_api_base_url),
            ..test_runtime_config(dir, DEFAULT_SIDECAR_IMAGE)
        },
    )
    .await
    .expect("runtime")
}

fn test_runtime_config(dir: &tempfile::TempDir, sidecar_image: &str) -> RuntimeConfig {
    RuntimeConfig {
        repo_root: dir.path().to_path_buf(),
        projects_root: dir.path().join("data/projects"),
        cache_root: dir.path().join("cache"),
        artifact_files_root: dir.path().join("data/artifacts/files"),
        sidecar_image: sidecar_image.to_string(),
        github_api_base_url: None,
        git_binary: None,
        system_skills_root: None,
        system_agents_root: None,
    }
}

fn test_project_summary(
    project_id: ProjectId,
    maintainer_agent_id: AgentId,
    git_account_id: &str,
) -> ProjectSummary {
    let timestamp = now();
    ProjectSummary {
        id: project_id,
        name: "owner/repo".to_string(),
        status: ProjectStatus::Creating,
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        repository_full_name: "owner/repo".to_string(),
        git_account_id: Some(git_account_id.to_string()),
        repository_id: 42,
        installation_id: 0,
        installation_account: "owner".to_string(),
        branch: "main".to_string(),
        docker_image: "unused-agent".to_string(),
        clone_status: ProjectCloneStatus::Pending,
        maintainer_agent_id,
        created_at: timestamp,
        updated_at: timestamp,
        last_error: None,
        auto_review_enabled: false,
        reviewer_extra_prompt: None,
        review_status: ProjectReviewStatus::Disabled,
        current_reviewer_agent_id: None,
        last_review_started_at: None,
        last_review_finished_at: None,
        next_review_at: None,
        last_review_outcome: None,
        review_last_error: None,
    }
}

fn ready_test_project_summary(
    project_id: ProjectId,
    maintainer_agent_id: AgentId,
    git_account_id: &str,
) -> ProjectSummary {
    let mut summary = test_project_summary(project_id, maintainer_agent_id, git_account_id);
    summary.status = ProjectStatus::Ready;
    summary.clone_status = ProjectCloneStatus::Ready;
    summary
}

fn github_pr(number: u64, draft: bool, head_sha: &str) -> Value {
    json!({
        "number": number,
        "draft": draft,
        "head": {
            "sha": head_sha,
        },
    })
}

fn github_commit(date: &str) -> Value {
    json!({
        "commit": {
            "committer": {
                "date": date,
            }
        }
    })
}

fn test_mcp_tool(server: &str, name: &str) -> McpTool {
    McpTool {
        server: server.to_string(),
        name: name.to_string(),
        model_name: mai_mcp::model_tool_name(server, name),
        description: format!("{server} {name}"),
        input_schema: json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "additionalProperties": false
        }),
        output_schema: None,
    }
}

fn fake_docker_path(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("fake-docker.sh");
    let log_path = fake_docker_log_path(dir);
    let workspace_root = dir.path().join("fake-sidecar-workspace");
    let script = format!(
        r#"#!/bin/sh
	LOG={}
	WORKSPACE={}
	last_created="created-container"
	case "$1" in
  ps)
    exit 0
    ;;
  commit)
    echo "$*" >> "$LOG"
    echo "sha256:snapshot"
    exit 0
    ;;
	  create)
	    echo "$*" >> "$LOG"
	    if printf '%s' "$*" | grep -q 'mai-review-skill-copy'; then
	      echo "review-copy-container"
	    else
	      echo "created-container"
	    fi
	    exit 0
	    ;;
	  run)
	    echo "$*" >> "$LOG"
	    command=""
	    last=""
	    for arg in "$@"; do
	      if [ "$last" = "-lc" ]; then
	        command="$arg"
	      fi
	      last="$arg"
	    done
	    if printf '%s' "$command" | grep -q "/workspace/repo/.claude/skills"; then
	      [ -d "$WORKSPACE/.claude/skills" ] && printf '%s\t%s\t%s\n' ".claude/skills" "claude" "/workspace/repo/.claude/skills"
	      [ -d "$WORKSPACE/.agents/skills" ] && printf '%s\t%s\t%s\n' ".agents/skills" "agents" "/workspace/repo/.agents/skills"
	      [ -d "$WORKSPACE/skills" ] && printf '%s\t%s\t%s\n' "skills" "skills" "/workspace/repo/skills"
	    fi
	    if printf '%s' "$command" | grep -q "fetch --prune origin"; then
	      echo "review-sync" >> "$LOG"
	    fi
	    if printf '%s' "$command" | grep -q "clone --no-checkout"; then
	      echo "sidecar-git-clone" >> "$LOG"
	      if [ -n "$MAI_GITHUB_INSTALLATION_TOKEN" ]; then
	        echo "token-present" >> "$LOG"
	      fi
	      mkdir -p "$WORKSPACE/repo/.git" "$WORKSPACE/.mai/install-log" "$WORKSPACE/.mai/tool-state" "$WORKSPACE/tmp"
	      printf 'hello\n' > "$WORKSPACE/repo/README.md"
	    fi
	    if printf '%s' "$command" | grep -q "gh api"; then
	      echo "sidecar-gh-api" >> "$LOG"
	      if [ -n "$GH_TOKEN" ]; then
	        echo "token-present" >> "$LOG"
	      fi
	      printf '{{"ok":true}}\n'
	    fi
	    exit 0
	    ;;
  rm|rmi|start)
    echo "$*" >> "$LOG"
    exit 0
    ;;
  exec)
    echo "$*" >> "$LOG"
    command=""
    last=""
    for arg in "$@"; do
      if [ "$last" = "-lc" ]; then
        command="$arg"
      fi
      last="$arg"
    done
	    if printf '%s' "$command" | grep -q "/workspace/repo/.claude/skills"; then
	      [ -d "$WORKSPACE/.claude/skills" ] && printf '%s\t%s\t%s\n' ".claude/skills" "claude" "/workspace/repo/.claude/skills"
	      [ -d "$WORKSPACE/.agents/skills" ] && printf '%s\t%s\t%s\n' ".agents/skills" "agents" "/workspace/repo/.agents/skills"
	      [ -d "$WORKSPACE/skills" ] && printf '%s\t%s\t%s\n' "skills" "skills" "/workspace/repo/skills"
	    fi
	    if printf '%s' "$command" | grep -q "git -c credential.helper= clone"; then
	      echo "sidecar-git-clone" >> "$LOG"
	      if [ -n "$MAI_GITHUB_INSTALLATION_TOKEN" ]; then
	        echo "token-present" >> "$LOG"
	      fi
	      mkdir -p "$WORKSPACE"
	      printf 'hello\n' > "$WORKSPACE/README.md"
	    fi
	    command=$(printf '%s' "$command" | sed "s#/workspace/repo#$WORKSPACE#g")
	    if printf '%s' "$command" | grep -q "sed -n"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "dd if="; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "^find "; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "rg --files"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "rg --json"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "printf /workspace"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "rm -f"; then
	      /bin/sh -lc "$command"
	    elif printf '%s' "$command" | grep -q "test -f"; then
	      /bin/sh -lc "$command"
	    fi
	    exit 0
	    ;;
    cp)
    echo "$*" >> "$LOG"
	    container="${{2%%:*}}"
	    if [ "$2" = "${{container}}:/workspace/repo/.claude/skills" ]; then
	      rm -rf "$3"
	      cp -R "$WORKSPACE/.claude/skills" "$3"
	    elif [ "$2" = "${{container}}:/workspace/repo/.agents/skills" ]; then
	      rm -rf "$3"
	      cp -R "$WORKSPACE/.agents/skills" "$3"
	    elif [ "$2" = "${{container}}:/workspace/repo/skills" ]; then
	      rm -rf "$3"
	      cp -R "$WORKSPACE/skills" "$3"
    elif printf '%s' "$3" | grep -q ':/tmp/.mai-team/skills'; then
      :
    elif printf '%s' "$3" | grep -q '^created-container:'; then
      dest="${{3#created-container:}}"
      dest=$(printf '%s' "$dest" | sed "s#/workspace/repo#$WORKSPACE#g")
      mkdir -p "$(dirname "$dest")"
      cp "$2" "$dest"
    elif printf '%s' "$2" | grep -q '^created-container:'; then
      src="${{2#created-container:}}"
      src=$(printf '%s' "$src" | sed "s#/workspace/repo#$WORKSPACE#g")
      mkdir -p "$(dirname "$3")"
      if [ -e "$src" ]; then
        cp "$src" "$3"
      else
        printf 'artifact\n' > "$3"
      fi
    fi
    exit 0
    ;;
  *)
    echo "$*" >> "$LOG"
    exit 0
    ;;
esac
"#,
        test_shell_quote(&log_path.to_string_lossy()),
        test_shell_quote(&workspace_root.to_string_lossy())
    );
    std::fs::write(&path, script).expect("write fake docker");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("fake docker metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod fake docker");
    }
    path.to_string_lossy().to_string()
}

fn fake_sidecar_workspace_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("fake-sidecar-workspace")
}

fn fake_git_path(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("fake-git.sh");
    let log_path = fake_git_log_path(dir);
    let script = format!(
        r#"#!/bin/sh
LOG={}
echo "$*" >> "$LOG"
if [ -n "$GIT_ASKPASS" ]; then
  echo "askpass=$GIT_ASKPASS" >> "$LOG"
fi
if [ -n "$MAI_GITHUB_INSTALLATION_TOKEN" ]; then
  echo "token-present" >> "$LOG"
fi
case "$1" in
  clone)
    last=""
    for arg in "$@"; do
      last="$arg"
    done
    mkdir -p "$last/.git"
    printf 'hello\n' > "$last/README.md"
    ;;
esac
exit 0
"#,
        test_shell_quote(&log_path.to_string_lossy())
    );
    std::fs::write(&path, script).expect("write fake git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("fake git metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod fake git");
    }
    path.to_string_lossy().to_string()
}

fn failing_git_path(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("failing-git.sh");
    let log_path = fake_git_log_path(dir);
    let script = format!(
        r#"#!/bin/sh
LOG={}
echo "$*" >> "$LOG"
if [ -n "$MAI_GITHUB_INSTALLATION_TOKEN" ]; then
  echo "token-present" >> "$LOG"
fi
echo "git clone failed with secret-token" >&2
exit 42
"#,
        test_shell_quote(&log_path.to_string_lossy())
    );
    std::fs::write(&path, script).expect("write failing git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("failing git metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod failing git");
    }
    path.to_string_lossy().to_string()
}

fn fake_docker_log_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("fake-docker.log")
}

fn fake_git_log_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("fake-git.log")
}

fn fake_docker_log(dir: &tempfile::TempDir) -> String {
    std::fs::read_to_string(fake_docker_log_path(dir)).unwrap_or_default()
}

fn fake_git_log(dir: &tempfile::TempDir) -> String {
    std::fs::read_to_string(fake_git_log_path(dir)).unwrap_or_default()
}

fn test_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn save_test_session(store: &ConfigStore, agent_id: AgentId, session_id: SessionId) {
    let timestamp = now();
    store
        .save_agent_session(
            agent_id,
            &AgentSessionSummary {
                id: session_id,
                title: "Chat 1".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
}

#[tokio::test]
async fn save_git_account_returns_verifying_without_waiting_for_verify() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "__close_without_response": true
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let runtime = test_runtime_with_github_api(&dir, Arc::clone(&store), base_url).await;

    let response = runtime
        .save_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");

    assert_eq!(response.account.status, GitAccountStatus::Verifying);
    assert_eq!(response.account.last_error, None);
}

#[tokio::test]
async fn verify_git_account_records_success_metadata() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "__headers": {
            "x-oauth-scopes": "repo, read:packages"
        },
        "login": "octo"
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "Personal".to_string(),
            token: Some("ghp_secret".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let runtime = test_runtime_with_github_api(&dir, Arc::clone(&store), base_url).await;

    let account = runtime
        .verify_git_account("account-1")
        .await
        .expect("verify account");

    assert_eq!(account.status, GitAccountStatus::Verified);
    assert_eq!(account.login.as_deref(), Some("octo"));
    assert_eq!(account.token_kind, GitTokenKind::Classic);
    assert!(account.scopes.contains(&"repo".to_string()));
    assert!(account.last_verified_at.is_some());
    assert_eq!(account.last_error, None);
}

#[tokio::test]
async fn verify_git_account_records_failed_http_error() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "__status": 401,
        "message": "Bad credentials"
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
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
    let runtime = test_runtime_with_github_api(&dir, Arc::clone(&store), base_url).await;

    let account = runtime
        .verify_git_account("account-1")
        .await
        .expect("verify account");

    assert_eq!(account.status, GitAccountStatus::Failed);
    assert!(account.last_verified_at.is_some());
    assert!(
        account
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("Bad credentials")
    );
}

#[test]
fn extracts_skill_mentions() {
    assert_eq!(
        extract_skill_mentions("please use $rust-dev, then $plugin:doc and $PATH."),
        vec!["rust-dev", "plugin:doc"]
    );
}

#[test]
fn agent_status_allows_new_turn_after_completion() {
    assert!(AgentStatus::Completed.can_start_turn());
    assert!(!AgentStatus::RunningTurn.can_start_turn());
}

#[tokio::test]
async fn create_task_persists_planner_metadata_and_allows_root_sessions() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider("http://localhost".to_string())],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let task = runtime
        .create_task(
            Some("Build task UI".to_string()),
            None,
            Some("ubuntu:latest".to_string()),
        )
        .await
        .expect("create task");

    assert_eq!(task.status, TaskStatus::Planning);
    assert_eq!(task.plan_status, PlanStatus::Missing);
    let detail = runtime.get_task(task.id, None).await.expect("task detail");
    assert_eq!(detail.agents.len(), 1);
    assert_eq!(detail.selected_agent.summary.role, Some(AgentRole::Planner));
    assert_eq!(detail.selected_agent.summary.task_id, Some(task.id));
    assert_eq!(detail.selected_agent.sessions.len(), 1);
    assert_eq!(detail.selected_agent.sessions[0].title, "Chat 1");
    let second = runtime
        .create_session(detail.selected_agent.summary.id)
        .await
        .expect("root task session");
    assert_eq!(second.title, "Chat 2");

    let explorer = runtime
        .spawn_task_role_agent(
            task.planner_agent_id,
            AgentRole::Explorer,
            Some("Explorer".to_string()),
        )
        .await
        .expect("explorer");
    assert!(runtime.create_session(explorer.id).await.is_err());

    let snapshot = store.load_runtime_snapshot(20).await.expect("snapshot");
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].summary.id, task.id);
    let planner = snapshot
        .agents
        .iter()
        .find(|agent| agent.summary.id == task.planner_agent_id)
        .expect("planner");
    assert_eq!(planner.summary.task_id, Some(task.id));
    assert_eq!(planner.summary.role, Some(AgentRole::Planner));
}

#[tokio::test]
async fn ensure_default_environment_creates_root_chat_environment() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let environment = runtime
        .ensure_default_environment()
        .await
        .expect("ensure default")
        .expect("default environment");

    assert_eq!(environment.name, "默认环境");
    assert_eq!(environment.conversation_count, 1);
    assert_eq!(environment.docker_image, "unused");
    let detail = runtime
        .get_environment(environment.id, None)
        .await
        .expect("environment detail");
    assert_eq!(detail.root_agent.sessions.len(), 1);
    assert_eq!(detail.root_agent.sessions[0].title, "Chat 1");
    assert_eq!(
        detail.current_conversation_id,
        detail.root_agent.sessions[0].id
    );
}

#[tokio::test]
async fn ensure_default_environment_does_not_require_provider_config() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let environment = runtime
        .ensure_default_environment()
        .await
        .expect("ensure default")
        .expect("default environment");

    assert_eq!(environment.name, "默认环境");
    assert_eq!(environment.conversation_count, 1);
    let detail = runtime
        .get_environment(environment.id, None)
        .await
        .expect("environment detail");
    assert_eq!(
        detail.root_agent.summary.provider_id,
        UNCONFIGURED_PROVIDER_ID
    );
    assert_eq!(detail.root_agent.summary.model, UNCONFIGURED_MODEL_ID);
    assert_eq!(detail.root_agent.sessions[0].title, "Chat 1");

    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    wait_until(
        || {
            let runtime = Arc::clone(&runtime);
            let agent_id = environment.root_agent_id;
            async move {
                runtime
                    .get_agent(agent_id, None)
                    .await
                    .map(|agent| agent.summary.status == AgentStatus::Idle)
                    .unwrap_or(false)
            }
        },
        Duration::from_secs(5),
    )
    .await;
    let updated = runtime
        .update_agent(
            environment.root_agent_id,
            UpdateAgentRequest {
                provider_id: Some("openai".to_string()),
                model: Some("gpt-5.5".to_string()),
                reasoning_effort: Some("high".to_string()),
            },
        )
        .await
        .expect("update unconfigured environment model");
    assert_eq!(updated.provider_id, "openai");
    assert_eq!(updated.model, "gpt-5.5");
}

#[tokio::test]
async fn create_environment_returns_before_container_is_ready() {
    let dir = tempdir().expect("tempdir");
    let docker = dir.path().join("slow-docker.sh");
    let log_path = fake_docker_log_path(&dir);
    fs::write(
        &docker,
        format!(
            r#"#!/bin/sh
LOG={}
case "$1" in
  ps)
    exit 0
    ;;
  create)
    echo "$*" >> "$LOG"
    sleep 2
    echo "created-container"
    exit 0
    ;;
  start)
    echo "$*" >> "$LOG"
    exit 0
    ;;
  rm|rmi)
    echo "$*" >> "$LOG"
    exit 0
    ;;
esac
exit 0
"#,
            log_path.display()
        ),
    )
    .expect("write slow docker");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&docker)
            .expect("slow docker metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&docker, permissions).expect("chmod slow docker");
    }
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = AgentRuntime::new(
        DockerClient::new_with_binary("slow-agent", docker.to_string_lossy().to_string()),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let started = Instant::now();
    let environment = runtime
        .create_environment("Chat".to_string(), Some("slow-agent".to_string()))
        .await
        .expect("create environment");

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "environment creation waited for container startup"
    );
    let detail = runtime
        .get_environment(environment.id, None)
        .await
        .expect("environment detail");
    assert_eq!(
        detail.root_agent.summary.status,
        AgentStatus::StartingContainer
    );
    wait_until(
        || {
            let runtime = Arc::clone(&runtime);
            let agent_id = environment.root_agent_id;
            async move {
                runtime
                    .get_agent(agent_id, None)
                    .await
                    .map(|detail| detail.summary.status == AgentStatus::Idle)
                    .unwrap_or(false)
            }
        },
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn create_environment_uses_name_docker_image_and_allows_multiple_conversations() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let environment = runtime
        .create_environment(" Research ".to_string(), Some(" ubuntu:24.04 ".to_string()))
        .await
        .expect("create environment");
    let second = runtime
        .create_environment_conversation(environment.id)
        .await
        .expect("second conversation");

    assert_eq!(environment.name, "Research");
    assert_eq!(environment.docker_image, "ubuntu:24.04");
    assert_eq!(second.title, "Chat 2");
    let summaries = runtime.list_environments().await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].conversation_count, 2);
    assert_eq!(summaries[0].docker_image, "ubuntu:24.04");
    let detail = runtime
        .get_environment(environment.id, Some(second.id))
        .await
        .expect("environment detail");
    assert_eq!(detail.root_agent.sessions.len(), 2);
    assert_eq!(detail.current_conversation_id, second.id);
}

#[tokio::test]
async fn send_environment_message_targets_selected_conversation_without_plan_transition() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "resp-1",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": "hello from selected chat"
                    }
                ]
            }
        ]
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let environment = runtime
        .create_environment("Chat".to_string(), Some("ubuntu:latest".to_string()))
        .await
        .expect("create environment");
    wait_for_agent_idle(
        Arc::clone(&runtime),
        environment.root_agent_id,
        std::time::Duration::from_secs(3),
    )
    .await;
    let second = runtime
        .create_environment_conversation(environment.id)
        .await
        .expect("second conversation");

    runtime
        .send_environment_message(environment.id, second.id, "hello".to_string(), Vec::new())
        .await
        .expect("send message");
    wait_for_agent_idle(
        Arc::clone(&runtime),
        environment.root_agent_id,
        std::time::Duration::from_secs(3),
    )
    .await;

    let detail = runtime
        .get_environment(environment.id, Some(second.id))
        .await
        .expect("environment detail");
    assert_eq!(detail.current_conversation_id, second.id);
    assert_eq!(detail.root_agent.messages.len(), 2);
    assert_eq!(detail.root_agent.messages[0].content, "hello");
    let first = detail
        .root_agent
        .sessions
        .iter()
        .find(|session| session.title == "Chat 1")
        .expect("first session");
    assert_eq!(first.message_count, 0);
    let task_detail = runtime
        .get_task(environment.id, None)
        .await
        .expect("internal backing record");
    assert_eq!(task_detail.summary.status, TaskStatus::Planning);
    assert_eq!(task_detail.plan.status, PlanStatus::Missing);
    assert_eq!(requests.lock().await.len(), 1);
}

#[tokio::test]
async fn environment_root_model_update_before_send_and_busy_rejection_remain() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "__close_without_response": true
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let mut provider = compact_test_provider(base_url);
    provider.models.push(test_model("mock-alt"));
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let environment = runtime
        .create_environment("Chat".to_string(), Some("ubuntu:latest".to_string()))
        .await
        .expect("create environment");
    wait_for_agent_idle(
        Arc::clone(&runtime),
        environment.root_agent_id,
        std::time::Duration::from_secs(3),
    )
    .await;

    let updated = runtime
        .update_agent(
            environment.root_agent_id,
            UpdateAgentRequest {
                provider_id: None,
                model: Some("mock-alt".to_string()),
                reasoning_effort: Some("high".to_string()),
            },
        )
        .await
        .expect("update model");

    assert_eq!(updated.model, "mock-alt");
    let detail = runtime
        .get_environment(environment.id, None)
        .await
        .expect("environment detail");
    let turn_id = runtime
        .send_environment_message(
            environment.id,
            detail.current_conversation_id,
            "hello".to_string(),
            Vec::new(),
        )
        .await
        .expect("send message");
    let busy = runtime
        .update_agent(
            environment.root_agent_id,
            UpdateAgentRequest {
                provider_id: None,
                model: Some("mock-model".to_string()),
                reasoning_effort: None,
            },
        )
        .await;
    assert!(matches!(
        busy,
        Err(RuntimeError::AgentBusy(id)) if id == environment.root_agent_id
    ));
    runtime
        .cancel_agent_turn(environment.root_agent_id, turn_id)
        .await
        .expect("cancel hung turn");
}

#[tokio::test]
async fn task_plan_tool_requires_planner_and_updates_task_status() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let task = runtime
        .create_task(
            Some("Plan me".to_string()),
            None,
            Some("ubuntu:latest".to_string()),
        )
        .await
        .expect("create task");

    let output = runtime
        .execute_tool_for_test(
            task.planner_agent_id,
            "save_task_plan",
            json!({
                "title": "Implementation plan",
                "markdown": "# Plan\n\nShip it carefully."
            }),
        )
        .await
        .expect("save plan");
    assert!(output.success);
    let detail = runtime.get_task(task.id, None).await.expect("task detail");
    assert_eq!(detail.summary.status, TaskStatus::AwaitingApproval);
    assert_eq!(detail.plan.status, PlanStatus::Ready);
    assert_eq!(detail.plan.version, 1);
    assert_eq!(detail.plan.title.as_deref(), Some("Implementation plan"));

    let explorer = runtime
        .spawn_task_role_agent(
            task.planner_agent_id,
            AgentRole::Explorer,
            Some("Explorer".to_string()),
        )
        .await
        .expect("explorer");
    let rejected = runtime
        .execute_tool_for_test(
            explorer.id,
            "save_task_plan",
            json!({
                "title": "Nope",
                "markdown": "Only planner may do this."
            }),
        )
        .await;
    assert!(rejected.is_err());
}

#[tokio::test]
async fn update_todo_list_accepts_todos_json_string_alias() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let output = runtime
            .execute_tool_for_test(
                agent_id,
                "update_todo_list",
                json!({
                    "todos": r#"[{"step":"获取认证用户信息和读取 helper 脚本","status":"in_progress"},{"step":"选择一个符合条件的 PR","status":"pending"}]"#
                }),
            )
            .await
            .expect("update todo list");

    assert!(output.success);
    let events = runtime.events.snapshot().await;
    let items = events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            ServiceEventKind::TodoListUpdated {
                agent_id: event_agent_id,
                items,
                ..
            } if *event_agent_id == agent_id => Some(items),
            _ => None,
        })
        .expect("todo event");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].step, "获取认证用户信息和读取 helper 脚本");
    assert_eq!(items[0].status, mai_protocol::TodoListStatus::InProgress);
}

#[tokio::test]
async fn read_file_returns_bounded_paged_output() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
    let workspace = fake_sidecar_workspace_path(&dir);
    fs::create_dir_all(&workspace).expect("mkdir workspace");
    fs::write(workspace.join("sample.txt"), "alpha\nbeta\ngamma\n").expect("write file");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let output = runtime
        .execute_tool_for_test(
            agent_id,
            "read_file",
            json!({
                "path": "/workspace/repo/sample.txt",
                "line_start": 2,
                "line_count": 2,
                "max_bytes": 20
            }),
        )
        .await
        .expect("read file");

    assert!(output.success);
    let value = serde_json::from_str::<Value>(&output.output).expect("json output");
    assert_eq!(value["text"], "beta\ngamma\n");
    assert_eq!(value["truncated"], false);
}

#[tokio::test]
async fn file_tools_list_files_respects_glob_and_limit() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
    let workspace = fake_sidecar_workspace_path(&dir);
    fs::create_dir_all(workspace.join("src")).expect("mkdir workspace");
    fs::write(workspace.join("src/a.rs"), "fn a() {}\n").expect("write file");
    fs::write(workspace.join("src/b.rs"), "fn b() {}\n").expect("write file");
    fs::write(workspace.join("README.md"), "hello\n").expect("write file");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let output = runtime
        .execute_tool_for_test(
            agent_id,
            "list_files",
            json!({
                "path": "/workspace/repo",
                "glob": "*.rs",
                "max_files": 1
            }),
        )
        .await
        .expect("list files");

    assert!(output.success);
    let value = serde_json::from_str::<Value>(&output.output).expect("json output");
    assert_eq!(value["count"], 1);
    assert_eq!(value["truncated"], true);
    assert!(
        value["files"]
            .as_array()
            .expect("files")
            .iter()
            .all(|path| path.as_str().is_some_and(|path| path.ends_with(".rs")))
    );
}

#[tokio::test]
async fn file_tools_search_files_returns_structured_matches() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
    let workspace = fake_sidecar_workspace_path(&dir);
    fs::create_dir_all(&workspace).expect("mkdir workspace");
    fs::write(workspace.join("one.txt"), "alpha\nbeta\n").expect("write file");
    fs::write(workspace.join("two.md"), "ALPHA\n").expect("write file");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let output = runtime
        .execute_tool_for_test(
            agent_id,
            "search_files",
            json!({
                "query": "alpha",
                "path": "/workspace/repo",
                "glob": "*.txt",
                "literal": true,
                "max_matches": 5
            }),
        )
        .await
        .expect("search files");

    assert!(output.success);
    let value = serde_json::from_str::<Value>(&output.output).expect("json output");
    assert_eq!(value["count"], 1);
    assert_eq!(value["matches"][0]["line"], 1);
    assert!(
        value["matches"][0]["text"]
            .as_str()
            .unwrap()
            .contains("alpha")
    );
}

#[tokio::test]
async fn file_tools_apply_patch_add_update_delete_and_move() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
    let workspace = fake_sidecar_workspace_path(&dir);
    fs::create_dir_all(&workspace).expect("mkdir workspace");
    fs::write(workspace.join("edit.txt"), "one\ntwo\nthree\n").expect("write file");
    fs::write(workspace.join("delete.txt"), "remove me\n").expect("write file");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let patch = "*** Begin Patch\n*** Add File: added.txt\n+created\n*** Update File: edit.txt\n*** Move to: moved.txt\n@@\n one\n-two\n+dos\n three\n*** Delete File: delete.txt\n*** End Patch\n";

    let output = runtime
        .execute_tool_for_test(
            agent_id,
            "apply_patch",
            json!({
                "cwd": "/workspace/repo",
                "input": patch
            }),
        )
        .await
        .expect("apply patch");

    assert!(output.success);
    assert_eq!(
        fs::read_to_string(workspace.join("added.txt")).expect("added"),
        "created\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("moved.txt")).expect("moved"),
        "one\ndos\nthree\n"
    );
    assert!(!workspace.join("edit.txt").exists());
    assert!(!workspace.join("delete.txt").exists());
    let value = serde_json::from_str::<Value>(&output.output).expect("json output");
    assert!(value["changed_files"].as_array().unwrap().len() >= 3);
}

#[tokio::test]
async fn file_tools_apply_patch_rejects_bad_paths_and_mismatched_hunks() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
    let workspace = fake_sidecar_workspace_path(&dir);
    fs::create_dir_all(&workspace).expect("mkdir workspace");
    fs::write(workspace.join("edit.txt"), "one\n").expect("write file");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let bad_path = runtime
        .execute_tool_for_test(
            agent_id,
            "apply_patch",
            json!({
                "cwd": "/workspace/repo",
                "input": "*** Begin Patch\n*** Add File: ../bad.txt\n+nope\n*** End Patch\n"
            }),
        )
        .await;
    assert!(bad_path.is_err());

    let mismatch = runtime
            .execute_tool_for_test(
                agent_id,
                "apply_patch",
                json!({
                    "cwd": "/workspace/repo",
                    "input": "*** Begin Patch\n*** Update File: edit.txt\n@@\n-two\n+dos\n*** End Patch\n"
                }),
            )
            .await;
    assert!(mismatch.is_err());
}

#[tokio::test]
async fn approving_task_without_ready_plan_fails() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = test_runtime(&dir, store).await;
    let task = runtime
        .create_task(
            Some("Needs plan".to_string()),
            None,
            Some("ubuntu:latest".to_string()),
        )
        .await
        .expect("create task");

    assert!(runtime.approve_task_plan(task.id).await.is_err());
}

#[tokio::test]
async fn delete_parent_cascades_to_children_and_grandchildren() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let sibling = Uuid::new_v4();
    let grandchild = Uuid::new_v4();
    save_agent_with_session(
        &store,
        &test_agent_summary(parent, Some("parent-container")),
    )
    .await;
    save_agent_with_session(
        &store,
        &test_agent_summary_with_parent(child, Some(parent), Some("child-container")),
    )
    .await;
    save_agent_with_session(
        &store,
        &test_agent_summary_with_parent(sibling, Some(parent), Some("sibling-container")),
    )
    .await;
    save_agent_with_session(
        &store,
        &test_agent_summary_with_parent(grandchild, Some(child), Some("grandchild-container")),
    )
    .await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    runtime.delete_agent(parent).await.expect("delete parent");

    assert!(runtime.list_agents().await.is_empty());
    assert!(
        store
            .load_runtime_snapshot(RECENT_EVENT_LIMIT)
            .await
            .expect("snapshot")
            .agents
            .is_empty()
    );
    let events = runtime.events.snapshot().await;
    let deleted = events
        .iter()
        .filter_map(|event| match event.kind {
            ServiceEventKind::AgentDeleted { agent_id } => Some(agent_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deleted, vec![grandchild, child, sibling, parent]);
}

#[tokio::test]
async fn delete_child_keeps_parent_and_sibling() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let sibling = Uuid::new_v4();
    save_agent_with_session(
        &store,
        &test_agent_summary(parent, Some("parent-container")),
    )
    .await;
    save_agent_with_session(
        &store,
        &test_agent_summary_with_parent(child, Some(parent), Some("child-container")),
    )
    .await;
    save_agent_with_session(
        &store,
        &test_agent_summary_with_parent(sibling, Some(parent), Some("sibling-container")),
    )
    .await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    runtime.delete_agent(child).await.expect("delete child");

    let remaining = runtime
        .list_agents()
        .await
        .into_iter()
        .map(|agent| agent.id)
        .collect::<HashSet<_>>();
    assert_eq!(remaining, HashSet::from([parent, sibling]));
}

#[test]
fn auto_compact_threshold_uses_last_context_tokens() {
    assert!(!should_auto_compact(0, 100));
    assert!(!should_auto_compact(89, 100));
    assert!(should_auto_compact(90, 100));
    assert!(should_auto_compact(360_000, 400_000));
    assert!(!should_auto_compact(90, 0));
}

#[test]
fn compact_summary_uses_last_non_empty_assistant_output() {
    let output = vec![
        ModelOutputItem::Message {
            text: "first".to_string(),
        },
        ModelOutputItem::Reasoning {
            content: "  second  ".to_string(),
        },
    ];

    assert_eq!(
        turn::history::compact_summary_from_output(&output).as_deref(),
        Some("second")
    );
    assert_eq!(turn::history::compact_summary_from_output(&[]), None);
}

#[test]
fn repair_adds_missing_tool_outputs_for_function_call_after_reasoning() {
    let mut history = vec![
        ModelInputItem::user_text("do something"),
        ModelInputItem::Reasoning {
            content: "thinking".to_string(),
        },
        ModelInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "container_exec".to_string(),
            arguments: "{}".to_string(),
        },
    ];
    turn::history::repair_incomplete_tool_history(&mut history);
    assert_eq!(history.len(), 4);
    assert!(matches!(
        &history[3],
        ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
    ));
}

#[test]
fn repair_adds_missing_tool_outputs_for_partial_results() {
    let mut history = vec![
        ModelInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "container_exec".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: "done".to_string(),
        },
        ModelInputItem::FunctionCall {
            call_id: "call_2".to_string(),
            name: "wait_agent".to_string(),
            arguments: "{}".to_string(),
        },
    ];
    turn::history::repair_incomplete_tool_history(&mut history);
    assert_eq!(history.len(), 4);
    assert!(matches!(
        &history[3],
        ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_2"
    ));
}

#[test]
fn repair_adds_missing_tool_outputs_for_function_call() {
    let mut history = vec![ModelInputItem::FunctionCall {
        call_id: "call_a".to_string(),
        name: "container_exec".to_string(),
        arguments: "{}".to_string(),
    }];
    turn::history::repair_incomplete_tool_history(&mut history);
    assert_eq!(history.len(), 2);
    assert!(matches!(
        &history[1],
        ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_a"
    ));
}

#[test]
fn repair_does_nothing_for_complete_history() {
    let mut history = vec![
        ModelInputItem::user_text("run"),
        ModelInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "container_exec".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: "ok".to_string(),
        },
        ModelInputItem::Message {
            role: "assistant".to_string(),
            content: vec![ModelContentItem::OutputText {
                text: "done".to_string(),
            }],
        },
    ];
    turn::history::repair_incomplete_tool_history(&mut history);
    assert_eq!(history.len(), 4);
}

#[test]
fn repair_does_nothing_for_empty_history() {
    let mut history: Vec<ModelInputItem> = vec![];
    turn::history::repair_incomplete_tool_history(&mut history);
    assert!(history.is_empty());
}

#[test]
fn repair_inserts_before_user_message() {
    let mut history = vec![
        ModelInputItem::user_text("do something"),
        ModelInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "container_exec".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::user_text("继续"),
    ];
    turn::history::repair_incomplete_tool_history(&mut history);
    // Should be: user, FunctionCall, FunctionCallOutput, user("继续")
    assert_eq!(history.len(), 4);
    assert!(matches!(
        &history[2],
        ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
    ));
    assert!(matches!(
        &history[3],
        ModelInputItem::Message { role, .. } if role == "user"
    ));
}

#[test]
fn repair_inserts_partial_before_user_message() {
    let mut history = vec![
        ModelInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "exec".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: "ok".to_string(),
        },
        ModelInputItem::FunctionCall {
            call_id: "call_2".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::user_text("继续"),
    ];
    turn::history::repair_incomplete_tool_history(&mut history);
    // Should be: FunctionCall(call_1), FCO(call_1), FunctionCall(call_2), FCO(call_2), user("继续")
    assert_eq!(history.len(), 5);
    assert!(matches!(
        &history[3],
        ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_2"
    ));
    assert!(matches!(
        &history[4],
        ModelInputItem::Message { role, .. } if role == "user"
    ));
}

#[test]
fn repair_keeps_consecutive_function_calls_in_one_batch() {
    let mut history = vec![
        ModelInputItem::Reasoning {
            content: "thinking".to_string(),
        },
        ModelInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "exec".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::FunctionCall {
            call_id: "call_2".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::FunctionCallOutput {
            call_id: "call_2".to_string(),
            output: "ok".to_string(),
        },
        ModelInputItem::user_text("继续"),
    ];
    turn::history::repair_incomplete_tool_history(&mut history);

    assert_eq!(history.len(), 6);
    assert!(matches!(
        &history[1],
        ModelInputItem::FunctionCall { call_id, .. } if call_id == "call_1"
    ));
    assert!(matches!(
        &history[2],
        ModelInputItem::FunctionCall { call_id, .. } if call_id == "call_2"
    ));
    assert!(matches!(
        &history[3],
        ModelInputItem::FunctionCallOutput { call_id, output }
            if call_id == "call_2" && output == "ok"
    ));
    assert!(matches!(
        &history[4],
        ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
    ));
    assert!(matches!(
        &history[5],
        ModelInputItem::Message { role, .. } if role == "user"
    ));
}

#[test]
fn compacted_history_keeps_recent_user_messages_and_summary_only() {
    let history = vec![
        ModelInputItem::user_text("first user"),
        ModelInputItem::assistant_text("assistant old"),
        ModelInputItem::user_text(turn::history::compact_summary_message(
            "old summary",
            COMPACT_SUMMARY_PREFIX,
        )),
        ModelInputItem::FunctionCall {
            call_id: "call_1".to_string(),
            name: "container_exec".to_string(),
            arguments: "{}".to_string(),
        },
        ModelInputItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: "{}".to_string(),
        },
        ModelInputItem::user_text("second user"),
    ];

    let compacted = turn::history::build_compacted_history(
        &history,
        "new summary",
        COMPACT_USER_MESSAGE_MAX_CHARS,
        COMPACT_SUMMARY_PREFIX,
    );
    assert_eq!(compacted.len(), 3);
    assert!(matches!(
        &compacted[0],
        ModelInputItem::Message { content, .. }
            if matches!(&content[0], ModelContentItem::InputText { text } if text == "first user")
    ));
    assert!(matches!(
        &compacted[1],
        ModelInputItem::Message { content, .. }
            if matches!(&content[0], ModelContentItem::InputText { text } if text == "second user")
    ));
    assert!(matches!(
        &compacted[2],
        ModelInputItem::Message { content, .. }
            if matches!(&content[0], ModelContentItem::InputText { text } if text.contains("new summary") && turn::history::is_compact_summary(text, COMPACT_SUMMARY_PREFIX))
    ));
}

#[test]
fn recent_user_messages_truncates_from_oldest_side() {
    let history = vec![
        ModelInputItem::user_text("abcdef"),
        ModelInputItem::user_text("ghij"),
    ];

    assert_eq!(
        turn::history::recent_user_messages(&history, 7, COMPACT_SUMMARY_PREFIX),
        vec!["def", "ghij"]
    );
}

#[tokio::test]
async fn restores_persisted_agents_and_continues_event_sequence() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open store");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let timestamp = now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "restored".to_string(),
        status: AgentStatus::RunningTurn,
        container_id: Some("old-container".to_string()),
        docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.2".to_string(),
        reasoning_effort: None,
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: Some(turn_id),
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    let message = AgentMessage {
        role: MessageRole::User,
        content: "hello".to_string(),
        created_at: timestamp,
    };
    store
        .save_agent(&summary, Some("system"))
        .await
        .expect("save agent");
    store
        .save_agent_session(
            agent_id,
            &AgentSessionSummary {
                id: session_id,
                title: "Chat 1".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
    store
        .append_agent_message(agent_id, session_id, 0, &message)
        .await
        .expect("save message");
    store
        .append_agent_history_item(agent_id, session_id, 0, &ModelInputItem::user_text("hello"))
        .await
        .expect("save history");
    store
        .append_service_event(&ServiceEvent {
            sequence: 41,
            timestamp,
            kind: ServiceEventKind::AgentMessage {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                role: MessageRole::User,
                content: "hello".to_string(),
            },
        })
        .await
        .expect("save event");
    drop(store);

    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("reopen store"),
    );
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        store,
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let agents = runtime.list_agents().await;
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].status, AgentStatus::Idle);
    assert_eq!(agents[0].container_id.as_deref(), Some("old-container"));
    assert_eq!(agents[0].current_turn, None);
    assert_eq!(
        agents[0].last_error.as_deref(),
        Some("interrupted by server restart")
    );

    let detail = runtime
        .get_agent(agent_id, Some(session_id))
        .await
        .expect("detail");
    assert_eq!(detail.selected_session_id, session_id);
    assert_eq!(detail.sessions.len(), 1);
    assert_eq!(detail.messages.len(), 1);
    assert_eq!(detail.messages[0].content, "hello");

    runtime
        .events
        .publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: AgentStatus::Failed,
        })
        .await;
    let events = runtime.events.snapshot().await;
    assert_eq!(events.last().expect("event").sequence, 42);
}

#[tokio::test]
async fn wait_agent_tool_returns_final_assistant_response() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let parent_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let child_session_id = Uuid::new_v4();
    let timestamp = now();
    let parent = test_agent_summary(parent_id, Some("parent-container"));
    let child = AgentSummary {
        id: child_id,
        parent_id: Some(parent_id),
        task_id: None,
        project_id: None,
        role: Some(AgentRole::Explorer),
        name: "Explorer".to_string(),
        status: AgentStatus::Completed,
        container_id: Some("child-container".to_string()),
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "mock".to_string(),
        provider_name: "Mock".to_string(),
        model: "mock-model".to_string(),
        reasoning_effort: Some("medium".to_string()),
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    store.save_agent(&parent, None).await.expect("save parent");
    save_test_session(&store, parent_id, Uuid::new_v4()).await;
    store.save_agent(&child, None).await.expect("save child");
    store
        .save_agent_session(
            child_id,
            &AgentSessionSummary {
                id: child_session_id,
                title: "Task".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save child session");
    store
        .append_agent_message(
            child_id,
            child_session_id,
            0,
            &AgentMessage {
                role: MessageRole::User,
                content: "Explore auth code".to_string(),
                created_at: timestamp,
            },
        )
        .await
        .expect("save user message");
    store
        .append_agent_message(
            child_id,
            child_session_id,
            1,
            &AgentMessage {
                role: MessageRole::Assistant,
                content: "Explorer conclusion: auth lives in crates/auth.".to_string(),
                created_at: timestamp,
            },
        )
        .await
        .expect("save assistant message");
    let runtime = test_runtime(&dir, store).await;

    let output = runtime
        .execute_tool_for_test(
            parent_id,
            "wait_agent",
            json!({
                "agent_id": child_id.to_string(),
                "timeout_secs": 1
            }),
        )
        .await
        .expect("wait agent");
    assert!(output.success);
    let value: Value = serde_json::from_str(&output.output).expect("wait output json");
    let completed = value["completed"].as_array().expect("completed");
    assert_eq!(completed.len(), 1);
    let child_output = &completed[0];
    assert_eq!(
        child_output["final_response"].as_str(),
        Some("Explorer conclusion: auth lives in crates/auth.")
    );
    assert_eq!(
        child_output["recent_messages"]
            .as_array()
            .expect("messages")
            .len(),
        2
    );
    assert_eq!(
        child_output["agent"]["id"].as_str(),
        Some(child_id.to_string().as_str())
    );
    assert_eq!(value["timed_out"].as_bool(), Some(false));
    assert!(matches!(
        runtime.agent(child_id).await,
        Err(RuntimeError::AgentNotFound(id)) if id == child_id
    ));
    assert!(
        runtime
            .list_agents()
            .await
            .iter()
            .all(|agent| agent.id != child_id)
    );
}

#[tokio::test]
async fn tool_trace_returns_full_history_with_event_metadata() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open store");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let timestamp = now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "trace".to_string(),
        status: AgentStatus::Completed,
        container_id: None,
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.2".to_string(),
        reasoning_effort: None,
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    store.save_agent(&summary, None).await.expect("save agent");
    store
        .save_agent_session(
            agent_id,
            &AgentSessionSummary {
                id: session_id,
                title: "Chat 1".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
    store
        .append_agent_history_item(
            agent_id,
            session_id,
            0,
            &ModelInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "container_exec".to_string(),
                arguments: r#"{"command":"printf hello","cwd":"/workspace"}"#.to_string(),
            },
        )
        .await
        .expect("save call");
    store
        .append_agent_history_item(
            agent_id,
            session_id,
            1,
            &ModelInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: r#"{"status":0,"stdout":"hello","stderr":""}"#.to_string(),
            },
        )
        .await
        .expect("save output");
    store
        .append_service_event(&ServiceEvent {
            sequence: 9,
            timestamp,
            kind: ServiceEventKind::ToolCompleted {
                agent_id,
                session_id: Some(session_id),
                turn_id,
                call_id: "call_1".to_string(),
                tool_name: "container_exec".to_string(),
                success: true,
                output_preview: "hello".to_string(),
                duration_ms: Some(27),
            },
        })
        .await
        .expect("save event");
    drop(store);

    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("reopen store"),
        ),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let trace = runtime
        .tool_trace(agent_id, Some(session_id), "call_1".to_string())
        .await
        .expect("trace");
    assert_eq!(trace.tool_name, "container_exec");
    assert_eq!(trace.arguments["command"], "printf hello");
    assert_eq!(trace.output, r#"{"status":0,"stdout":"hello","stderr":""}"#);
    assert!(trace.success);
    assert_eq!(trace.duration_ms, Some(27));
    assert_eq!(trace.agent_id, agent_id);
    assert_eq!(trace.session_id, Some(session_id));
    assert!(trace.output_preview.contains("\"stdout\": \"hello\""));
}

#[tokio::test]
async fn tool_trace_prefers_persisted_trace_records() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open store");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let timestamp = now();
    let summary = test_agent_summary(agent_id, None);
    store.save_agent(&summary, None).await.expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    store
        .save_tool_trace_completed(
            &ToolTraceDetail {
                agent_id,
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                call_id: "call_persisted".to_string(),
                tool_name: "container_exec".to_string(),
                arguments: json!({ "command": "printf persisted" }),
                output: r#"{"status":0,"stdout":"persisted","stderr":""}"#.to_string(),
                success: true,
                duration_ms: Some(99),
                started_at: Some(timestamp),
                completed_at: Some(timestamp),
                output_preview: "persisted".to_string(),
                output_artifacts: Vec::new(),
            },
            timestamp,
            timestamp,
        )
        .await
        .expect("save trace");
    drop(store);

    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("reopen store"),
        ),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let trace = runtime
        .tool_trace(agent_id, Some(session_id), "call_persisted".to_string())
        .await
        .expect("trace");
    assert_eq!(trace.turn_id, Some(turn_id));
    assert_eq!(trace.arguments["command"], "printf persisted");
    assert_eq!(trace.duration_ms, Some(99));
    assert_eq!(trace.output_preview, "persisted");
    assert_eq!(trace.started_at, Some(timestamp));
    assert_eq!(trace.completed_at, Some(timestamp));
}

#[tokio::test]
async fn tool_trace_finds_calls_stored_in_function_call_items() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = ConfigStore::open_with_config_path(&db_path, &config_path)
        .await
        .expect("open store");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let timestamp = now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "assistant-turn-trace".to_string(),
        status: AgentStatus::Completed,
        container_id: None,
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.2".to_string(),
        reasoning_effort: None,
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    store.save_agent(&summary, None).await.expect("save agent");
    store
        .save_agent_session(
            agent_id,
            &AgentSessionSummary {
                id: session_id,
                title: "Chat 1".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
    store
        .append_agent_history_item(
            agent_id,
            session_id,
            0,
            &ModelInputItem::FunctionCall {
                call_id: "call_nested".to_string(),
                name: "container_exec".to_string(),
                arguments: r#"{"command":"pwd"}"#.to_string(),
            },
        )
        .await
        .expect("save function call");
    store
        .append_agent_history_item(
            agent_id,
            session_id,
            1,
            &ModelInputItem::FunctionCallOutput {
                call_id: "call_nested".to_string(),
                output: r#"{"status":0,"stdout":"/workspace\n","stderr":""}"#.to_string(),
            },
        )
        .await
        .expect("save output");
    drop(store);

    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::new(
            ConfigStore::open_with_config_path(&db_path, &config_path)
                .await
                .expect("reopen store"),
        ),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let trace = runtime
        .tool_trace(agent_id, Some(session_id), "call_nested".to_string())
        .await
        .expect("trace");
    assert_eq!(trace.tool_name, "container_exec");
    assert_eq!(trace.arguments["command"], "pwd");
    assert_eq!(
        trace.output,
        r#"{"status":0,"stdout":"/workspace\n","stderr":""}"#
    );
    assert!(trace.success);
}

#[tokio::test]
async fn auto_compact_failure_keeps_original_history() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "id": "compact_empty",
        "output": [],
        "usage": { "input_tokens": 50, "output_tokens": 1, "total_tokens": 51 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_no_continuation_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let original_history = [
        ModelInputItem::user_text("original request"),
        ModelInputItem::assistant_text("original answer"),
    ];
    for (position, item) in original_history.iter().enumerate() {
        store
            .append_agent_history_item(agent_id, session_id, position, item)
            .await
            .expect("append history");
    }
    store
        .save_session_context_tokens(agent_id, session_id, 90)
        .await
        .expect("save tokens");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");
    let agent = runtime.agent(agent_id).await.expect("agent");

    let compacted = runtime
        .maybe_auto_compact(
            &agent,
            agent_id,
            session_id,
            Uuid::new_v4(),
            &CancellationToken::new(),
        )
        .await;

    assert!(matches!(compacted, Err(RuntimeError::InvalidInput(_))));
    let history = store
        .load_runtime_snapshot(10)
        .await
        .expect("snapshot")
        .agents[0]
        .sessions[0]
        .history
        .clone();
    assert_eq!(history.len(), original_history.len());
    assert!(matches!(
        &history[0],
        ModelInputItem::Message { content, .. }
            if matches!(&content[0], ModelContentItem::InputText { text } if text == "original request")
    ));
    assert!(matches!(
        &history[1],
        ModelInputItem::Message { content, .. }
            if matches!(&content[0], ModelContentItem::OutputText { text } if text == "original answer")
    ));
    assert_eq!(
        store
            .load_runtime_snapshot(10)
            .await
            .expect("snapshot")
            .agents[0]
            .sessions[0]
            .last_context_tokens,
        Some(90)
    );
}

#[tokio::test]
async fn auto_compact_runs_after_tool_output_before_next_model_request() {
    let (base_url, requests) = start_mock_responses(vec![
        json!({
            "id": "first",
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "unknown_tool",
                "arguments": "{}"
            }],
            "usage": { "input_tokens": 88, "output_tokens": 2, "total_tokens": 90 }
        }),
        json!({
            "id": "compact",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "summary after tool output" }]
            }],
            "usage": { "input_tokens": 20, "output_tokens": 5, "total_tokens": 25 }
        }),
        json!({
            "id": "second",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "final answer" }]
            }],
            "usage": { "input_tokens": 40, "output_tokens": 4, "total_tokens": 44 }
        }),
    ])
    .await;
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_no_continuation_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "please use a tool".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = requests.lock().await.clone();
    assert_eq!(requests.len(), 3);
    let visible_tools = turn::tools::visible_tool_names(&runtime.state, &agent, &[]).await;
    let expected_tool_count =
        build_tool_definitions_with_filter(&[], |name| visible_tools.contains(name)).len();
    assert_eq!(
        requests[0]["tools"].as_array().expect("first tools").len(),
        expected_tool_count
    );
    assert!(
        requests[1].get("tools").is_none(),
        "compact request should not send tools"
    );
    assert_eq!(
        requests[2]["tools"].as_array().expect("second tools").len(),
        expected_tool_count
    );
    let compact_input = requests[1]["input"].as_array().expect("compact input");
    assert!(matches!(
        compact_input.last(),
        Some(value) if value["content"][0]["text"].as_str().is_some_and(|text| text.contains("CONTEXT CHECKPOINT COMPACTION"))
    ));

    let snapshot = store.load_runtime_snapshot(20).await.expect("snapshot");
    let session = &snapshot.agents[0].sessions[0];
    assert_eq!(session.last_context_tokens, Some(44));
    assert!(session.history.iter().any(|item| matches!(
            item,
            ModelInputItem::Message { role, content }
                if role == "user"
                    && matches!(&content[0], ModelContentItem::InputText { text } if turn::history::is_compact_summary(text, COMPACT_SUMMARY_PREFIX) && text.contains("summary after tool output"))
        )));
    assert!(
        !session
            .history
            .iter()
            .any(|item| matches!(item, ModelInputItem::FunctionCallOutput { .. }))
    );
    assert_eq!(
        session
            .history
            .last()
            .and_then(turn::history::user_message_text),
        None
    );
    assert!(matches!(
        session.history.last(),
        Some(ModelInputItem::Message { role, content })
            if role == "assistant"
                && matches!(&content[0], ModelContentItem::OutputText { text } if text == "final answer")
    ));
    assert!(runtime.events.snapshot().await.iter().any(|event| matches!(
        event.kind,
        ServiceEventKind::ContextCompacted {
            tokens_before: 90,
            ..
        }
    )));
}

#[tokio::test]
async fn turn_loop_has_no_tool_iteration_budget() {
    let mut responses = Vec::new();
    for i in 0..205 {
        responses.push(json!({
            "id": format!("tool_{i}"),
            "output": [{
                "type": "function_call",
                "call_id": format!("call_{i}"),
                "name": "list_agents",
                "arguments": "{}"
            }],
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
        }));
    }
    responses.push(json!({
        "id": "final",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "done after many tools" }]
        }],
        "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
    }));
    let (base_url, requests) = start_mock_responses(responses).await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "keep going".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");

    assert_eq!(requests.lock().await.len(), 206);
    let (_, messages) = runtime
        .agent_recent_messages(agent_id, 4)
        .await
        .expect("messages");
    assert!(messages.iter().any(|message| {
        message.role == MessageRole::Assistant && message.content == "done after many tools"
    }));
}

#[tokio::test]
async fn user_turn_includes_selected_skill_as_user_fragment() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "skill",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "done" }]
        }],
        "usage": {
            "input_tokens": 10,
            "input_tokens_details": { "cached_tokens": 7 },
            "output_tokens": 2,
            "output_tokens_details": { "reasoning_tokens": 1 },
            "total_tokens": 12
        }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join(".agents/skills/demo");
    std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill.\n---\nUse the demo flow.",
    )
    .expect("write skill");
    let store = Arc::new(
        ConfigStore::open_with_config_path(
            dir.path().join("runtime.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    let turn_id = Uuid::new_v4();
    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            turn_id,
            "please use $demo".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = requests.lock().await.clone();
    assert_eq!(
        requests[0]["prompt_cache_key"],
        format!("agent:{agent_id}:session:{session_id}")
    );
    let input = requests[0]["input"].as_array().expect("input");
    assert!(input.iter().any(|item| {
        item["role"] == "user"
            && item["content"][0]["text"].as_str().is_some_and(|text| {
                text.contains("<skill>")
                    && text.contains("<name>demo</name>")
                    && text.contains("Use the demo flow.")
            })
    }));
    let events = runtime.events.snapshot().await;
    let activated = events
        .iter()
        .find_map(|event| match &event.kind {
            ServiceEventKind::SkillsActivated {
                agent_id: event_agent_id,
                session_id: event_session_id,
                turn_id: event_turn_id,
                skills,
            } if *event_agent_id == agent_id
                && *event_session_id == Some(session_id)
                && *event_turn_id == turn_id =>
            {
                Some(skills)
            }
            _ => None,
        })
        .expect("skills activated event");
    assert_eq!(activated.len(), 1);
    assert_eq!(activated[0].name, "demo");
    assert_eq!(activated[0].scope, mai_protocol::SkillScope::Repo);
    let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
    assert_eq!(
        snapshot.agents[0].summary.token_usage,
        TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 7,
            output_tokens: 2,
            reasoning_output_tokens: 1,
            total_tokens: 12,
        }
    );
}

#[tokio::test]
async fn user_turn_semantic_match_is_available_but_not_runtime_injected() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "skill",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join(".agents/skills/frontend-app-builder");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: frontend-app-builder\ndescription: Build frontend apps.\n---\nUse the frontend app builder flow.",
        )
        .expect("write skill");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "please use the frontend app builder".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = requests.lock().await.clone();
    let request_text = serde_json::to_string(&requests[0]).expect("request json");
    assert!(!request_text.contains("<name>frontend-app-builder</name>"));
    let instructions = requests[0]["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains("$frontend-app-builder"));
    assert!(instructions.contains("Build frontend apps."));
    assert!(instructions.contains("task clearly matches a skill's description"));
    assert!(
        !runtime
            .events
            .snapshot()
            .await
            .iter()
            .any(|event| matches!(event.kind, ServiceEventKind::SkillsActivated { .. }))
    );
}

#[tokio::test]
async fn disabled_skill_is_not_injected() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "skill",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join(".agents/skills/demo");
    std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill.\n---\nUse the demo flow.",
    )
    .expect("write skill");
    let store = Arc::new(
        ConfigStore::open_with_config_path(
            dir.path().join("runtime.sqlite3"),
            dir.path().join("config.toml"),
        )
        .await
        .expect("open store"),
    );
    store
        .save_skills_config(&SkillsConfigRequest {
            config: vec![mai_protocol::SkillConfigEntry {
                name: Some("demo".to_string()),
                path: None,
                enabled: false,
            }],
        })
        .await
        .expect("save skills config");
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "please use $demo".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let request_text = serde_json::to_string(&requests.lock().await[0]).expect("request json");
    assert!(!request_text.contains("<skill>"));
    assert!(!request_text.contains("Use the demo flow."));
    assert!(
        !runtime
            .events
            .snapshot()
            .await
            .iter()
            .any(|event| matches!(event.kind, ServiceEventKind::SkillsActivated { .. }))
    );
}

#[tokio::test]
async fn cancel_agent_turn_stops_model_request_and_marks_cancelled() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "__delay_ms": 5_000,
        "id": "slow",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "too late" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let turn_id = runtime
        .send_message(
            agent_id,
            Some(session_id),
            "slow please".to_string(),
            Vec::new(),
        )
        .await
        .expect("send");

    wait_until(
        || {
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .agent(agent_id)
                    .await
                    .expect("agent")
                    .summary
                    .read()
                    .await
                    .current_turn
                    == Some(turn_id)
            }
        },
        Duration::from_secs(2),
    )
    .await;
    runtime
        .cancel_agent_turn(agent_id, turn_id)
        .await
        .expect("cancel");

    let summary = runtime
        .agent(agent_id)
        .await
        .expect("agent")
        .summary
        .read()
        .await
        .clone();
    assert_eq!(summary.status, AgentStatus::Cancelled);
    assert_eq!(summary.current_turn, None);
    assert!(runtime.events.snapshot().await.iter().any(|event| matches!(
        event.kind,
        ServiceEventKind::TurnCompleted {
            agent_id: event_agent_id,
            turn_id: event_turn_id,
            status: TurnStatus::Cancelled,
            ..
        } if event_agent_id == agent_id && event_turn_id == turn_id
    )));
}

#[tokio::test]
async fn send_input_interrupt_starts_replacement_without_losing_message() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "id": "replacement",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "replacement done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let old_turn_id = Uuid::new_v4();
    let agent = runtime.agent(agent_id).await.expect("agent");
    {
        let mut summary = agent.summary.write().await;
        summary.status = AgentStatus::RunningTurn;
        summary.current_turn = Some(old_turn_id);
    }
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });
    *agent.active_turn.lock().expect("active turn lock") = Some(TurnControl {
        turn_id: old_turn_id,
        session_id,
        cancellation_token: CancellationToken::new(),
        abort_handle: None,
    });

    let output = runtime
        .send_input_to_agent(
            agent_id,
            Some(session_id),
            "replacement".to_string(),
            Vec::new(),
            true,
        )
        .await
        .expect("interrupt");
    assert_eq!(output["queued"].as_bool(), Some(false));
    wait_until(
        || {
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .agent(agent_id)
                    .await
                    .expect("agent")
                    .summary
                    .read()
                    .await
                    .current_turn
                    .is_none()
            }
        },
        Duration::from_secs(2),
    )
    .await;

    let detail = runtime
        .get_agent(agent_id, Some(session_id))
        .await
        .expect("detail");
    let message_dump = detail
        .messages
        .iter()
        .map(|message| format!("{:?}: {}", message.role, message.content))
        .collect::<Vec<_>>()
        .join(" | ");
    let event_dump = runtime
        .events
        .snapshot()
        .await
        .iter()
        .map(|event| format!("{:?}", event.kind))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        detail.messages.iter().any(|message| {
            message.role == MessageRole::User && message.content == "replacement"
        }),
        "messages: {message_dump}; status: {:?}; events: {event_dump}",
        detail.summary.status
    );
    assert!(
        detail.messages.iter().any(|message| {
            message.role == MessageRole::Assistant && message.content == "replacement done"
        }),
        "messages: {message_dump}; status: {:?}; error: {:?}; events: {event_dump}",
        detail.summary.status,
        detail.summary.last_error
    );
}

#[tokio::test]
async fn stale_turn_completion_does_not_overwrite_current_turn() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent = runtime.agent(agent_id).await.expect("agent");
    let stale_turn_id = Uuid::new_v4();
    let current_turn_id = Uuid::new_v4();
    {
        let mut summary = agent.summary.write().await;
        summary.status = AgentStatus::RunningTurn;
        summary.current_turn = Some(current_turn_id);
    }
    *agent.active_turn.lock().expect("active turn lock") = Some(TurnControl {
        turn_id: current_turn_id,
        session_id,
        cancellation_token: CancellationToken::new(),
        abort_handle: None,
    });

    let completed = turn::completion::complete_turn_if_current(
        runtime.deps.store.as_ref(),
        &runtime.events,
        &agent,
        agent_id,
        TurnResult {
            turn_id: stale_turn_id,
            status: TurnStatus::Cancelled,
            agent_status: AgentStatus::Cancelled,
            final_text: None,
            error: None,
        },
    )
    .await
    .expect("complete stale");

    assert!(!completed);
    let summary = agent.summary.read().await.clone();
    assert_eq!(summary.status, AgentStatus::RunningTurn);
    assert_eq!(summary.current_turn, Some(current_turn_id));
    assert!(runtime.events.snapshot().await.iter().all(|event| {
        !matches!(
            event.kind,
            ServiceEventKind::TurnCompleted {
                turn_id,
                status: TurnStatus::Cancelled,
                ..
            } if turn_id == stale_turn_id
        )
    }));
}

#[tokio::test]
async fn save_artifact_uses_configured_artifact_roots() {
    let dir = tempdir().expect("tempdir");
    let artifact_index_root = dir.path().join("data/artifacts/index");
    let store = Arc::new(
        ConfigStore::open_with_config_and_artifact_index_path(
            dir.path().join("runtime.sqlite3"),
            dir.path().join("config.toml"),
            &artifact_index_root,
        )
        .await
        .expect("open store"),
    );
    let task_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("created-container"));
    agent.task_id = Some(task_id);
    store.save_agent(&agent, None).await.expect("save agent");
    let plan = TaskPlan::default();
    let timestamp = now();
    let task = TaskSummary {
        id: task_id,
        title: "Artifact Task".to_string(),
        status: TaskStatus::Planning,
        plan_status: plan.status.clone(),
        plan_version: plan.version,
        planner_agent_id: agent_id,
        current_agent_id: Some(agent_id),
        agent_count: 1,
        review_rounds: 0,
        created_at: timestamp,
        updated_at: timestamp,
        last_error: None,
        final_report: None,
    };
    store.save_task(&task, &plan).await.expect("save task");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent_record = runtime.agent(agent_id).await.expect("agent");
    *agent_record.container.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "created-container".to_string(),
        image: "unused".to_string(),
    });

    let artifact = runtime
        .save_artifact(
            agent_id,
            "/workspace/report.txt".to_string(),
            Some("report.txt".to_string()),
        )
        .await
        .expect("save artifact");

    let file_path = dir
        .path()
        .join("data/artifacts/files")
        .join(task_id.to_string())
        .join(&artifact.id)
        .join("report.txt");
    assert_eq!(runtime.artifact_file_path(&artifact), file_path);
    assert_eq!(
        fs::read_to_string(&file_path).expect("artifact file"),
        "artifact\n"
    );
    assert!(
        artifact_index_root
            .join(format!("{}.json", artifact.id))
            .exists()
    );
    let artifacts = store.load_artifacts(&task_id).expect("load artifacts");
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].id, artifact.id);
    assert_eq!(artifacts[0].task_id, task_id);
    assert_eq!(artifacts[0].name, "report.txt");
    assert!(!dir.path().join("artifacts").exists());
}

#[tokio::test]
async fn project_skill_cache_lists_project_scope_with_source_paths() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("container-1"));
    agent.project_id = Some(project_id);
    save_agent_with_session(&store, &agent).await;
    let project = ready_test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let cache_dir = runtime.project_skill_cache_dir(project_id);
    assert_eq!(
        cache_dir,
        dir.path()
            .join("cache")
            .join(PROJECT_SKILLS_CACHE_DIR)
            .join(project_id.to_string())
    );
    assert!(!dir.path().join(PROJECT_SKILLS_CACHE_DIR).exists());
    let skill_dir = cache_dir.join("claude").join("demo");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: project-demo\ndescription: Project demo skill.\n---\nUse project demo.",
    )
    .expect("write skill");

    let response = runtime
        .list_project_skills(project_id)
        .await
        .expect("project skills");

    assert!(response.errors.is_empty());
    assert_eq!(response.skills.len(), 1);
    assert_eq!(response.skills[0].scope, SkillScope::Project);
    assert_eq!(
        response.skills[0].source_path.as_deref(),
        Some(Path::new("/workspace/repo/.claude/skills/demo/SKILL.md"))
    );
    assert_eq!(
        response.roots,
        vec![PathBuf::from("/workspace/repo/.claude/skills")]
    );
}

#[tokio::test]
async fn detects_project_skills_from_sidecar_candidate_dirs() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("container-1"));
    agent.project_id = Some(project_id);
    save_agent_with_session(&store, &agent).await;
    let project = ready_test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let workspace = ensure_project_clone(&dir, project_id, agent_id);
    let claude_skill = workspace.join(".claude/skills/claude-demo");
    let agents_skill = workspace.join(".agents/skills/agents-demo");
    let root_skill = workspace.join("skills/root-demo");
    for (path, name) in [
        (&claude_skill, "claude-demo"),
        (&agents_skill, "agents-demo"),
        (&root_skill, "root-demo"),
    ] {
        fs::create_dir_all(path).expect("mkdir skill");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name}\n---\nBody."),
        )
        .expect("write skill");
    }
    fs::create_dir_all(workspace.join("template/ignored")).expect("mkdir ignored");
    fs::write(
        workspace.join("template/ignored/SKILL.md"),
        "---\nname: ignored\ndescription: ignored\n---\nIgnored.",
    )
    .expect("write ignored");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project = runtime.project(project_id).await.expect("project");
    *project.sidecar.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "sidecar".to_string(),
        image: "unused".to_string(),
    });

    let response = runtime
        .detect_project_skills(project_id)
        .await
        .expect("detect skills");

    let names = response
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["agents-demo", "claude-demo", "root-demo"]);
    assert!(
        response
            .skills
            .iter()
            .all(|skill| skill.scope == SkillScope::Project)
    );
    assert_eq!(response.roots.len(), 3);
    assert!(
        runtime
            .project_skill_cache_dir(project_id)
            .join("claude/claude-demo/SKILL.md")
            .exists()
    );
    assert!(!names.contains(&"ignored"));
}

#[tokio::test]
async fn project_skill_refresh_serializes_cache_replacement() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("created-container"));
    agent.project_id = Some(project_id);
    save_agent_with_session(&store, &agent).await;
    let project = ready_test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project = runtime.project(project_id).await.expect("project");
    *project.sidecar.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "sidecar".to_string(),
        image: "unused".to_string(),
    });
    write_project_skill(
        &runtime,
        project_id,
        "serialized-refresh",
        "Old serialized skill.",
        "Old body.",
    );
    write_workspace_project_skill(
        &dir,
        project_id,
        agent_id,
        ".claude/skills",
        "serialized-refresh",
        "New serialized skill.",
        "New body.",
    );
    let reader_runtime = Arc::clone(&runtime);
    let refresher_runtime = Arc::clone(&runtime);

    let (read_result, refresh_result) = tokio::join!(
        async move { reader_runtime.project_skills_from_cache(project_id).await },
        async move { refresher_runtime.detect_project_skills(project_id).await },
    );

    read_result.expect("read skills");
    refresh_result.expect("refresh skills");
    let response = runtime
        .project_skills_from_cache(project_id)
        .await
        .expect("skills after refresh");
    let skill = response
        .skills
        .iter()
        .find(|skill| skill.name == "serialized-refresh")
        .expect("serialized skill");
    assert_eq!(skill.description, "New serialized skill.");
    assert!(
        fs::read_to_string(&skill.path)
            .expect("skill body")
            .contains("New body.")
    );
}

#[tokio::test]
async fn project_turn_injects_selected_project_skill_path() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "project-skill",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("container-1"));
    agent.project_id = Some(project_id);
    store.save_agent(&agent, None).await.expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let project = ready_test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent_record = runtime.agent(agent_id).await.expect("agent");
    *agent_record.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });
    let project = runtime.project(project_id).await.expect("project");
    *project.sidecar.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "sidecar".to_string(),
        image: "unused".to_string(),
    });
    let skill_dir = runtime
        .project_skill_cache_dir(project_id)
        .join("claude")
        .join("demo");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    let skill_path = skill_dir.join("SKILL.md");
    fs::write(
        &skill_path,
        "---\nname: demo\ndescription: Project demo skill.\n---\nUse the project workflow.",
    )
    .expect("write skill");
    write_workspace_project_skill(
        &dir,
        project_id,
        agent_id,
        ".claude/skills",
        "demo",
        "Project demo skill.",
        "Use the project workflow.",
    );

    let turn_id = Uuid::new_v4();
    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            turn_id,
            "please help".to_string(),
            vec![skill_path.display().to_string()],
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = requests.lock().await.clone();
    let input = requests[0]["input"].as_array().expect("input");
    assert!(input.iter().any(|item| {
        item["role"] == "user"
            && item["content"][0]["text"].as_str().is_some_and(|text| {
                text.contains("<skill>") && text.contains("Use the project workflow.")
            })
    }));
    let instructions = requests[0]["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains("Project demo skill."));
    assert!(instructions.contains("/workspace/repo/.claude/skills/demo/SKILL.md"));
    let events = runtime.events.snapshot().await;
    let activated = events
        .iter()
        .find_map(|event| match &event.kind {
            ServiceEventKind::SkillsActivated {
                agent_id: event_agent_id,
                turn_id: event_turn_id,
                skills,
                ..
            } if *event_agent_id == agent_id && *event_turn_id == turn_id => Some(skills),
            _ => None,
        })
        .expect("skills activated");
    assert_eq!(activated.len(), 1);
    assert_eq!(activated[0].scope, SkillScope::Project);
}

#[tokio::test]
async fn project_turn_refreshes_stale_project_skill_cache_before_injection() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "project-skill-refresh",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("created-container"));
    agent.project_id = Some(project_id);
    store.save_agent(&agent, None).await.expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let project = ready_test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent_record = runtime.agent(agent_id).await.expect("agent");
    *agent_record.container.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "created-container".to_string(),
        image: "unused".to_string(),
    });
    let project = runtime.project(project_id).await.expect("project");
    *project.sidecar.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "sidecar".to_string(),
        image: "unused".to_string(),
    });
    write_project_skill(
        &runtime,
        project_id,
        "dynamic-demo",
        "Old project skill.",
        "Old cached body.",
    );
    write_workspace_project_skill(
        &dir,
        project_id,
        agent_id,
        ".claude/skills",
        "dynamic-demo",
        "New project skill.",
        "New workspace body.",
    );

    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "please use dynamic-demo".to_string(),
            vec!["dynamic-demo".to_string()],
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = requests.lock().await.clone();
    let input = requests[0]["input"].as_array().expect("input");
    assert!(input.iter().any(|item| {
        item["role"] == "user"
            && item["content"][0]["text"].as_str().is_some_and(|text| {
                text.contains("<name>dynamic-demo</name>")
                    && text.contains("New workspace body.")
                    && !text.contains("Old cached body.")
            })
    }));
    let instructions = requests[0]["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains("New project skill."));
}

#[tokio::test]
async fn project_skill_plain_name_is_ambiguous_until_path_selected() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("container-1"));
    agent.project_id = Some(project_id);
    save_agent_with_session(&store, &agent).await;
    let project = ready_test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let global_skill_dir = dir.path().join(".agents/skills/demo");
    fs::create_dir_all(&global_skill_dir).expect("mkdir global");
    fs::write(
        global_skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Global demo.\n---\nGlobal.",
    )
    .expect("write global");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project_skill_dir = runtime
        .project_skill_cache_dir(project_id)
        .join("claude")
        .join("demo");
    fs::create_dir_all(&project_skill_dir).expect("mkdir project");
    let project_skill_path = project_skill_dir.join("SKILL.md");
    fs::write(
        &project_skill_path,
        "---\nname: demo\ndescription: Project demo.\n---\nProject.",
    )
    .expect("write project");
    let skills_manager = runtime.skills_manager_with_project_roots(project_id);

    let ambiguous = skills_manager
        .build_injections(&["demo".to_string()], &SkillsConfigRequest::default())
        .expect("ambiguous injection");
    assert!(ambiguous.items.is_empty());

    let selected = skills_manager
        .build_injections(
            &[project_skill_path.display().to_string()],
            &SkillsConfigRequest::default(),
        )
        .expect("path injection");
    assert_eq!(selected.items.len(), 1);
    assert_eq!(selected.items[0].metadata.scope, SkillScope::Project);
}

#[tokio::test]
async fn project_skill_detection_requires_ready_workspace() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("container-1"));
    agent.project_id = Some(project_id);
    save_agent_with_session(&store, &agent).await;
    let project = test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let err = runtime
        .detect_project_skills(project_id)
        .await
        .expect_err("not ready");

    assert!(err.to_string().contains("workspace is not ready"));
}

#[tokio::test]
async fn model_failure_after_tool_keeps_tool_success_event_separate() {
    let (base_url, _requests) = start_mock_responses(vec![
        json!({
            "id": "first",
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "list_agents",
                "arguments": "{}"
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        }),
        json!({ "__close_without_response": true }),
    ])
    .await;
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    let result = runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "please list agents".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_err());
    let events = runtime.events.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        ServiceEventKind::ToolCompleted {
            call_id,
            tool_name,
            success: true,
            ..
        } if call_id == "call_1" && tool_name == "list_agents"
    )));
    drop(events);
    let snapshot = store.load_runtime_snapshot(20).await.expect("snapshot");
    assert!(snapshot.agents[0].sessions[0].history.iter().any(|item| {
        matches!(
            item,
            ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
        )
    }));
}

#[tokio::test]
async fn sessions_are_created_and_selected_independently() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let agent = runtime
        .create_agent(CreateAgentRequest {
            name: Some("chat-agent".to_string()),
            provider_id: Some("openai".to_string()),
            model: Some("gpt-5.5".to_string()),
            reasoning_effort: Some("high".to_string()),
            docker_image: None,
            parent_id: None,
            system_prompt: None,
        })
        .await;
    assert!(
        agent.is_err(),
        "unused docker cannot start, but agent is persisted"
    );
    let agent = runtime.list_agents().await[0].clone();
    assert_eq!(agent.reasoning_effort, Some("high".to_string()));
    assert_eq!(agent.docker_image, "unused");
    let first = runtime
        .get_agent(agent.id, None)
        .await
        .expect("first detail");
    assert_eq!(first.sessions.len(), 1);
    assert_eq!(first.sessions[0].title, "Chat 1");

    let second = runtime.create_session(agent.id).await.expect("new session");
    assert_eq!(second.title, "Chat 2");
    let detail = runtime
        .get_agent(agent.id, Some(second.id))
        .await
        .expect("second detail");
    assert_eq!(detail.sessions.len(), 2);
    assert_eq!(detail.selected_session_id, second.id);
    assert!(detail.messages.is_empty());
    assert_eq!(
        detail
            .context_usage
            .as_ref()
            .map(|usage| usage.context_tokens),
        Some(400_000)
    );
    assert_eq!(
        detail
            .context_usage
            .as_ref()
            .map(|usage| usage.threshold_percent),
        Some(90)
    );

    let reopened = store.load_runtime_snapshot(10).await.expect("snapshot");
    assert_eq!(reopened.agents[0].sessions.len(), 2);
    assert_eq!(
        reopened.agents[0].summary.reasoning_effort,
        Some("high".to_string())
    );
}

#[tokio::test]
async fn agent_detail_uses_deepseek_v4_context_tokens() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![deepseek_test_provider()],
            default_provider_id: Some("deepseek".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let timestamp = now();
    store
        .save_agent(
            &AgentSummary {
                id: agent_id,
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                name: "deepseek-context".to_string(),
                status: AgentStatus::Idle,
                container_id: None,
                docker_image: "ubuntu:latest".to_string(),
                provider_id: "deepseek".to_string(),
                provider_name: "DeepSeek".to_string(),
                model: "deepseek-v4-pro".to_string(),
                reasoning_effort: Some("high".to_string()),
                created_at: timestamp,
                updated_at: timestamp,
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
                id: Uuid::new_v4(),
                title: "Chat 1".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        store,
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let detail = runtime.get_agent(agent_id, None).await.expect("detail");

    assert_eq!(
        detail
            .context_usage
            .as_ref()
            .map(|usage| usage.context_tokens),
        Some(1_000_000)
    );
    assert_eq!(
        detail.context_usage.as_ref().map(|usage| usage.used_tokens),
        Some(0)
    );
}

#[tokio::test]
async fn agent_config_resolves_effective_default_and_validates_updates() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider(), alt_test_provider()],
            default_provider_id: Some("alt".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let config = runtime.agent_config().await.expect("config");
    assert_eq!(config.planner, None);
    assert_eq!(config.explorer, None);
    assert_eq!(config.executor, None);
    assert_eq!(config.reviewer, None);
    let effective = config.effective_executor.expect("effective default");
    assert_eq!(effective.provider_id, "alt");
    assert_eq!(effective.model, "alt-default");
    assert_eq!(effective.reasoning_effort, Some("medium".to_string()));
    assert_eq!(
        config.effective_planner.expect("planner default").model,
        "alt-default"
    );
    assert_eq!(
        config.effective_explorer.expect("explorer default").model,
        "alt-default"
    );
    assert_eq!(
        config.effective_reviewer.expect("reviewer default").model,
        "alt-default"
    );

    let updated = runtime
        .update_agent_config(AgentConfigRequest {
            executor: Some(AgentModelPreference {
                provider_id: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some("high".to_string()),
            }),
            ..Default::default()
        })
        .await
        .expect("update");
    assert_eq!(
        updated.effective_executor.expect("effective").model,
        "gpt-5.4"
    );

    let invalid = runtime
        .update_agent_config(AgentConfigRequest {
            reviewer: Some(AgentModelPreference {
                provider_id: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some("max".to_string()),
            }),
            ..Default::default()
        })
        .await;
    assert!(matches!(invalid, Err(RuntimeError::InvalidInput(_))));
}

#[tokio::test]
async fn role_model_resolution_falls_back_when_saved_provider_is_removed() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .save_agent_config(&AgentConfigRequest {
            reviewer: Some(AgentModelPreference {
                provider_id: "mimo-token-plan".to_string(),
                model: "mimo-v1".to_string(),
                reasoning_effort: None,
            }),
            ..Default::default()
        })
        .await
        .expect("save stale config");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let resolved = runtime
        .resolve_role_agent_model(AgentRole::Reviewer)
        .await
        .expect("fallback reviewer model");

    assert_eq!(resolved.preference.provider_id, "openai");
    assert_eq!(resolved.preference.model, "gpt-5.5");
}

#[tokio::test]
async fn project_detail_selects_live_reviewer_without_replacing_maintainer() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    let mut reviewer =
        test_agent_summary_with_parent(reviewer_id, None, Some("reviewer-container"));
    reviewer.project_id = Some(project_id);
    reviewer.role = Some(AgentRole::Reviewer);
    save_agent_with_session(&store, &maintainer).await;
    save_agent_with_session(&store, &reviewer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let detail = runtime
        .get_project(project_id, Some(reviewer_id), None)
        .await
        .expect("project detail");

    assert_eq!(detail.selected_agent_id, reviewer_id);
    assert_eq!(detail.selected_agent.summary.id, reviewer_id);
    assert_eq!(
        detail.selected_agent.summary.role,
        Some(AgentRole::Reviewer)
    );
    assert_eq!(detail.maintainer_agent.summary.id, maintainer_id);
    assert_eq!(
        detail.maintainer_agent.summary.role,
        Some(AgentRole::Planner)
    );
    assert!(detail.agents.iter().any(|agent| agent.id == maintainer_id));
    assert!(detail.agents.iter().any(|agent| agent.id == reviewer_id));
}

#[tokio::test]
async fn project_detail_falls_back_to_maintainer_when_selected_reviewer_is_gone() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let reviewer_session_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let detail = runtime
        .get_project(project_id, Some(reviewer_id), Some(reviewer_session_id))
        .await
        .expect("project detail");

    assert_eq!(detail.selected_agent_id, maintainer_id);
    assert_eq!(detail.selected_agent.summary.id, maintainer_id);
    assert_eq!(detail.maintainer_agent.summary.id, maintainer_id);
}

#[tokio::test]
async fn spawn_agent_uses_executor_default_when_role_omitted() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider(), alt_test_provider()],
            default_provider_id: Some("alt".to_string()),
        })
        .await
        .expect("save providers");
    let parent_id = Uuid::new_v4();
    let timestamp = now();
    store
        .save_agent(
            &AgentSummary {
                id: parent_id,
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                name: "parent".to_string(),
                status: AgentStatus::Idle,
                container_id: None,
                docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
                provider_id: "openai".to_string(),
                provider_name: "OpenAI".to_string(),
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some("high".to_string()),
                created_at: timestamp,
                updated_at: timestamp,
                current_turn: None,
                last_error: None,
                token_usage: TokenUsage::default(),
            },
            None,
        )
        .await
        .expect("save parent");
    store
        .save_agent_session(
            parent_id,
            &AgentSessionSummary {
                id: Uuid::new_v4(),
                title: "Chat 1".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let parent = runtime.agent(parent_id).await.expect("parent");
    *parent.container.write().await = Some(ContainerHandle {
        id: "parent-container".to_string(),
        name: "parent-container".to_string(),
        image: "unused".to_string(),
    });

    let result = runtime
        .execute_tool_for_test(
            parent_id,
            "spawn_agent",
            json!({
                "name": "child",
                "provider_id": "openai",
                "model": "gpt-5.4"
            }),
        )
        .await;
    assert!(result.expect("spawn agent").success);
    let child = runtime
        .list_agents()
        .await
        .into_iter()
        .find(|agent| agent.parent_id == Some(parent_id))
        .expect("child");
    assert_eq!(child.provider_id, "openai");
    assert_eq!(child.model, "gpt-5.4");
    assert_eq!(child.reasoning_effort, Some("high".to_string()));
    assert_eq!(
        child.docker_image,
        "ghcr.io/rcore-os/tgoskits-container:latest"
    );
    let docker_log = fake_docker_log(&dir);
    assert!(docker_log.contains("commit parent-container mai-team-snapshot-"));
    assert!(docker_log.contains(&format!("create --name mai-team-{}", child.id)));
    assert!(docker_log.contains("rmi -f mai-team-snapshot-"));
}

#[tokio::test]
async fn spawn_agent_uses_role_config_over_parent_defaults() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider(), alt_test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .save_agent_config(&AgentConfigRequest {
            planner: Some(AgentModelPreference {
                provider_id: "alt".to_string(),
                model: "alt-default".to_string(),
                reasoning_effort: Some("medium".to_string()),
            }),
            explorer: Some(AgentModelPreference {
                provider_id: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning_effort: Some("medium".to_string()),
            }),
            executor: Some(AgentModelPreference {
                provider_id: "alt".to_string(),
                model: "alt-research".to_string(),
                reasoning_effort: Some("low".to_string()),
            }),
            reviewer: Some(AgentModelPreference {
                provider_id: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some("high".to_string()),
            }),
        })
        .await
        .expect("save config");
    let parent_id = Uuid::new_v4();
    let timestamp = now();
    store
        .save_agent(
            &AgentSummary {
                id: parent_id,
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                name: "parent".to_string(),
                status: AgentStatus::Idle,
                container_id: None,
                docker_image: "ghcr.io/rcore-os/tgoskits-container:latest".to_string(),
                provider_id: "openai".to_string(),
                provider_name: "OpenAI".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning_effort: Some("high".to_string()),
                created_at: timestamp,
                updated_at: timestamp,
                current_turn: None,
                last_error: None,
                token_usage: TokenUsage::default(),
            },
            None,
        )
        .await
        .expect("save parent");
    store
        .save_agent_session(
            parent_id,
            &AgentSessionSummary {
                id: Uuid::new_v4(),
                title: "Chat 1".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save session");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let parent = runtime.agent(parent_id).await.expect("parent");
    *parent.container.write().await = Some(ContainerHandle {
        id: "parent-container".to_string(),
        name: "parent-container".to_string(),
        image: "unused".to_string(),
    });

    let result = runtime
        .execute_tool_for_test(
            parent_id,
            "spawn_agent",
            json!({
                "name": "child",
                "role": "reviewer",
                "provider_id": "openai",
                "model": "gpt-5.4"
            }),
        )
        .await;
    assert!(result.expect("spawn agent").success);
    let child = runtime
        .list_agents()
        .await
        .into_iter()
        .find(|agent| agent.parent_id == Some(parent_id))
        .expect("child");
    assert_eq!(child.provider_id, "openai");
    assert_eq!(child.model, "gpt-5.4");
    assert_eq!(child.reasoning_effort, Some("high".to_string()));
    assert_eq!(
        child.docker_image,
        "ghcr.io/rcore-os/tgoskits-container:latest"
    );
    let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
    let child_record = snapshot
        .agents
        .into_iter()
        .find(|agent| agent.summary.id == child.id)
        .expect("child record");
    assert!(
        child_record
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .contains("Reviewer")
    );
}

#[tokio::test]
async fn spawn_agent_inherits_parent_and_accepts_codex_overrides() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider(), alt_test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let parent_id = Uuid::new_v4();
    let timestamp = now();
    store
        .save_agent(
            &AgentSummary {
                id: parent_id,
                parent_id: None,
                task_id: None,
                project_id: None,
                role: None,
                name: "parent".to_string(),
                status: AgentStatus::Idle,
                container_id: None,
                docker_image: "ubuntu:latest".to_string(),
                provider_id: "openai".to_string(),
                provider_name: "OpenAI".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning_effort: Some("medium".to_string()),
                created_at: timestamp,
                updated_at: timestamp,
                current_turn: None,
                last_error: None,
                token_usage: TokenUsage::default(),
            },
            None,
        )
        .await
        .expect("save parent");
    save_test_session(&store, parent_id, Uuid::new_v4()).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let parent = runtime.agent(parent_id).await.expect("parent");
    *parent.container.write().await = Some(ContainerHandle {
        id: "parent-container".to_string(),
        name: "parent-container".to_string(),
        image: "unused".to_string(),
    });

    let result = runtime
        .execute_tool_for_test(
            parent_id,
            "spawn_agent",
            json!({
                "agent_type": "worker",
                "model": "gpt-5.4",
                "reasoning_effort": "high",
                "message": "start"
            }),
        )
        .await
        .expect("spawn");
    assert!(result.success);
    let child = runtime
        .list_agents()
        .await
        .into_iter()
        .find(|agent| agent.parent_id == Some(parent_id))
        .expect("child");
    assert_eq!(child.provider_id, "openai");
    assert_eq!(child.model, "gpt-5.4");
    assert_eq!(child.reasoning_effort, Some("high".to_string()));
    assert_eq!(child.role, Some(AgentRole::Executor));
}

#[tokio::test]
async fn spawn_agent_skill_item_injects_child_initial_turn() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "child-skill",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "child done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join(".agents/skills/demo");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill.\n---\nUse child demo.",
    )
    .expect("write skill");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let parent_id = Uuid::new_v4();
    store
        .save_agent(
            &test_agent_summary(parent_id, Some("parent-container")),
            None,
        )
        .await
        .expect("save parent");
    save_test_session(&store, parent_id, Uuid::new_v4()).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let parent = runtime.agent(parent_id).await.expect("parent");
    *parent.container.write().await = Some(ContainerHandle {
        id: "parent-container".to_string(),
        name: "parent-container".to_string(),
        image: "unused".to_string(),
    });

    let result = runtime
        .execute_tool_for_test(
            parent_id,
            "spawn_agent",
            json!({
                "items": [
                    { "type": "text", "text": "child task" },
                    { "type": "skill", "name": "demo" }
                ]
            }),
        )
        .await
        .expect("spawn");
    assert!(result.success);

    wait_until(
        || {
            let requests = Arc::clone(&requests);
            async move { !requests.lock().await.is_empty() }
        },
        Duration::from_secs(2),
    )
    .await;
    let requests = requests.lock().await.clone();
    let input = requests[0]["input"].as_array().expect("input");
    assert!(input.iter().any(|item| {
        item["role"] == "user"
            && item["content"][0]["text"].as_str().is_some_and(|text| {
                text.contains("<name>demo</name>") && text.contains("Use child demo.")
            })
    }));
}

#[tokio::test]
async fn project_subagent_refreshes_and_reads_new_project_skill_resource() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    let mut child =
        test_agent_summary_with_parent(child_id, Some(maintainer_id), Some("child-container"));
    child.project_id = Some(project_id);
    child.role = Some(AgentRole::Explorer);
    save_agent_with_session(&store, &maintainer).await;
    save_agent_with_session(&store, &child).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project = runtime.project(project_id).await.expect("project");
    *project.sidecar.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "sidecar".to_string(),
        image: "unused".to_string(),
    });
    write_workspace_project_skill(
        &dir,
        project_id,
        maintainer_id,
        ".claude/skills",
        "fresh-child-skill",
        "Fresh child skill.",
        "Fresh child body.",
    );

    runtime
        .refresh_project_skills_for_agent(&runtime.agent(child_id).await.expect("child"))
        .await
        .expect("refresh");
    let result = runtime
        .execute_tool_for_test(
            child_id,
            "read_mcp_resource",
            json!({
                "server": "project-skill",
                "uri": "skill:///fresh-child-skill"
            }),
        )
        .await
        .expect("read skill");

    let output: Value = serde_json::from_str(&result.output).expect("json output");
    assert!(
        output["contents"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Fresh child body.")
    );
}

#[tokio::test]
async fn project_subagent_turn_syncs_project_skill_to_container() {
    let (base_url, _requests) = start_mock_responses(vec![json!({
        "id": "project-child-skill",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "child done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    let mut child =
        test_agent_summary_with_parent(child_id, Some(maintainer_id), Some("child-container"));
    child.project_id = Some(project_id);
    child.role = Some(AgentRole::Explorer);
    save_agent_with_session(&store, &maintainer).await;
    let session_id = Uuid::new_v4();
    store.save_agent(&child, None).await.expect("save child");
    save_test_session(&store, child_id, session_id).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    runtime.state.project_mcp_managers.write().await.insert(
        project_id,
        projects::mcp::ProjectMcpManagerHandle::without_token_expiry(Arc::new(
            McpAgentManager::from_tools_for_test(Vec::new()),
        )),
    );
    let project = runtime.project(project_id).await.expect("project");
    *project.sidecar.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "sidecar".to_string(),
        image: "unused".to_string(),
    });
    let child_record = runtime.agent(child_id).await.expect("child");
    *child_record.container.write().await = Some(ContainerHandle {
        id: "child-container".to_string(),
        name: "child".to_string(),
        image: "unused".to_string(),
    });
    write_workspace_project_skill(
        &dir,
        project_id,
        maintainer_id,
        ".claude/skills",
        "fresh-child-skill",
        "Fresh child skill.",
        "Fresh child body.",
    );

    runtime
        .run_turn_inner(
            child_id,
            session_id,
            Uuid::new_v4(),
            "Use $fresh-child-skill".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let docker_log = fake_docker_log(&dir);
    assert!(
        docker_log.contains("cp")
            && docker_log.contains("/tmp/.mai-team/skills/project/fresh-child-skill")
    );
}

#[tokio::test]
async fn project_worker_cannot_spawn_agents_and_hidden_from_tools() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let worker_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    let mut worker =
        test_agent_summary_with_parent(worker_id, Some(maintainer_id), Some("worker-container"));
    worker.project_id = Some(project_id);
    worker.role = Some(AgentRole::Executor);
    save_agent_with_session(&store, &maintainer).await;
    save_agent_with_session(&store, &worker).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let worker_record = runtime.agent(worker_id).await.expect("worker");

    let visible = turn::tools::visible_tool_names(&runtime.state, &worker_record, &[]).await;
    assert!(!visible.contains(mai_tools::TOOL_SPAWN_AGENT));
    assert!(!visible.contains(mai_tools::TOOL_CLOSE_AGENT));
    assert!(!visible.contains(mai_tools::TOOL_QUEUE_PROJECT_REVIEW_PRS));

    let result = runtime
        .execute_tool_for_test(
            worker_id,
            "spawn_agent",
            json!({
                "message": "should fail"
            }),
        )
        .await;

    assert!(
        matches!(result, Err(RuntimeError::InvalidInput(message)) if message.contains("spawn_agent"))
    );
}

#[tokio::test]
async fn project_selector_can_queue_review_prs() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let selector_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    let mut selector = test_agent_summary_with_parent(
        selector_id,
        Some(maintainer_id),
        Some("selector-container"),
    );
    selector.project_id = Some(project_id);
    selector.role = Some(AgentRole::Explorer);
    save_agent_with_session(&store, &maintainer).await;
    save_agent_with_session(&store, &selector).await;
    let project = test_project_summary(project_id, maintainer_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project_record = runtime.project(project_id).await.expect("project");
    {
        let mut summary = project_record.summary.write().await;
        summary.status = ProjectStatus::Ready;
        summary.clone_status = ProjectCloneStatus::Ready;
        summary.auto_review_enabled = true;
        summary.review_status = ProjectReviewStatus::Idle;
    }
    let selector_record = runtime.agent(selector_id).await.expect("selector");

    let visible = turn::tools::visible_tool_names(&runtime.state, &selector_record, &[]).await;
    assert!(visible.contains(mai_tools::TOOL_QUEUE_PROJECT_REVIEW_PRS));

    let result = runtime
        .execute_tool_for_test(
            selector_id,
            "queue_project_review_prs",
            json!({
                "prs": [
                    { "number": 9, "head_sha": "abc", "reason": "test" },
                    { "number": 9, "head_sha": "def", "reason": "test-duplicate" },
                    { "number": 4, "reason": "test" }
                ]
            }),
        )
        .await
        .expect("queue prs");

    assert!(result.success);
    let output: Value = serde_json::from_str(&result.output).expect("json output");
    assert_eq!(output["queued"], json!([9, 4]));
    assert_eq!(output["deduped"], json!([9]));
    assert_eq!(output["ignored"], json!([]));
    runtime.stop_project_review_loop(project_id).await;
}

#[tokio::test]
async fn relay_signal_queues_relay_pr_without_touching_review_pool() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut project = test_project_summary(project_id, maintainer_id, "account-1");
    project.auto_review_enabled = true;
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let summary = runtime
        .enqueue_project_review_relay_signal(ProjectReviewQueueRequest {
            project_id,
            pr: 9,
            head_sha: Some("head-9".to_string()),
            delivery_id: Some("delivery-9".to_string()),
            reason: "check_run".to_string(),
        })
        .await
        .expect("queue relay signal");

    assert_eq!(summary.queued, vec![9]);
    assert_eq!(summary.deduped, Vec::<u64>::new());
    assert_eq!(summary.ignored, Vec::<u64>::new());
    let project_record = runtime.project(project_id).await.expect("project");
    assert_eq!(None, project_record.review_pool.lock().await.next());
    let relay_signal = project_record
        .relay_review_queue
        .lock()
        .await
        .next()
        .expect("relay signal queued");
    assert_eq!(9, relay_signal.pr);
    assert_eq!(Some("head-9".to_string()), relay_signal.head_sha);
    assert_eq!(Some("delivery-9".to_string()), relay_signal.delivery_id);
    assert_eq!("check_run", relay_signal.reason);
}

#[tokio::test]
async fn project_review_selector_pages_and_queues_all_eligible_prs_without_model_request() {
    let mut page_one = vec![github_pr(1, false, "head-1")];
    page_one.extend((2..=20).map(|number| github_pr(number, true, &format!("head-{number}"))));
    let mut responses = vec![
        Value::Array(page_one),
        github_pr(1, false, "head-1"),
        json!([]),
        github_commit("2026-01-01T00:00:00Z"),
        json!({"check_runs": [{"status": "in_progress", "conclusion": null}]}),
        json!({"state": "success"}),
    ];
    responses.extend((2..=20).map(|number| github_pr(number, true, &format!("head-{number}"))));
    responses.extend([
        json!([
            github_pr(21, false, "head-21"),
            github_pr(22, false, "head-22")
        ]),
        github_pr(21, false, "head-21"),
        json!([]),
        github_commit("2026-01-21T00:00:00Z"),
        json!({"check_runs": [{"status": "completed", "conclusion": "failure"}]}),
        json!({"state": "failure"}),
        github_pr(22, false, "head-22"),
        json!([]),
        github_commit("2026-01-22T00:00:00Z"),
        json!({"check_runs": [{"status": "completed", "conclusion": "success"}]}),
        json!({"state": "success"}),
    ]);
    let (base_url, requests) = start_mock_responses(responses).await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            provider: GitProvider::Github,
            label: "GitHub".to_string(),
            login: Some("mai-bot".to_string()),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut project = test_project_summary(project_id, maintainer_id, "account-1");
    project.auto_review_enabled = true;
    project.review_status = ProjectReviewStatus::Idle;
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime_with_github_api(&dir, Arc::clone(&store), base_url).await;

    projects::review::selector::run_project_review_selector(
        &runtime,
        project_id,
        CancellationToken::new(),
    )
    .await
    .expect("run selector");

    let project_record = runtime.project(project_id).await.expect("project");
    let mut pool = project_record.review_pool.lock().await;
    let first = pool.next().expect("first queued pr");
    let second = pool.next().expect("second queued pr");
    assert_eq!(first.pr, 21);
    assert_eq!(first.head_sha.as_deref(), Some("head-21"));
    assert_eq!(second.pr, 22);
    assert_eq!(second.head_sha.as_deref(), Some("head-22"));
    assert_eq!(None, pool.next());
    let request_lines = requests
        .lock()
        .await
        .iter()
        .filter_map(|request| request.get("request_line").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert!(request_lines.iter().any(|line| line.contains(
        "GET /repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page=1 "
    )));
    assert!(request_lines.iter().any(|line| line.contains(
        "GET /repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=20&page=2 "
    )));
    assert!(!request_lines.iter().any(|line| line.contains("/responses")));
}

#[tokio::test]
async fn project_maintainer_can_spawn_agent() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider("http://localhost".to_string())],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    ensure_project_repo(&dir, project_id);
    let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");
    *maintainer_record.container.write().await = Some(ContainerHandle {
        id: "maintainer-container".to_string(),
        name: "maintainer-container".to_string(),
        image: "unused".to_string(),
    });

    let visible = turn::tools::visible_tool_names(&runtime.state, &maintainer_record, &[]).await;
    assert!(visible.contains(mai_tools::TOOL_SPAWN_AGENT));

    let result = runtime
        .execute_tool_for_test(
            maintainer_id,
            "spawn_agent",
            json!({
                "agent_type": "worker"
            }),
        )
        .await
        .expect("spawn");

    assert!(result.success);
}

#[tokio::test]
async fn project_agent_without_discovered_mcp_tools_has_no_static_fallback() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");

    let tools = runtime.agent_mcp_tools(&maintainer_record).await;

    assert!(tools.is_empty());
    let visible = turn::tools::visible_tool_names(&runtime.state, &maintainer_record, &tools).await;
    assert!(!visible.contains("mcp__github__create_pull_request_review"));
    assert!(!visible.contains("mcp__github__pull_request_review_write"));
    assert!(!visible.contains("mcp__git__git_status"));
}

#[tokio::test]
async fn project_agent_mcp_tools_match_project_manager_discovery() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let discovered = vec![
        test_mcp_tool("github", "pull_request_review_write"),
        test_mcp_tool("git", "git_diff_unstaged"),
    ];
    runtime.state.project_mcp_managers.write().await.insert(
        project_id,
        projects::mcp::ProjectMcpManagerHandle::without_token_expiry(Arc::new(
            McpAgentManager::from_tools_for_test(discovered.clone()),
        )),
    );
    let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");

    let tools = runtime.agent_mcp_tools(&maintainer_record).await;

    let names = tools
        .iter()
        .map(|tool| tool.model_name.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(
        names,
        HashSet::from([
            "mcp__github__pull_request_review_write",
            "mcp__git__git_diff_unstaged",
        ])
    );
    assert_eq!(tools.len(), discovered.len());
    let visible = turn::tools::visible_tool_names(&runtime.state, &maintainer_record, &tools).await;
    assert!(visible.contains("mcp__github__pull_request_review_write"));
    assert!(visible.contains("mcp__git__git_diff_unstaged"));
    assert!(!visible.contains("mcp__github__create_pull_request_review"));
    assert!(!visible.contains("mcp__git__git_status"));
}

#[tokio::test]
async fn project_reviewer_reads_project_skill_resource_without_mcp_session() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    ensure_project_repo(&dir, project_id);
    write_project_skill(
        &runtime,
        project_id,
        "review-open-prs",
        "Review open PRs.",
        "Use the review workflow.",
    );
    let reviewer = projects::review::reviewer::spawn_project_reviewer_agent(&runtime, project_id)
        .await
        .expect("spawn reviewer");

    let result = runtime
        .execute_tool_for_test(
            reviewer.id,
            "read_mcp_resource",
            json!({
                "server": "project-skill",
                "uri": "skill:///review-open-prs"
            }),
        )
        .await
        .expect("read skill resource");

    assert!(result.success);
    let output: Value = serde_json::from_str(&result.output).expect("json output");
    let text = output["contents"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("name: review-open-prs"));
    assert!(text.contains("Use the review workflow."));
}

#[tokio::test]
async fn skill_resource_can_read_bundled_relative_file() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let system_skill_dir = dir.path().join("system-skills").join("demo");
    fs::create_dir_all(system_skill_dir.join("scripts")).expect("mkdir skill");
    fs::write(
        system_skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill.\n---\nDemo body.",
    )
    .expect("write skill");
    fs::write(
        system_skill_dir.join("scripts/helper.py"),
        "print('hello from helper')\n",
    )
    .expect("write helper");
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container-1"))).await;
    let runtime = AgentRuntime::new(
        DockerClient::new_with_binary("unused", fake_docker_path(&dir)),
        ModelClient::new(),
        Arc::clone(&store),
        RuntimeConfig {
            system_skills_root: Some(dir.path().join("system-skills")),
            ..test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE)
        },
    )
    .await
    .expect("runtime");

    let result = runtime
        .execute_tool_for_test(
            agent_id,
            "read_mcp_resource",
            json!({
                "server": "skill",
                "uri": "skill:///demo/scripts/helper.py"
            }),
        )
        .await
        .expect("read helper");

    assert!(result.success);
    let output: Value = serde_json::from_str(&result.output).expect("json output");
    assert_eq!(output["contents"][0]["mimeType"], "text/x-python");
    assert!(
        output["contents"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("hello from helper")
    );
}

#[tokio::test]
async fn project_subagent_inherits_project_skill_resources() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    save_agent_with_session(&store, &maintainer).await;
    let mut child =
        test_agent_summary_with_parent(child_id, Some(maintainer_id), Some("child-container"));
    child.project_id = Some(project_id);
    child.role = Some(AgentRole::Explorer);
    save_agent_with_session(&store, &child).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    write_project_skill(
        &runtime,
        project_id,
        "review-open-prs",
        "Review open PRs.",
        "Inherited by child agents.",
    );

    let result = runtime
        .execute_tool_for_test(
            child_id,
            "read_mcp_resource",
            json!({
                "server": "project-skill",
                "uri": "skill:///review-open-prs"
            }),
        )
        .await
        .expect("child reads skill");

    let output: Value = serde_json::from_str(&result.output).expect("json output");
    assert!(
        output["contents"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Inherited by child agents.")
    );
}

#[tokio::test]
async fn project_agent_lists_project_mcp_and_skill_resources() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    write_project_skill(
        &runtime,
        project_id,
        "review-open-prs",
        "Review open PRs.",
        "Use the review workflow.",
    );
    runtime.state.project_mcp_managers.write().await.insert(
        project_id,
        projects::mcp::ProjectMcpManagerHandle::without_token_expiry(Arc::new(
            McpAgentManager::from_resources_for_test(vec![(
                "github",
                vec![json!({
                    "uri": "github://pulls",
                    "name": "pulls",
                    "mimeType": "application/json"
                })],
            )]),
        )),
    );

    let result = runtime
        .execute_tool_for_test(maintainer_id, "list_mcp_resources", json!({}))
        .await
        .expect("list resources");

    let output: Value = serde_json::from_str(&result.output).expect("json output");
    let uris = output["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter_map(|item| item.get("uri").and_then(Value::as_str))
        .collect::<HashSet<_>>();
    assert!(uris.contains("skill:///review-open-prs"));
    assert!(uris.contains("github://pulls"));
}

#[tokio::test]
async fn unknown_resource_provider_returns_clear_error() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container"))).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let err = runtime
        .execute_tool_for_test(
            agent_id,
            "read_mcp_resource",
            json!({
                "server": "missing-provider",
                "uri": "missing://resource"
            }),
        )
        .await
        .expect_err("missing provider");

    let message = err.to_string();
    assert!(message.contains("resource provider not found: missing-provider"));
    assert!(!message.contains("session not found"));
}

#[tokio::test]
async fn task_agent_reads_agent_mcp_resources() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let agent_id = Uuid::new_v4();
    save_agent_with_session(&store, &test_agent_summary(agent_id, Some("container"))).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.mcp.write().await = Some(Arc::new(McpAgentManager::from_resources_for_test(vec![(
        "agent-docs",
        vec![json!({
            "uri": "agent://docs",
            "name": "docs",
            "mimeType": "text/plain"
        })],
    )])));

    let result = runtime
        .execute_tool_for_test(
            agent_id,
            "read_mcp_resource",
            json!({
                "server": "agent-docs",
                "uri": "agent://docs"
            }),
        )
        .await
        .expect("read agent resource");

    let output: Value = serde_json::from_str(&result.output).expect("json output");
    assert_eq!(output["contents"][0]["uri"], "agent://docs");
}

#[test]
fn project_mcp_configs_use_official_defaults_without_git_token_env() {
    let configs = projects::mcp::project_mcp_configs("secret-token");
    let github = configs.get("github").expect("github");
    assert_eq!(
        github
            .env
            .get("GITHUB_PERSONAL_ACCESS_TOKEN")
            .map(String::as_str),
        Some("secret-token")
    );
    assert_eq!(
        github.env.get("GITHUB_TOOLSETS").map(String::as_str),
        Some("context,repos,issues,pull_requests")
    );
    assert!(!configs.contains_key("git"));
}

#[tokio::test]
async fn wait_agent_accepts_targets_and_send_input_queues_busy_target() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let parent_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let child_session_id = Uuid::new_v4();
    let timestamp = now();
    store
        .save_agent(&test_agent_summary_at(parent_id, None, timestamp), None)
        .await
        .expect("save parent");
    save_test_session(&store, parent_id, Uuid::new_v4()).await;
    let mut child = test_agent_summary_at(child_id, Some(parent_id), timestamp);
    child.status = AgentStatus::RunningTurn;
    child.current_turn = Some(Uuid::new_v4());
    store.save_agent(&child, None).await.expect("save child");
    store
        .save_agent_session(
            child_id,
            &AgentSessionSummary {
                id: child_session_id,
                title: "Task".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save child session");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let child_record = runtime.agent(child_id).await.expect("child");
    {
        let mut summary = child_record.summary.write().await;
        summary.status = AgentStatus::RunningTurn;
        summary.current_turn = Some(Uuid::new_v4());
    }

    let queued = runtime
        .execute_tool_for_test(
            parent_id,
            "send_input",
            json!({
                "target": child_id.to_string(),
                "items": [{ "type": "text", "text": "queued hello" }]
            }),
        )
        .await
        .expect("send input");
    assert!(queued.success);
    let value: Value = serde_json::from_str(&queued.output).expect("json");
    assert_eq!(value["queued"].as_bool(), Some(true));

    let waited = runtime
        .execute_tool_for_test(
            parent_id,
            "wait_agent",
            json!({
                "targets": [child_id.to_string()],
                "timeout_ms": 1
            }),
        )
        .await
        .expect("wait");
    let value: Value = serde_json::from_str(&waited.output).expect("json");
    assert!(value["completed"].as_array().expect("completed").is_empty());
    let pending = value["pending"].as_array().expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0]["agent_id"].as_str(),
        Some(child_id.to_string().as_str())
    );
    assert_eq!(pending[0]["status"].as_str(), Some("running_turn"));
    assert_eq!(
        pending[0]["diagnostics"]["current_turn"].as_str(),
        pending[0]["current_turn"].as_str()
    );
    assert!(pending[0]["diagnostics"]["idle_ms"].as_u64().is_some());
    assert_eq!(value["timed_out"].as_bool(), Some(true));
}

#[tokio::test]
async fn send_input_queued_skill_item_is_preserved_for_next_turn() {
    let (base_url, requests) = start_mock_responses(vec![json!({
        "id": "queued-skill",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "queued done" }]
        }],
        "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
    })])
    .await;
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join(".agents/skills/demo");
    fs::create_dir_all(&skill_dir).expect("mkdir skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill.\n---\nQueued demo body.",
    )
    .expect("write skill");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let parent_id = Uuid::new_v4();
    let child_id = Uuid::new_v4();
    let child_session_id = Uuid::new_v4();
    let timestamp = now();
    store
        .save_agent(&test_agent_summary_at(parent_id, None, timestamp), None)
        .await
        .expect("save parent");
    save_test_session(&store, parent_id, Uuid::new_v4()).await;
    let mut child = test_agent_summary_at(child_id, Some(parent_id), timestamp);
    child.status = AgentStatus::RunningTurn;
    child.current_turn = Some(Uuid::new_v4());
    child.container_id = Some("child-container".to_string());
    store.save_agent(&child, None).await.expect("save child");
    store
        .save_agent_session(
            child_id,
            &AgentSessionSummary {
                id: child_session_id,
                title: "Task".to_string(),
                created_at: timestamp,
                updated_at: timestamp,
                message_count: 0,
            },
        )
        .await
        .expect("save child session");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let child_record = runtime.agent(child_id).await.expect("child");
    {
        let mut summary = child_record.summary.write().await;
        summary.status = AgentStatus::RunningTurn;
        summary.current_turn = Some(Uuid::new_v4());
        summary.container_id = Some("child-container".to_string());
    }
    *child_record.container.write().await = Some(ContainerHandle {
        id: "child-container".to_string(),
        name: "child-container".to_string(),
        image: "unused".to_string(),
    });

    let queued = runtime
        .execute_tool_for_test(
            parent_id,
            "send_input",
            json!({
                "target": child_id.to_string(),
                "items": [
                    { "type": "text", "text": "queued hello" },
                    { "type": "skill", "name": "demo" }
                ]
            }),
        )
        .await
        .expect("send input");
    assert!(queued.success);
    {
        let mut summary = child_record.summary.write().await;
        summary.status = AgentStatus::Idle;
        summary.current_turn = None;
    }
    agents::start_next_queued_input(runtime.as_ref(), &runtime, child_id)
        .await
        .expect("start queued");

    wait_until(
        || {
            let requests = Arc::clone(&requests);
            async move { !requests.lock().await.is_empty() }
        },
        Duration::from_secs(2),
    )
    .await;
    let requests = requests.lock().await.clone();
    let input = requests[0]["input"].as_array().expect("input");
    assert!(input.iter().any(|item| {
        item["role"] == "user"
            && item["content"][0]["text"].as_str().is_some_and(|text| {
                text.contains("<name>demo</name>") && text.contains("Queued demo body.")
            })
    }));
}

#[tokio::test]
async fn create_agent_persists_and_uses_explicit_docker_image() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let runtime = AgentRuntime::new(
        DockerClient::new_with_binary("ubuntu:latest", fake_docker_path(&dir)),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let image = "ghcr.io/rcore-os/tgoskits-container:latest";
    let agent = runtime
        .create_agent(CreateAgentRequest {
            name: Some("custom-image".to_string()),
            provider_id: Some("openai".to_string()),
            model: Some("gpt-5.5".to_string()),
            reasoning_effort: None,
            docker_image: Some(format!("  {image}  ")),
            parent_id: None,
            system_prompt: None,
        })
        .await
        .expect("create agent");

    assert_eq!(agent.docker_image, image);
    assert!(fake_docker_log(&dir).contains(image));
    let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
    assert_eq!(snapshot.agents[0].summary.docker_image, image);
}

#[test]
fn project_maintainer_prompt_includes_clone_url_and_workspace() {
    let prompt = projects::service::project_maintainer_system_prompt(
        "owner",
        "repo",
        "https://github.com/owner/repo.git",
        "main",
    );

    assert!(prompt.contains("https://github.com/owner/repo.git"));
    assert!(prompt.contains("/workspace/repo"));
}

#[tokio::test]
async fn project_clone_uses_host_git_and_project_data_repo() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, None);
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    let project = test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime_with_sidecar_image_and_git(
        &dir,
        Arc::clone(&store),
        "ghcr.io/example/mai-team-sidecar:test",
    )
    .await;

    runtime
        .clone_project_repository(project_id, agent_id)
        .await
        .expect("clone");

    let git_log = fake_git_log(&dir);
    let repo_cache_path =
        projects::workspace::paths::project_repo_cache_path(&runtime.projects_root, project_id);
    let clone_path =
        projects::workspace::paths::agent_clone_path(&runtime.projects_root, project_id, agent_id);
    assert!(git_log.contains(&format!(
        "clone --mirror -- https://github.com/owner/repo.git {}",
        repo_cache_path.display()
    )));
    assert!(git_log.contains(&format!(
        "clone --local --no-checkout {} {}",
        repo_cache_path.display(),
        clone_path.display()
    )));
    assert!(git_log.contains("remote set-url origin https://github.com/owner/repo.git"));
    assert!(git_log.contains(&format!("checkout -B mai-agent/{agent_id} origin/main")));
    assert!(git_log.contains("token-present"));
    assert!(
        repo_cache_path.exists(),
        "project repo cache should be created"
    );
    assert!(
        clone_path.join(".git").exists(),
        "maintainer agent clone should be created"
    );
    let docker_log = fake_docker_log(&dir);
    let workspace_volume =
        mai_docker::project_agent_workspace_volume(&project_id.to_string(), &agent_id.to_string());
    let cache_volume = mai_docker::project_cache_volume(&project_id.to_string());
    assert!(docker_log.contains(&format!(
        "volume create --label mai.team.managed=true --label mai.team.kind=project-cache --label mai.team.project={project_id} {cache_volume}"
    )));
    assert!(docker_log.contains(&format!("{workspace_volume}:/workspace")));
    assert!(docker_log.contains("sidecar-git-clone"));
    assert!(docker_log.contains("token-present"));
    assert!(!docker_log.contains("secret-token"));
    assert!(!git_log.contains("secret-token"));
}

#[tokio::test]
async fn project_workspace_setup_moves_from_pending_to_ready() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, None);
    agent.status = AgentStatus::Created;
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    let project = test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime_with_sidecar_image_and_git(
        &dir,
        Arc::clone(&store),
        "ghcr.io/example/mai-team-sidecar:test",
    )
    .await;
    let mut events = runtime.subscribe();

    runtime
        .start_project_workspace(project_id, agent_id)
        .await
        .expect("setup");

    let detail = runtime
        .get_project(project_id, None, None)
        .await
        .expect("detail");
    assert_eq!(detail.summary.status, ProjectStatus::Ready);
    assert_eq!(detail.summary.clone_status, ProjectCloneStatus::Ready);
    assert_eq!(detail.maintainer_agent.summary.status, AgentStatus::Idle);
    let docker_log = fake_docker_log(&dir);
    let workspace_volume =
        mai_docker::project_agent_workspace_volume(&project_id.to_string(), &agent_id.to_string());
    let cache_volume = mai_docker::project_cache_volume(&project_id.to_string());
    assert!(docker_log.contains(&format!("volume create --label mai.team.managed=true --label mai.team.kind=project-cache --label mai.team.project={project_id} {cache_volume}")));
    assert!(docker_log.contains(&format!("{workspace_volume}:/workspace")));
    assert!(docker_log.contains("-w /workspace/repo"));
    assert!(!docker_log.contains(&format!(
            "{}:/workspace/repo",
            runtime
                .workspace_manager
                .agent_clone_path(project_id, agent_id)
                .display()
        )));
    let git_log = fake_git_log(&dir);
    let repo_cache_path =
        projects::workspace::paths::project_repo_cache_path(&runtime.projects_root, project_id);
    let clone_path =
        projects::workspace::paths::agent_clone_path(&runtime.projects_root, project_id, agent_id);
    assert!(git_log.contains(&format!(
        "clone --mirror -- https://github.com/owner/repo.git {}",
        repo_cache_path.display()
    )));
    assert!(git_log.contains(&format!(
        "clone --local --no-checkout {} {}",
        repo_cache_path.display(),
        clone_path.display()
    )));
    assert!(git_log.contains("token-present"));

    let mut saw_cloning = false;
    let mut saw_ready = false;
    while let Ok(event) = events.try_recv() {
        match event.kind {
            ServiceEventKind::ProjectUpdated { project }
                if project.id == project_id
                    && project.clone_status == ProjectCloneStatus::Cloning =>
            {
                saw_cloning = true;
            }
            ServiceEventKind::ProjectUpdated { project }
                if project.id == project_id
                    && project.clone_status == ProjectCloneStatus::Ready =>
            {
                saw_ready = true;
            }
            _ => {}
        }
    }
    assert!(saw_cloning);
    assert!(saw_ready);
}

#[tokio::test]
async fn runtime_start_reconciles_orphan_project_clone_dirs() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let orphan_agent_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    let project = ready_test_project_summary(project_id, maintainer_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let live_clone = ensure_project_clone(&dir, project_id, maintainer_id);
    let orphan_clone = ensure_project_clone(&dir, project_id, orphan_agent_id);

    let _runtime = test_runtime(&dir, Arc::clone(&store)).await;

    assert!(live_clone.exists());
    assert!(!orphan_clone.exists());
}

#[tokio::test]
async fn runtime_start_marks_missing_project_repo_cache_failed() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    let project = ready_test_project_summary(project_id, maintainer_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let clone_path = dir
        .path()
        .join("data/projects")
        .join(project_id.to_string())
        .join("clones")
        .join(maintainer_id.to_string())
        .join("repo");
    fs::create_dir_all(&clone_path).expect("mkdir clone");

    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let detail = runtime
        .get_project(project_id, None, None)
        .await
        .expect("project");
    assert_eq!(detail.summary.status, ProjectStatus::Failed);
    assert_eq!(detail.summary.clone_status, ProjectCloneStatus::Failed);
    assert!(
        detail
            .summary
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("repository cache"))
    );
}

#[tokio::test]
async fn runtime_start_restores_missing_project_agent_clone() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    let project = ready_test_project_summary(project_id, maintainer_id, "account-1");
    store.save_project(&project).await.expect("save project");
    ensure_project_repo(&dir, project_id);

    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let agent = runtime.get_agent(maintainer_id, None).await.expect("agent");
    assert_eq!(agent.summary.status, AgentStatus::Idle);
    assert!(
        projects::workspace::paths::agent_clone_path(
            &runtime.projects_root,
            project_id,
            maintainer_id,
        )
        .exists()
    );
}

#[tokio::test]
async fn runtime_start_starts_auto_review_worker_immediately() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    let mut project = ready_test_project_summary(project_id, agent_id, "account-1");
    project.auto_review_enabled = true;
    project.review_status = ProjectReviewStatus::Waiting;
    project.next_review_at = Some(now() + TimeDelta::minutes(30));
    store.save_project(&project).await.expect("save project");

    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project_record = runtime.project(project_id).await.expect("project");

    wait_until(
        || {
            let project_record = Arc::clone(&project_record);
            async move { project_record.review_worker.lock().await.is_some() }
        },
        Duration::from_secs(2),
    )
    .await;
    runtime.stop_project_review_loop(project_id).await;
}

#[tokio::test]
async fn runtime_start_cleans_stale_project_reviewer_before_new_worker() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    let mut reviewer = test_agent_summary_with_parent(
        reviewer_id,
        Some(maintainer_id),
        Some("reviewer-container"),
    );
    reviewer.project_id = Some(project_id);
    reviewer.role = Some(AgentRole::Reviewer);
    reviewer.status = AgentStatus::RunningTurn;
    reviewer.current_turn = Some(turn_id);
    save_agent_with_session(&store, &reviewer).await;
    let mut project = ready_test_project_summary(project_id, maintainer_id, "account-1");
    project.auto_review_enabled = true;
    project.review_status = ProjectReviewStatus::Running;
    project.current_reviewer_agent_id = Some(reviewer_id);
    store.save_project(&project).await.expect("save project");
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: Some(reviewer_id),
                turn_id: Some(turn_id),
                started_at: now(),
                finished_at: None,
                status: ProjectReviewRunStatus::Running,
                outcome: None,
                pr: None,
                summary: Some("in progress".to_string()),
                error: None,
            },
            messages: Vec::new(),
            events: Vec::new(),
        })
        .await
        .expect("save run");

    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project_record = runtime.project(project_id).await.expect("project");

    wait_until(
        || {
            let project_record = Arc::clone(&project_record);
            async move { project_record.review_worker.lock().await.is_some() }
        },
        Duration::from_secs(2),
    )
    .await;
    assert!(matches!(
        runtime.agent(reviewer_id).await,
        Err(RuntimeError::AgentNotFound(id)) if id == reviewer_id
    ));
    let run = runtime
        .get_project_review_run(project_id, run_id)
        .await
        .expect("run");
    assert_eq!(run.summary.status, ProjectReviewRunStatus::Cancelled);
    assert_eq!(
        run.summary.error.as_deref(),
        Some("review interrupted by server restart")
    );
    let project = runtime.project(project_id).await.expect("project");
    let summary = project.summary.read().await.clone();
    assert_eq!(summary.current_reviewer_agent_id, None);
    assert_eq!(summary.next_review_at, None);
    runtime.stop_project_review_loop(project_id).await;
}

#[tokio::test]
async fn runtime_start_deletes_orphan_project_reviewer() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let orphan_reviewer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    let mut reviewer = test_agent_summary_with_parent(
        orphan_reviewer_id,
        Some(maintainer_id),
        Some("orphan-reviewer-container"),
    );
    reviewer.project_id = Some(project_id);
    reviewer.role = Some(AgentRole::Reviewer);
    reviewer.status = AgentStatus::RunningTurn;
    reviewer.current_turn = Some(Uuid::new_v4());
    save_agent_with_session(&store, &reviewer).await;
    let mut project = ready_test_project_summary(project_id, maintainer_id, "account-1");
    project.auto_review_enabled = true;
    project.review_status = ProjectReviewStatus::Idle;
    store.save_project(&project).await.expect("save project");

    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    assert!(matches!(
        runtime.agent(orphan_reviewer_id).await,
        Err(RuntimeError::AgentNotFound(id)) if id == orphan_reviewer_id
    ));
    let project_record = runtime.project(project_id).await.expect("project");
    assert!(project_record.review_worker.lock().await.is_some());
    runtime.stop_project_review_loop(project_id).await;
}

#[tokio::test]
async fn runtime_start_reviewer_singleton_keeps_non_reviewer_project_agents() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let executor_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    let mut executor = test_agent_summary_with_parent(
        executor_id,
        Some(maintainer_id),
        Some("executor-container"),
    );
    executor.project_id = Some(project_id);
    executor.role = Some(AgentRole::Executor);
    executor.status = AgentStatus::RunningTurn;
    executor.current_turn = Some(Uuid::new_v4());
    save_agent_with_session(&store, &executor).await;
    let mut project = test_project_summary(project_id, maintainer_id, "account-1");
    project.auto_review_enabled = true;
    project.review_status = ProjectReviewStatus::Idle;
    store.save_project(&project).await.expect("save project");

    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    runtime.agent(maintainer_id).await.expect("maintainer");
    runtime.agent(executor_id).await.expect("executor");
    let project_record = runtime.project(project_id).await.expect("project");
    assert!(project_record.review_worker.lock().await.is_none());
}

#[tokio::test]
async fn runtime_start_does_not_start_auto_review_for_not_ready_project() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    let mut project = test_project_summary(project_id, agent_id, "account-1");
    project.auto_review_enabled = true;
    project.review_status = ProjectReviewStatus::Idle;
    store.save_project(&project).await.expect("save project");

    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let project_record = runtime.project(project_id).await.expect("project");

    assert!(project_record.review_worker.lock().await.is_none());
}

#[tokio::test]
async fn project_reviewer_starts_from_image_with_own_project_clone() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    maintainer.docker_image = "ghcr.io/rcore-os/tgoskits-container:latest".to_string();
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    ensure_project_repo(&dir, project_id);

    let reviewer = projects::review::reviewer::spawn_project_reviewer_agent(&runtime, project_id)
        .await
        .expect("spawn reviewer");

    assert_eq!(reviewer.role, Some(AgentRole::Reviewer));
    assert_eq!(reviewer.parent_id, Some(maintainer_id));
    let docker_log = fake_docker_log(&dir);
    let workspace_volume = mai_docker::project_agent_workspace_volume(
        &project_id.to_string(),
        &reviewer.id.to_string(),
    );
    let clone_path = runtime
        .workspace_manager
        .agent_clone_path(project_id, reviewer.id);
    assert!(!docker_log.contains("commit maintainer-container"));
    assert!(docker_log.contains(&format!("create --name mai-team-{}", reviewer.id)));
    assert!(docker_log.contains(&format!("{workspace_volume}:/workspace")));
    assert!(!docker_log.contains(&format!("mai-team-workspace-{}:/workspace", reviewer.id)));
    assert!(!docker_log.contains(&format!("{}:/workspace/repo", clone_path.display())));
    assert!(docker_log.contains("--user root"));
    assert!(!docker_log.contains("/workspace/reviews"));
}

#[tokio::test]
async fn deleting_project_reviewer_cleans_project_clone() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    ensure_project_repo(&dir, project_id);
    let reviewer = projects::review::reviewer::spawn_project_reviewer_agent(&runtime, project_id)
        .await
        .expect("spawn reviewer");
    let reviewer_id = reviewer.id;
    let clone_path = runtime
        .workspace_manager
        .agent_clone_path(project_id, reviewer_id);
    std::fs::create_dir_all(&clone_path).expect("reviewer clone");
    let workspace_volume = mai_docker::project_agent_workspace_volume(
        &project_id.to_string(),
        &reviewer_id.to_string(),
    );

    runtime
        .delete_agent(reviewer_id)
        .await
        .expect("delete reviewer");

    let docker_log = fake_docker_log(&dir);
    assert!(docker_log.contains("rm -f created-container"));
    assert!(docker_log.contains(&format!("volume rm -f {workspace_volume}")));
    assert!(!clone_path.exists());
}

#[tokio::test]
async fn project_reviewer_initial_message_uses_latest_extra_prompt() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    let mut project = ready_test_project_summary(project_id, agent_id, "account-1");
    project.reviewer_extra_prompt = Some("old prompt".to_string());
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    runtime
        .update_project(
            project_id,
            UpdateProjectRequest {
                reviewer_extra_prompt: Some("new prompt".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("update project");
    let message = projects::review::reviewer::project_reviewer_initial_message(
        &runtime,
        project_id,
        reviewer_id,
        None,
    )
    .await
    .expect("message");

    assert!(message.contains("new prompt"));
    assert!(!message.contains("old prompt"));
    assert!(message.contains("Target pull request: none."));
    assert!(!message.contains("worktree"));
    assert!(!message.contains("/workspace/reviews"));
}

#[tokio::test]
async fn project_reviewer_initial_message_can_target_pr() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut agent = test_agent_summary(reviewer_id, None);
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Reviewer);
    save_agent_with_session(&store, &agent).await;
    let project = ready_test_project_summary(project_id, reviewer_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime(&dir, store).await;

    let message = projects::review::reviewer::project_reviewer_initial_message(
        &runtime,
        project_id,
        reviewer_id,
        Some(42),
    )
    .await
    .expect("message");

    assert!(message.contains("review PR #42 only"));
    assert!(message.contains("select-pr --target-pr 42"));
}

#[tokio::test]
async fn auto_review_refreshes_project_skills_from_synced_default_branch() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = AgentRuntime::new(
        DockerClient::new_with_binary("unused-agent", fake_docker_path(&dir)),
        ModelClient::new(),
        store,
        RuntimeConfig {
            git_binary: Some(fake_git_path(&dir)),
            ..test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE)
        },
    )
    .await
    .expect("runtime");
    write_project_skill(
        &runtime,
        project_id,
        "review-default-branch",
        "Old review skill.",
        "Old review body.",
    );

    runtime
        .sync_project_cache_repo(project_id)
        .await
        .expect("sync review repo");

    write_workspace_project_skill(
        &dir,
        project_id,
        maintainer_id,
        ".claude/skills",
        "review-default-branch",
        "New review skill.",
        "New review body.",
    );
    runtime
        .refresh_project_skills_from_agent_workspace(project_id)
        .await
        .expect("refresh review skills");

    let response = runtime
        .project_skills_from_cache(project_id)
        .await
        .expect("skills");
    let skill = response
        .skills
        .iter()
        .find(|skill| skill.name == "review-default-branch")
        .expect("review skill");
    assert_eq!(skill.description, "New review skill.");
    assert_eq!(
        fs::read_to_string(&skill.path).expect("skill body"),
        "---\nname: review-default-branch\ndescription: New review skill.\n---\nNew review body."
    );
}

#[tokio::test]
async fn project_reviewer_instructions_include_extra_prompt_project_skill() {
    let (base_url, requests) = start_mock_responses(vec![json!({
            "id": "review-skill",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "{\"outcome\":\"no_eligible_pr\",\"pr\":null,\"summary\":\"No eligible pull request found.\",\"error\":null}" }]
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        })])
        .await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![compact_test_provider(base_url)],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let project = ProjectSummary {
        reviewer_extra_prompt: Some("用中文评论, review-single-pr".to_string()),
        ..ready_test_project_summary(project_id, reviewer_id, "account-1")
    };
    store.save_project(&project).await.expect("save project");
    let mut reviewer = test_agent_summary(reviewer_id, Some("container-1"));
    reviewer.project_id = Some(project_id);
    reviewer.role = Some(AgentRole::Reviewer);
    let session_id = Uuid::new_v4();
    store.save_agent(&reviewer, None).await.expect("save agent");
    save_test_session(&store, reviewer_id, session_id).await;
    let system_skill_dir = dir
        .path()
        .join("system-skills")
        .join("reviewer-agent-review-pr");
    fs::create_dir_all(&system_skill_dir).expect("mkdir system skill");
    fs::write(
            system_skill_dir.join("SKILL.md"),
            "---\nname: reviewer-agent-review-pr\ndescription: Review one PR.\n---\nReviewer system body.",
        )
        .expect("write system skill");
    let runtime = AgentRuntime::new(
        DockerClient::new_with_binary("unused", fake_docker_path(&dir)),
        ModelClient::new(),
        Arc::clone(&store),
        RuntimeConfig {
            system_skills_root: Some(dir.path().join("system-skills")),
            ..test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE)
        },
    )
    .await
    .expect("runtime");
    let agent = runtime.agent(reviewer_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });
    runtime.state.project_mcp_managers.write().await.insert(
        project_id,
        projects::mcp::ProjectMcpManagerHandle::without_token_expiry(Arc::new(
            McpAgentManager::from_tools_for_test(Vec::new()),
        )),
    );
    write_project_skill(
        &runtime,
        project_id,
        "review-single-pr",
        "Review exactly one pull request with Chinese comments.",
        "Review single PR body.",
    );
    write_workspace_project_skill(
        &dir,
        project_id,
        reviewer_id,
        ".claude/skills",
        "review-single-pr",
        "Review exactly one pull request with Chinese comments.",
        "Review single PR body.",
    );
    let message = projects::review::reviewer::project_reviewer_initial_message(
        &runtime,
        project_id,
        reviewer_id,
        None,
    )
    .await
    .expect("message");
    runtime
        .run_turn_inner(
            reviewer_id,
            session_id,
            Uuid::new_v4(),
            message,
            vec!["reviewer-agent-review-pr".to_string()],
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = requests.lock().await.clone();
    let request_text = serde_json::to_string(&requests[0]).expect("request json");
    assert!(request_text.contains("用中文评论, review-single-pr"));
    assert!(request_text.contains("$review-single-pr"));
    assert!(request_text.contains("Review exactly one pull request with Chinese comments."));
    assert!(request_text.contains("/tmp/.mai-team/skills/project/review-single-pr/SKILL.md"));
    assert!(
        request_text.contains("/tmp/.mai-team/skills/system/reviewer-agent-review-pr/SKILL.md")
    );
    assert!(!request_text.contains("/workspace/repo/.claude/skills/review-single-pr/SKILL.md"));
    assert!(request_text.contains("<name>reviewer-agent-review-pr</name>"));
    assert!(request_text.contains("<name>review-single-pr</name>"));
    let docker_log = fake_docker_log(&dir);
    assert!(
        docker_log.contains("cp")
            && docker_log.contains("/tmp/.mai-team/skills/system/reviewer-agent-review-pr")
    );
    assert!(
        docker_log.contains("cp")
            && docker_log.contains("/tmp/.mai-team/skills/project/review-single-pr")
    );
}

#[tokio::test]
async fn project_detail_includes_recent_review_runs() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, None);
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            agent_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let run_id = Uuid::new_v4();
    let started_at = now();
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: Some(Uuid::new_v4()),
                turn_id: Some(Uuid::new_v4()),
                started_at,
                finished_at: Some(started_at + TimeDelta::minutes(1)),
                status: ProjectReviewRunStatus::Completed,
                outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
                pr: Some(7),
                summary: Some("submitted review".to_string()),
                error: None,
            },
            messages: vec![AgentMessage {
                role: MessageRole::Assistant,
                content: "review complete".to_string(),
                created_at: started_at,
            }],
            events: Vec::new(),
        })
        .await
        .expect("save run");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let detail = runtime
        .get_project(project_id, None, None)
        .await
        .expect("detail");
    assert_eq!(detail.review_runs.len(), 1);
    assert_eq!(detail.review_runs[0].id, run_id);
    assert_eq!(detail.review_runs[0].pr, Some(7));
    let run = runtime
        .get_project_review_run(project_id, run_id)
        .await
        .expect("run detail");
    assert_eq!(run.messages[0].content, "review complete");
}

#[tokio::test]
async fn finishing_project_review_run_captures_reviewer_snapshot() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, None);
    maintainer.project_id = Some(project_id);
    maintainer.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &maintainer).await;
    let mut reviewer = test_agent_summary(reviewer_id, None);
    reviewer.project_id = Some(project_id);
    reviewer.parent_id = Some(maintainer_id);
    reviewer.role = Some(AgentRole::Reviewer);
    reviewer.status = AgentStatus::Completed;
    store
        .save_agent(&reviewer, None)
        .await
        .expect("save reviewer");
    store
        .save_agent_session(
            reviewer_id,
            &AgentSessionSummary {
                id: session_id,
                title: "Review".to_string(),
                created_at: now(),
                updated_at: now(),
                message_count: 0,
            },
        )
        .await
        .expect("save reviewer session");
    store
        .append_agent_message(
            reviewer_id,
            session_id,
            0,
            &AgentMessage {
                role: MessageRole::Assistant,
                content: "snapshot summary".to_string(),
                created_at: now(),
            },
        )
        .await
        .expect("message");
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    store
        .save_project_review_run(&ProjectReviewRunDetail {
            summary: ProjectReviewRunSummary {
                id: run_id,
                project_id,
                reviewer_agent_id: Some(reviewer_id),
                turn_id: Some(turn_id),
                started_at: now(),
                finished_at: None,
                status: ProjectReviewRunStatus::Running,
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
    runtime
        .events
        .publish(ServiceEventKind::TurnCompleted {
            agent_id: reviewer_id,
            session_id: Some(session_id),
            turn_id,
            status: TurnStatus::Completed,
        })
        .await;

    projects::review::runs::finish_project_review_run(
        &runtime.deps.store,
        runtime.as_ref(),
        FinishReviewRun {
            run_id,
            project_id,
            reviewer_agent_id: Some(reviewer_id),
            turn_id: Some(turn_id),
            status: ProjectReviewRunStatus::Completed,
            outcome: Some(ProjectReviewOutcome::ReviewSubmitted),
            pr: Some(12),
            summary_text: Some("submitted".to_string()),
            error: None,
        },
    )
    .await
    .expect("finish");

    let run = runtime
        .get_project_review_run(project_id, run_id)
        .await
        .expect("run");
    assert_eq!(run.summary.pr, Some(12));
    assert_eq!(run.messages[0].content, "snapshot summary");
    assert!(run.events.iter().any(|event| {
        matches!(
            event.kind,
            ServiceEventKind::TurnCompleted { agent_id, .. } if agent_id == reviewer_id
        )
    }));
}

#[tokio::test]
async fn project_review_retention_cleanup_does_not_touch_workspace_volume() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, None);
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            agent_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    projects::review::cleanup::cleanup_project_review_history(&runtime)
        .await
        .expect("cleanup");

    let docker_log = fake_docker_log(&dir);
    assert!(!docker_log.contains("git -C /workspace/repo worktree prune"));
    assert!(!docker_log.contains("/workspace/reviews"));
    assert!(!docker_log.contains("rm -rf /workspace/repo"));
}

#[tokio::test]
async fn delete_project_removes_project_mcp_manager() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
    agent.project_id = Some(project_id);
    save_agent_with_session(&store, &agent).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            agent_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    runtime.state.project_mcp_managers.write().await.insert(
        project_id,
        projects::mcp::ProjectMcpManagerHandle::without_token_expiry(Arc::new(
            McpAgentManager::from_tools_for_test(vec![test_mcp_tool("github", "get_me")]),
        )),
    );

    runtime.delete_project(project_id).await.expect("delete");

    assert!(
        !runtime
            .state
            .project_mcp_managers
            .read()
            .await
            .contains_key(&project_id)
    );
}

#[tokio::test]
async fn project_workspace_setup_failure_marks_project_failed() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, None);
    agent.status = AgentStatus::Created;
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    let project = test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = AgentRuntime::new(
        DockerClient::new_with_binary("unused-agent", fake_docker_path(&dir)),
        ModelClient::new(),
        Arc::clone(&store),
        RuntimeConfig {
            git_binary: Some(failing_git_path(&dir)),
            ..test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE)
        },
    )
    .await
    .expect("runtime");

    runtime
        .start_project_workspace(project_id, agent_id)
        .await
        .expect("setup handles failure");

    let detail = runtime
        .get_project(project_id, None, None)
        .await
        .expect("detail");
    assert_eq!(detail.summary.status, ProjectStatus::Failed);
    assert_eq!(detail.summary.clone_status, ProjectCloneStatus::Failed);
    assert_ne!(detail.maintainer_agent.summary.status, AgentStatus::Failed);
    assert!(
        detail
            .summary
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("git clone failed")
    );
    assert!(
        !detail
            .summary
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("secret-token")
    );
    assert!(fake_git_log(&dir).contains("token-present"));
}

#[tokio::test]
async fn project_sidecar_is_removed_when_project_is_deleted() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let agent_container_id = format!("mai-team-{agent_id}");
    let mut agent = test_agent_summary(agent_id, Some(&agent_container_id));
    agent.project_id = Some(project_id);
    agent.role = Some(AgentRole::Planner);
    save_agent_with_session(&store, &agent).await;
    let project = test_project_summary(project_id, agent_id, "account-1");
    store.save_project(&project).await.expect("save project");
    let runtime = test_runtime_with_sidecar_image_and_git(
        &dir,
        Arc::clone(&store),
        "ghcr.io/example/mai-team-sidecar:test",
    )
    .await;

    runtime
        .clone_project_repository(project_id, agent_id)
        .await
        .expect("clone");
    let project = runtime.project(project_id).await.expect("project");
    *project.sidecar.write().await = Some(ContainerHandle {
        id: "created-container".to_string(),
        name: "sidecar".to_string(),
        image: "unused".to_string(),
    });
    runtime
        .delete_project(project_id)
        .await
        .expect("delete project");

    let docker_log = fake_docker_log(&dir);
    assert!(docker_log.contains("rm -f created-container"));
    assert!(docker_log.contains(&format!("rm -f mai-team-{agent_id}")));
    assert!(!runtime.projects_root.join(project_id.to_string()).exists());
}

#[tokio::test]
async fn update_agent_changes_model_persists_and_publishes() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let timestamp = now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "model-switch".to_string(),
        status: AgentStatus::Idle,
        container_id: None,
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.5".to_string(),
        reasoning_effort: Some("low".to_string()),
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    store.save_agent(&summary, None).await.expect("save agent");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");
    let mut events = runtime.subscribe();

    let updated = runtime
        .update_agent(
            agent_id,
            UpdateAgentRequest {
                provider_id: None,
                model: Some("gpt-5.4".to_string()),
                reasoning_effort: Some("high".to_string()),
            },
        )
        .await
        .expect("update");

    assert_eq!(updated.model, "gpt-5.4");
    assert_eq!(updated.reasoning_effort, Some("high".to_string()));
    let event = events.recv().await.expect("event");
    assert!(matches!(
        event.kind,
        ServiceEventKind::AgentUpdated { agent } if agent.id == agent_id
            && agent.model == "gpt-5.4"
            && agent.reasoning_effort == Some("high".to_string())
    ));
    let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
    assert_eq!(snapshot.agents[0].summary.model, "gpt-5.4");
    assert_eq!(
        snapshot.agents[0].summary.reasoning_effort,
        Some("high".to_string())
    );
}

#[tokio::test]
async fn update_agent_rejects_invalid_reasoning_and_clears_unsupported_model() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let timestamp = now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "reasoning-switch".to_string(),
        status: AgentStatus::Idle,
        container_id: None,
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.5".to_string(),
        reasoning_effort: Some("medium".to_string()),
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    store.save_agent(&summary, None).await.expect("save agent");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let invalid = runtime
        .update_agent(
            agent_id,
            UpdateAgentRequest {
                provider_id: None,
                model: Some("gpt-5.4".to_string()),
                reasoning_effort: Some("max".to_string()),
            },
        )
        .await;
    assert!(matches!(invalid, Err(RuntimeError::InvalidInput(_))));

    let updated = runtime
        .update_agent(
            agent_id,
            UpdateAgentRequest {
                provider_id: None,
                model: Some("gpt-4.1".to_string()),
                reasoning_effort: Some("high".to_string()),
            },
        )
        .await
        .expect("clear unsupported");
    assert_eq!(updated.model, "gpt-4.1");
    assert_eq!(updated.reasoning_effort, None);
}

#[tokio::test]
async fn update_agent_rejects_busy_and_unknown_model() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let timestamp = now();
    let summary = AgentSummary {
        id: agent_id,
        parent_id: None,
        task_id: None,
        project_id: None,
        role: None,
        name: "busy".to_string(),
        status: AgentStatus::Idle,
        container_id: None,
        docker_image: "ubuntu:latest".to_string(),
        provider_id: "openai".to_string(),
        provider_name: "OpenAI".to_string(),
        model: "gpt-5.5".to_string(),
        reasoning_effort: Some("medium".to_string()),
        created_at: timestamp,
        updated_at: timestamp,
        current_turn: None,
        last_error: None,
        token_usage: TokenUsage::default(),
    };
    store.save_agent(&summary, None).await.expect("save agent");
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        ModelClient::new(),
        store,
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");

    let unknown = runtime
        .update_agent(
            agent_id,
            UpdateAgentRequest {
                provider_id: None,
                model: Some("missing".to_string()),
                reasoning_effort: None,
            },
        )
        .await;
    assert!(matches!(unknown, Err(RuntimeError::Store(_))));

    let agent = runtime.agent(agent_id).await.expect("agent");
    {
        let mut summary = agent.summary.write().await;
        summary.status = AgentStatus::RunningTurn;
        summary.current_turn = Some(Uuid::new_v4());
    }
    let busy = runtime
        .update_agent(
            agent_id,
            UpdateAgentRequest {
                provider_id: None,
                model: Some("gpt-5.4".to_string()),
                reasoning_effort: None,
            },
        )
        .await;
    assert!(matches!(busy, Err(RuntimeError::AgentBusy(id)) if id == agent_id));
}

#[test]
fn tool_event_preview_redacts_sensitive_and_large_values() {
    let value = json!({
        "command": "echo ok",
        "api_key": "secret",
        "content_base64": "a".repeat(320),
    });

    let preview = turn::tools::trace_preview_value(&value, 1_000);

    assert!(preview.contains("echo ok"));
    assert!(preview.contains("<redacted>"));
    assert!(!preview.contains("secret"));
    assert!(!preview.contains(&"a".repeat(120)));
}
