use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use mai_docker::{ContainerHandle, DockerClient};
use mai_mcp::McpAgentManager;
use mai_model::ResponsesClient;
use mai_protocol::{
    AgentDetail, AgentId, AgentMessage, AgentStatus, AgentSummary, CreateAgentRequest, MessageRole,
    ModelInputItem, ModelOutputItem, ServiceEvent, ServiceEventKind, TokenUsage, TurnId,
    TurnStatus, now, preview,
};
use mai_skills::SkillsManager;
use mai_store::ConfigStore;
use mai_tools::{RoutedTool, build_tool_definitions, route_tool};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::time::{Duration, Instant, sleep};
use uuid::Uuid;

const MAX_TOOL_ITERATIONS: usize = 16;
const RECENT_EVENT_LIMIT: usize = 500;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("agent not found: {0}")]
    AgentNotFound(AgentId),
    #[error("agent is busy: {0}")]
    AgentBusy(AgentId),
    #[error("agent has no container: {0}")]
    MissingContainer(AgentId),
    #[error("docker error: {0}")]
    Docker(#[from] mai_docker::DockerError),
    #[error("model error: {0}")]
    Model(#[from] mai_model::ModelError),
    #[error("mcp error: {0}")]
    Mcp(#[from] mai_mcp::McpError),
    #[error("store error: {0}")]
    Store(#[from] mai_store::StoreError),
    #[error("skill error: {0}")]
    Skill(#[from] mai_skills::SkillError),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Clone)]
pub struct RuntimeConfig {
    pub repo_root: PathBuf,
}

pub struct AgentRuntime {
    docker: DockerClient,
    model: ResponsesClient,
    store: Arc<ConfigStore>,
    skills: SkillsManager,
    agents: RwLock<HashMap<AgentId, Arc<AgentRecord>>>,
    event_tx: broadcast::Sender<ServiceEvent>,
    sequence: AtomicU64,
    recent_events: Mutex<VecDeque<ServiceEvent>>,
}

struct AgentRecord {
    summary: RwLock<AgentSummary>,
    messages: Mutex<Vec<AgentMessage>>,
    history: Mutex<Vec<ModelInputItem>>,
    container: RwLock<Option<ContainerHandle>>,
    mcp: RwLock<Option<Arc<McpAgentManager>>>,
    system_prompt: Option<String>,
    turn_lock: Mutex<()>,
    cancel_requested: AtomicBool,
}

#[derive(Debug)]
struct ToolExecution {
    success: bool,
    output: String,
}

impl AgentRuntime {
    pub fn new(
        docker: DockerClient,
        model: ResponsesClient,
        store: Arc<ConfigStore>,
        config: RuntimeConfig,
    ) -> Result<Arc<Self>> {
        let skills = SkillsManager::new(&config.repo_root);
        let (event_tx, _) = broadcast::channel(1024);
        Ok(Arc::new(Self {
            docker,
            model,
            store,
            skills,
            agents: RwLock::new(HashMap::new()),
            event_tx,
            sequence: AtomicU64::new(1),
            recent_events: Mutex::new(VecDeque::with_capacity(RECENT_EVENT_LIMIT)),
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.event_tx.subscribe()
    }

    pub async fn create_agent(
        self: &Arc<Self>,
        request: CreateAgentRequest,
    ) -> Result<AgentSummary> {
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let name = request
            .name
            .unwrap_or_else(|| format!("agent-{}", short_id(id)));
        let provider_selection = self
            .store
            .resolve_provider(request.provider_id.as_deref(), request.model.as_deref())?;
        let summary = AgentSummary {
            id,
            parent_id: request.parent_id,
            name,
            status: AgentStatus::Created,
            container_id: None,
            provider_id: provider_selection.provider.id.clone(),
            provider_name: provider_selection.provider.name.clone(),
            model: provider_selection.model.clone(),
            created_at,
            updated_at: created_at,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };

        let agent = Arc::new(AgentRecord {
            summary: RwLock::new(summary.clone()),
            messages: Mutex::new(Vec::new()),
            history: Mutex::new(Vec::new()),
            container: RwLock::new(None),
            mcp: RwLock::new(None),
            system_prompt: request.system_prompt,
            turn_lock: Mutex::new(()),
            cancel_requested: AtomicBool::new(false),
        });

        self.agents.write().await.insert(id, Arc::clone(&agent));
        self.publish(ServiceEventKind::AgentCreated {
            agent: summary.clone(),
        })
        .await;
        self.set_status(&agent, AgentStatus::StartingContainer, None)
            .await;

        match self.docker.create_agent_container(&id.to_string()).await {
            Ok(container) => {
                {
                    let mut summary = agent.summary.write().await;
                    summary.container_id = Some(container.id.clone());
                    summary.updated_at = now();
                }
                *agent.container.write().await = Some(container.clone());
                let mcp = McpAgentManager::start(
                    self.docker.clone(),
                    container.id,
                    self.store.list_mcp_servers()?,
                )
                .await;
                *agent.mcp.write().await = Some(Arc::new(mcp));
                self.set_status(&agent, AgentStatus::Idle, None).await;
                Ok(agent.summary.read().await.clone())
            }
            Err(err) => {
                let message = err.to_string();
                self.set_status(&agent, AgentStatus::Failed, Some(message.clone()))
                    .await;
                self.publish(ServiceEventKind::Error {
                    agent_id: Some(id),
                    turn_id: None,
                    message,
                })
                .await;
                Err(err.into())
            }
        }
    }

    pub async fn list_agents(&self) -> Vec<AgentSummary> {
        let agents = self.agents.read().await;
        let mut summaries = Vec::with_capacity(agents.len());
        for agent in agents.values() {
            summaries.push(agent.summary.read().await.clone());
        }
        summaries.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        summaries
    }

    pub async fn get_agent(&self, agent_id: AgentId) -> Result<AgentDetail> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let messages = agent.messages.lock().await.clone();
        let recent_events = self
            .recent_events
            .lock()
            .await
            .iter()
            .filter(|event| event_agent_id(event) == Some(agent_id))
            .cloned()
            .collect();
        Ok(AgentDetail {
            summary,
            messages,
            recent_events,
        })
    }

    pub async fn send_message(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let turn_id = self.prepare_turn(agent_id).await?;
        self.spawn_turn(agent_id, turn_id, message, skill_mentions);
        Ok(turn_id)
    }

    async fn prepare_turn(&self, agent_id: AgentId) -> Result<TurnId> {
        let agent = self.agent(agent_id).await?;
        let turn_id = Uuid::new_v4();
        let should_start = {
            let mut summary = agent.summary.write().await;
            if !summary.status.can_start_turn() {
                false
            } else {
                summary.status = AgentStatus::RunningTurn;
                summary.current_turn = Some(turn_id);
                summary.updated_at = now();
                summary.last_error = None;
                agent.cancel_requested.store(false, Ordering::SeqCst);
                true
            }
        };
        if !should_start {
            return Err(RuntimeError::AgentBusy(agent_id));
        }
        self.publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: AgentStatus::RunningTurn,
        })
        .await;
        Ok(turn_id)
    }

    fn spawn_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            runtime
                .run_turn(agent_id, turn_id, message, skill_mentions)
                .await;
        });
    }

    pub async fn cancel_agent(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        agent.cancel_requested.store(true, Ordering::SeqCst);
        self.set_status(&agent, AgentStatus::Cancelled, None).await;
        Ok(())
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        self.set_status(&agent, AgentStatus::DeletingContainer, None)
            .await;
        if let Some(container) = agent.container.write().await.take() {
            self.docker.delete_container(&container.id).await?;
        }
        self.set_status(&agent, AgentStatus::Deleted, None).await;
        self.agents.write().await.remove(&agent_id);
        self.publish(ServiceEventKind::AgentDeleted { agent_id })
            .await;
        Ok(())
    }

    pub async fn upload_file(
        &self,
        agent_id: AgentId,
        path: String,
        content_base64: String,
    ) -> Result<usize> {
        let bytes = BASE64
            .decode(content_base64.trim())
            .map_err(|err| RuntimeError::InvalidInput(format!("invalid base64: {err}")))?;
        let temp = NamedTempFile::new()?;
        std::fs::write(temp.path(), &bytes)?;
        let container_id = self.container_id(agent_id).await?;
        self.docker
            .copy_to_container(&container_id, temp.path(), &path)
            .await?;
        Ok(bytes.len())
    }

    pub async fn download_file_tar(&self, agent_id: AgentId, path: String) -> Result<Vec<u8>> {
        let container_id = self.container_id(agent_id).await?;
        Ok(self
            .docker
            .copy_from_container_tar(&container_id, &path)
            .await?)
    }

    async fn run_turn(
        self: Arc<Self>,
        agent_id: AgentId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        let result = self
            .run_turn_inner(agent_id, turn_id, message, skill_mentions)
            .await;
        if let Err(err) = result {
            if let Ok(agent) = self.agent(agent_id).await {
                {
                    let mut summary = agent.summary.write().await;
                    summary.status = AgentStatus::Failed;
                    summary.current_turn = None;
                    summary.updated_at = now();
                    summary.last_error = Some(err.to_string());
                }
                self.publish(ServiceEventKind::Error {
                    agent_id: Some(agent_id),
                    turn_id: Some(turn_id),
                    message: err.to_string(),
                })
                .await;
                self.publish(ServiceEventKind::TurnCompleted {
                    agent_id,
                    turn_id,
                    status: TurnStatus::Failed,
                })
                .await;
                self.publish(ServiceEventKind::AgentStatusChanged {
                    agent_id,
                    status: AgentStatus::Failed,
                })
                .await;
            }
        }
    }

    async fn run_turn_inner(
        self: &Arc<Self>,
        agent_id: AgentId,
        turn_id: TurnId,
        message: String,
        mut skill_mentions: Vec<String>,
    ) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let _turn_guard = agent.turn_lock.lock().await;
        self.publish(ServiceEventKind::TurnStarted { agent_id, turn_id })
            .await;

        skill_mentions.extend(extract_skill_mentions(&message));
        self.record_message(&agent, MessageRole::User, message.clone())
            .await;
        agent
            .history
            .lock()
            .await
            .push(ModelInputItem::user_text(message.clone()));
        self.publish(ServiceEventKind::AgentMessage {
            agent_id,
            turn_id: Some(turn_id),
            role: MessageRole::User,
            content: message,
        })
        .await;

        let loaded_skills = self.skills.load_explicit(&skill_mentions)?;
        for iteration in 0..MAX_TOOL_ITERATIONS {
            if agent.cancel_requested.load(Ordering::SeqCst) {
                self.finish_turn(
                    &agent,
                    agent_id,
                    turn_id,
                    TurnStatus::Cancelled,
                    AgentStatus::Cancelled,
                )
                .await;
                return Ok(());
            }

            self.set_status(&agent, AgentStatus::RunningTurn, None)
                .await;
            let mcp_tools = self.agent_mcp_tools(&agent).await;
            let tools = build_tool_definitions(&mcp_tools);
            let instructions = self
                .build_instructions(&agent, &loaded_skills, &mcp_tools)
                .await?;
            let model_name = agent.summary.read().await.model.clone();
            let provider_id = agent.summary.read().await.provider_id.clone();
            let provider_selection = self
                .store
                .resolve_provider(Some(&provider_id), Some(&model_name))?;
            let history = agent.history.lock().await.clone();
            let response = self
                .model
                .create_response(
                    &provider_selection.provider.base_url,
                    &provider_selection.provider.api_key,
                    &provider_selection.model,
                    &instructions,
                    &history,
                    &tools,
                )
                .await?;

            if let Some(usage) = response.usage {
                let mut summary = agent.summary.write().await;
                summary.token_usage.add(&usage);
                summary.updated_at = now();
            }

            let mut tool_calls = Vec::new();
            for item in response.output {
                match item {
                    ModelOutputItem::Message { text } => {
                        if !text.trim().is_empty() {
                            self.record_message(&agent, MessageRole::Assistant, text.clone())
                                .await;
                            agent
                                .history
                                .lock()
                                .await
                                .push(ModelInputItem::assistant_text(text.clone()));
                            self.publish(ServiceEventKind::AgentMessage {
                                agent_id,
                                turn_id: Some(turn_id),
                                role: MessageRole::Assistant,
                                content: text,
                            })
                            .await;
                        }
                    }
                    ModelOutputItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                        raw_arguments,
                    } => {
                        let call_id = if call_id.is_empty() {
                            format!("call_{}", Uuid::new_v4())
                        } else {
                            call_id
                        };
                        agent
                            .history
                            .lock()
                            .await
                            .push(ModelInputItem::FunctionCall {
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: raw_arguments,
                            });
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelOutputItem::Other { .. } => {}
                }
            }

            if tool_calls.is_empty() {
                self.finish_turn(
                    &agent,
                    agent_id,
                    turn_id,
                    TurnStatus::Completed,
                    AgentStatus::Completed,
                )
                .await;
                return Ok(());
            }

            self.set_status(&agent, AgentStatus::WaitingTool, None)
                .await;
            for (call_id, name, arguments) in tool_calls {
                self.publish(ServiceEventKind::ToolStarted {
                    agent_id,
                    turn_id,
                    call_id: call_id.clone(),
                    tool_name: name.clone(),
                })
                .await;
                let output = self
                    .execute_tool(&agent, agent_id, turn_id, &name, arguments)
                    .await;
                let execution = match output {
                    Ok(execution) => execution,
                    Err(err) => ToolExecution {
                        success: false,
                        output: err.to_string(),
                    },
                };
                agent
                    .history
                    .lock()
                    .await
                    .push(ModelInputItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: execution.output.clone(),
                    });
                self.publish(ServiceEventKind::ToolCompleted {
                    agent_id,
                    turn_id,
                    call_id,
                    tool_name: name,
                    success: execution.success,
                    output_preview: preview(&execution.output, 500),
                })
                .await;
            }

            if iteration + 1 == MAX_TOOL_ITERATIONS {
                return Err(RuntimeError::InvalidInput(format!(
                    "tool iteration limit reached ({MAX_TOOL_ITERATIONS})"
                )));
            }
        }

        Ok(())
    }

    async fn execute_tool(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        _turn_id: TurnId,
        name: &str,
        arguments: Value,
    ) -> Result<ToolExecution> {
        match route_tool(name) {
            RoutedTool::ContainerExec => {
                let command = required_string(&arguments, "command")?;
                let cwd = optional_string(&arguments, "cwd");
                let timeout = arguments.get("timeout_secs").and_then(Value::as_u64);
                let container_id = self.container_id(agent_id).await?;
                let output = self
                    .docker
                    .exec_shell(&container_id, &command, cwd.as_deref(), timeout)
                    .await?;
                Ok(ToolExecution {
                    success: output.status == 0,
                    output: serde_json::to_string(&json!({
                        "status": output.status,
                        "stdout": output.stdout,
                        "stderr": output.stderr,
                    }))
                    .unwrap_or_else(|_| "{}".to_string()),
                })
            }
            RoutedTool::ContainerCpUpload => {
                let path = required_string(&arguments, "path")?;
                let content_base64 = required_string(&arguments, "content_base64")?;
                let bytes = self
                    .upload_file(agent_id, path.clone(), content_base64)
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "path": path, "bytes": bytes }).to_string(),
                })
            }
            RoutedTool::ContainerCpDownload => {
                let path = required_string(&arguments, "path")?;
                let bytes = self.download_file_tar(agent_id, path.clone()).await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({
                        "path": path,
                        "tar_base64": BASE64.encode(bytes),
                    })
                    .to_string(),
                })
            }
            RoutedTool::SpawnAgent => {
                let name = optional_string(&arguments, "name");
                let message = optional_string(&arguments, "message");
                let parent_summary = agent.summary.read().await.clone();
                let created = self
                    .create_agent(CreateAgentRequest {
                        name,
                        provider_id: optional_string(&arguments, "provider_id")
                            .or(Some(parent_summary.provider_id)),
                        model: optional_string(&arguments, "model").or(Some(parent_summary.model)),
                        parent_id: Some(agent_id),
                        system_prompt: None,
                    })
                    .await?;
                let turn_id = if let Some(message) = message {
                    let turn_id = self.prepare_turn(created.id).await?;
                    self.spawn_turn(created.id, turn_id, message, Vec::new());
                    Some(turn_id)
                } else {
                    None
                };
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "agent": created, "turn_id": turn_id }).to_string(),
                })
            }
            RoutedTool::SendMessage => {
                let target = parse_agent_id(&required_string(&arguments, "agent_id")?)?;
                let message = required_string(&arguments, "message")?;
                let turn_id = self.prepare_turn(target).await?;
                self.spawn_turn(target, turn_id, message, Vec::new());
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "turn_id": turn_id }).to_string(),
                })
            }
            RoutedTool::WaitAgent => {
                let target = parse_agent_id(&required_string(&arguments, "agent_id")?)?;
                let timeout_secs = arguments
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300);
                let summary = self
                    .wait_agent(target, Duration::from_secs(timeout_secs))
                    .await?;
                Ok(ToolExecution {
                    success: true,
                    output: serde_json::to_string(&summary).unwrap_or_else(|_| "{}".to_string()),
                })
            }
            RoutedTool::ListAgents => Ok(ToolExecution {
                success: true,
                output: serde_json::to_string(&self.list_agents().await)
                    .unwrap_or_else(|_| "[]".to_string()),
            }),
            RoutedTool::CloseAgent => {
                let target = parse_agent_id(&required_string(&arguments, "agent_id")?)?;
                self.delete_agent(target).await?;
                Ok(ToolExecution {
                    success: true,
                    output: json!({ "closed": target }).to_string(),
                })
            }
            RoutedTool::Mcp(model_name) => {
                let manager = agent.mcp.read().await.clone().ok_or_else(|| {
                    RuntimeError::InvalidInput("MCP manager not initialized".to_string())
                })?;
                let output = manager.call_model_tool(&model_name, arguments).await?;
                Ok(ToolExecution {
                    success: true,
                    output: output.to_string(),
                })
            }
            RoutedTool::Unknown(name) => Ok(ToolExecution {
                success: false,
                output: format!("unknown tool: {name}"),
            }),
        }
    }

    async fn wait_agent(&self, agent_id: AgentId, timeout: Duration) -> Result<AgentSummary> {
        let deadline = Instant::now() + timeout;
        loop {
            let agent = self.agent(agent_id).await?;
            let summary = agent.summary.read().await.clone();
            if summary.current_turn.is_none()
                || matches!(
                    summary.status,
                    AgentStatus::Completed
                        | AgentStatus::Failed
                        | AgentStatus::Cancelled
                        | AgentStatus::Deleted
                        | AgentStatus::Idle
                )
            {
                return Ok(summary);
            }
            if Instant::now() >= deadline {
                return Ok(summary);
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    async fn build_instructions(
        &self,
        agent: &AgentRecord,
        loaded_skills: &[mai_skills::LoadedSkill],
        mcp_tools: &[mai_mcp::McpTool],
    ) -> Result<String> {
        let mut instructions = String::from(BASE_INSTRUCTIONS);
        if let Some(system_prompt) = &agent.system_prompt {
            instructions.push_str("\n\n## Agent System Prompt\n");
            instructions.push_str(system_prompt);
        }
        instructions.push_str("\n\n## Available Skills\n");
        instructions.push_str(&self.skills.render_available()?);
        if !loaded_skills.is_empty() {
            instructions.push_str("\n\n## Loaded Skill Instructions\n");
            for skill in loaded_skills {
                instructions.push_str(&format!(
                    "\n### ${}\nPath: {}\n{}\n",
                    skill.summary.name,
                    skill.summary.path.display(),
                    skill.contents
                ));
            }
        }
        instructions.push_str("\n\n## MCP Tools\n");
        if mcp_tools.is_empty() {
            instructions.push_str("No MCP tools are currently available.");
        } else {
            for tool in mcp_tools {
                instructions.push_str(&format!(
                    "\n- {} maps to MCP `{}` on server `{}`",
                    tool.model_name, tool.name, tool.server
                ));
            }
        }
        Ok(instructions)
    }

    async fn finish_turn(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        turn_id: TurnId,
        turn_status: TurnStatus,
        agent_status: AgentStatus,
    ) {
        {
            let mut summary = agent.summary.write().await;
            summary.status = agent_status.clone();
            summary.current_turn = None;
            summary.updated_at = now();
        }
        self.publish(ServiceEventKind::TurnCompleted {
            agent_id,
            turn_id,
            status: turn_status,
        })
        .await;
        self.publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: agent_status,
        })
        .await;
    }

    async fn set_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) {
        let agent_id = {
            let mut summary = agent.summary.write().await;
            summary.status = status.clone();
            summary.updated_at = now();
            if let Some(error) = error {
                summary.last_error = Some(error);
            }
            summary.id
        };
        self.publish(ServiceEventKind::AgentStatusChanged { agent_id, status })
            .await;
    }

    async fn record_message(&self, agent: &AgentRecord, role: MessageRole, content: String) {
        agent.messages.lock().await.push(AgentMessage {
            role,
            content,
            created_at: now(),
        });
    }

    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        self.agents
            .read()
            .await
            .get(&agent_id)
            .cloned()
            .ok_or(RuntimeError::AgentNotFound(agent_id))
    }

    async fn container_id(&self, agent_id: AgentId) -> Result<String> {
        let agent = self.agent(agent_id).await?;
        agent
            .container
            .read()
            .await
            .as_ref()
            .map(|container| container.id.clone())
            .ok_or(RuntimeError::MissingContainer(agent_id))
    }

    async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<mai_mcp::McpTool> {
        let Some(manager) = agent.mcp.read().await.clone() else {
            return Vec::new();
        };
        manager.tools().await
    }

    async fn publish(&self, kind: ServiceEventKind) {
        let event = ServiceEvent {
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst),
            timestamp: now(),
            kind,
        };
        {
            let mut recent = self.recent_events.lock().await;
            if recent.len() >= RECENT_EVENT_LIMIT {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.event_tx.send(event);
    }
}

fn required_string(arguments: &Value, field: &str) -> Result<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing string field `{field}`")))
}

fn optional_string(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn parse_agent_id(value: &str) -> Result<AgentId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid agent_id `{value}`: {err}")))
}

fn short_id(id: AgentId) -> String {
    id.to_string().chars().take(8).collect()
}

fn extract_skill_mentions(text: &str) -> Vec<String> {
    text.split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ')' || ch == '(')
        .filter_map(|part| part.strip_prefix('$'))
        .map(|part| {
            part.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
                .to_string()
        })
        .filter(|part| !part.is_empty())
        .collect()
}

fn event_agent_id(event: &ServiceEvent) -> Option<AgentId> {
    match &event.kind {
        ServiceEventKind::AgentCreated { agent } => Some(agent.id),
        ServiceEventKind::AgentStatusChanged { agent_id, .. }
        | ServiceEventKind::AgentDeleted { agent_id }
        | ServiceEventKind::TurnStarted { agent_id, .. }
        | ServiceEventKind::TurnCompleted { agent_id, .. }
        | ServiceEventKind::ToolStarted { agent_id, .. }
        | ServiceEventKind::ToolCompleted { agent_id, .. }
        | ServiceEventKind::AgentMessage { agent_id, .. } => Some(*agent_id),
        ServiceEventKind::Error { agent_id, .. } => *agent_id,
    }
}

const BASE_INSTRUCTIONS: &str = r#"You are Mai, a coding agent running inside a Docker-backed multi-agent service.

General rules:
- You execute all local work inside your own Docker container; do not assume access to a host workspace.
- Use `container_exec` for shell commands inside your container.
- Use `container_cp_upload` and `container_cp_download` for file transfer.
- Use `spawn_agent`, `send_message`, `wait_agent`, `list_agents`, and `close_agent` for multi-agent collaboration.
- Keep each child agent task concrete and bounded. Multiple agents can run in parallel.
- Use available skills only when explicitly requested by the user or when clearly relevant.
- MCP tools are exposed as ordinary function tools whose names begin with `mcp__`.
- Be concise with final answers and include important file paths or command outputs when they matter.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_skill_mentions() {
        assert_eq!(
            extract_skill_mentions("please use $rust-dev, then $doc."),
            vec!["rust-dev", "doc"]
        );
    }

    #[test]
    fn agent_status_allows_new_turn_after_completion() {
        assert!(AgentStatus::Completed.can_start_turn());
        assert!(!AgentStatus::RunningTurn.can_start_turn());
    }
}
