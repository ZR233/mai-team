use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use mai_docker::{ContainerHandle, DockerClient};
use mai_mcp::McpAgentManager;
use mai_model::ResponsesClient;
use mai_protocol::{
    AgentConfigRequest, AgentConfigResponse, AgentDetail, AgentId, AgentMessage,
    AgentModelPreference, AgentSessionSummary, AgentStatus, AgentSummary, ContextUsage,
    CreateAgentRequest, MessageRole, ModelConfig, ModelContentItem, ModelInputItem,
    ModelOutputItem, ModelToolCall, ProviderKind, ReasoningEffort, ResolvedAgentModelPreference,
    ServiceEvent, ServiceEventKind, SessionId, TokenUsage, ToolTraceDetail, TurnId, TurnStatus,
    UpdateAgentRequest, now, preview,
};
use mai_skills::SkillsManager;
use mai_store::{ConfigStore, ProviderSelection};
use mai_tools::{RoutedTool, build_tool_definitions, route_tool};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
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
const AUTO_COMPACT_THRESHOLD_PERCENT: u64 = 80;
const COMPACT_USER_MESSAGE_MAX_CHARS: usize = 80_000;
const COMPACT_SUMMARY_PREVIEW_CHARS: usize = 240;
const COMPACT_SUMMARY_PREFIX: &str = "Context checkpoint summary from earlier conversation history. This is background for continuity, not a new user request.";
const COMPACT_PROMPT: &str = r#"You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will continue this agent session.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done as clear next steps
- Any critical data, examples, file paths, command outputs, or references needed to continue

Be concise, structured, and focused on helping the next model seamlessly continue the work."#;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("agent not found: {0}")]
    AgentNotFound(AgentId),
    #[error("agent is busy: {0}")]
    AgentBusy(AgentId),
    #[error("agent has no container: {0}")]
    MissingContainer(AgentId),
    #[error("session not found: {agent_id}/{session_id}")]
    SessionNotFound {
        agent_id: AgentId,
        session_id: SessionId,
    },
    #[error("tool trace not found: {agent_id}/{call_id}")]
    ToolTraceNotFound { agent_id: AgentId, call_id: String },
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
    sessions: Mutex<Vec<AgentSessionRecord>>,
    container: RwLock<Option<ContainerHandle>>,
    mcp: RwLock<Option<Arc<McpAgentManager>>>,
    system_prompt: Option<String>,
    turn_lock: Mutex<()>,
    cancel_requested: AtomicBool,
}

#[derive(Clone)]
struct AgentSessionRecord {
    summary: AgentSessionSummary,
    messages: Vec<AgentMessage>,
    history: Vec<ModelInputItem>,
    last_context_tokens: Option<u64>,
}

#[derive(Debug)]
struct ToolExecution {
    success: bool,
    output: String,
}

#[derive(Debug, Clone)]
enum ContainerSource {
    FreshImage,
    CloneFrom { parent_container_id: String },
}

struct ResolvedAgentModel {
    preference: AgentModelPreference,
    effective: ResolvedAgentModelPreference,
}

impl AgentRuntime {
    pub async fn new(
        docker: DockerClient,
        model: ResponsesClient,
        store: Arc<ConfigStore>,
        config: RuntimeConfig,
    ) -> Result<Arc<Self>> {
        let skills = SkillsManager::new(&config.repo_root);
        let (event_tx, _) = broadcast::channel(1024);
        let snapshot = store.load_runtime_snapshot(RECENT_EVENT_LIMIT).await?;
        let mut agents = HashMap::new();
        for persisted in snapshot.agents {
            let (summary, changed) = recovered_summary(persisted.summary);
            let mut sessions = Vec::new();
            for persisted_session in persisted.sessions {
                let messages = store
                    .load_agent_messages(summary.id, persisted_session.summary.id)
                    .await?;
                sessions.push(AgentSessionRecord {
                    summary: AgentSessionSummary {
                        message_count: messages.len(),
                        ..persisted_session.summary
                    },
                    messages,
                    history: persisted_session.history,
                    last_context_tokens: persisted_session.last_context_tokens,
                });
            }
            if sessions.is_empty() {
                sessions.push(default_session_record());
            }
            let agent = Arc::new(AgentRecord {
                summary: RwLock::new(summary.clone()),
                sessions: Mutex::new(sessions),
                container: RwLock::new(None),
                mcp: RwLock::new(None),
                system_prompt: persisted.system_prompt,
                turn_lock: Mutex::new(()),
                cancel_requested: AtomicBool::new(false),
            });
            if changed {
                store
                    .save_agent(&summary, agent.system_prompt.as_deref())
                    .await?;
            }
            agents.insert(summary.id, agent);
        }

        Ok(Arc::new(Self {
            docker,
            model,
            store,
            skills,
            agents: RwLock::new(agents),
            event_tx,
            sequence: AtomicU64::new(snapshot.next_sequence),
            recent_events: Mutex::new(snapshot.recent_events.into_iter().collect()),
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.event_tx.subscribe()
    }

    pub async fn agent_config(&self) -> Result<AgentConfigResponse> {
        let config = self.store.load_agent_config().await?;
        let (effective_research_agent, validation_error) = match self
            .resolve_agent_model_preference(config.research_agent.as_ref())
            .await
        {
            Ok(resolved) => (Some(resolved.effective), None),
            Err(err) => (None, Some(err.to_string())),
        };
        Ok(AgentConfigResponse {
            research_agent: config.research_agent,
            effective_research_agent,
            validation_error,
        })
    }

    pub async fn update_agent_config(
        &self,
        request: AgentConfigRequest,
    ) -> Result<AgentConfigResponse> {
        if let Some(preference) = request.research_agent.as_ref() {
            self.resolve_agent_model_preference(Some(preference))
                .await?;
        }
        self.store.save_agent_config(&request).await?;
        self.agent_config().await
    }

    pub async fn create_agent(
        self: &Arc<Self>,
        request: CreateAgentRequest,
    ) -> Result<AgentSummary> {
        self.create_agent_with_container_source(request, ContainerSource::FreshImage)
            .await
    }

    async fn create_agent_with_container_source(
        self: &Arc<Self>,
        request: CreateAgentRequest,
        container_source: ContainerSource,
    ) -> Result<AgentSummary> {
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let name = request
            .name
            .unwrap_or_else(|| format!("agent-{}", short_id(id)));
        let provider_selection = self
            .store
            .resolve_provider(request.provider_id.as_deref(), request.model.as_deref())
            .await?;
        let reasoning_effort = normalize_reasoning_effort(
            provider_selection.provider.kind,
            &provider_selection.model,
            request.reasoning_effort,
            true,
        )?;
        let system_prompt = request.system_prompt;
        let summary = AgentSummary {
            id,
            parent_id: request.parent_id,
            name,
            status: AgentStatus::Created,
            container_id: None,
            provider_id: provider_selection.provider.id.clone(),
            provider_name: provider_selection.provider.name.clone(),
            model: provider_selection.model.id.clone(),
            reasoning_effort,
            created_at,
            updated_at: created_at,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        self.store
            .save_agent(&summary, system_prompt.as_deref())
            .await?;
        let session = default_session_record();
        self.store.save_agent_session(id, &session.summary).await?;

        let agent = Arc::new(AgentRecord {
            summary: RwLock::new(summary.clone()),
            sessions: Mutex::new(vec![session]),
            container: RwLock::new(None),
            mcp: RwLock::new(None),
            system_prompt,
            turn_lock: Mutex::new(()),
            cancel_requested: AtomicBool::new(false),
        });

        self.agents.write().await.insert(id, Arc::clone(&agent));
        self.publish(ServiceEventKind::AgentCreated {
            agent: summary.clone(),
        })
        .await;

        match self
            .ensure_agent_container_with_source(&agent, AgentStatus::Idle, &container_source)
            .await
        {
            Ok(_) => Ok(agent.summary.read().await.clone()),
            Err(err) => {
                let message = err.to_string();
                if let Err(store_err) = self
                    .set_status(&agent, AgentStatus::Failed, Some(message.clone()))
                    .await
                {
                    tracing::warn!("failed to persist agent failure: {store_err}");
                }
                self.publish(ServiceEventKind::Error {
                    agent_id: Some(id),
                    session_id: None,
                    turn_id: None,
                    message,
                })
                .await;
                Err(err)
            }
        }
    }

    pub async fn list_agents(&self) -> Vec<AgentSummary> {
        let agents = self.agents.read().await;
        let mut summaries = Vec::with_capacity(agents.len());
        for agent in agents.values() {
            summaries.push(agent.summary.read().await.clone());
        }
        summaries.sort_by_key(|s| s.created_at);
        summaries
    }

    pub async fn update_agent(
        &self,
        agent_id: AgentId,
        request: UpdateAgentRequest,
    ) -> Result<AgentSummary> {
        let agent = self.agent(agent_id).await?;
        {
            let summary = agent.summary.read().await;
            if !summary.status.can_start_turn() || summary.current_turn.is_some() {
                return Err(RuntimeError::AgentBusy(agent_id));
            }
        }
        let current = agent.summary.read().await.clone();
        let provider_id = request
            .provider_id
            .as_deref()
            .or(Some(&current.provider_id));
        let model = request.model.as_deref().or(Some(&current.model));
        let provider_selection = self.store.resolve_provider(provider_id, model).await?;
        let requested_reasoning_effort = if request.reasoning_effort.is_some()
            || provider_selection.model.id != current.model
            || provider_selection.provider.id != current.provider_id
        {
            request.reasoning_effort
        } else {
            current.reasoning_effort
        };
        let reasoning_effort = normalize_reasoning_effort(
            provider_selection.provider.kind,
            &provider_selection.model,
            requested_reasoning_effort,
            true,
        )?;
        let updated = {
            let mut summary = agent.summary.write().await;
            summary.provider_id = provider_selection.provider.id.clone();
            summary.provider_name = provider_selection.provider.name.clone();
            summary.model = provider_selection.model.id.clone();
            summary.reasoning_effort = reasoning_effort;
            summary.updated_at = now();
            summary.clone()
        };
        self.persist_agent(&agent).await?;
        self.publish(ServiceEventKind::AgentUpdated {
            agent: updated.clone(),
        })
        .await;
        Ok(updated)
    }

    pub async fn cleanup_orphaned_containers(&self) -> Result<Vec<String>> {
        let active_agent_ids = {
            let agents = self.agents.read().await;
            agents
                .keys()
                .map(ToString::to_string)
                .collect::<HashSet<_>>()
        };
        Ok(self
            .docker
            .cleanup_orphaned_agent_containers(&active_agent_ids)
            .await?)
    }

    pub async fn get_agent(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<AgentDetail> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let (sessions, selected_session_id, context_tokens_used, messages) = {
            let sessions = agent.sessions.lock().await;
            let selected_session = selected_session(&sessions, session_id).ok_or_else(|| {
                RuntimeError::SessionNotFound {
                    agent_id,
                    session_id: session_id.unwrap_or_default(),
                }
            })?;
            (
                sessions
                    .iter()
                    .map(|session| session.summary.clone())
                    .collect(),
                selected_session.summary.id,
                selected_session.last_context_tokens.unwrap_or_default(),
                selected_session.messages.clone(),
            )
        };
        let context_usage = self
            .store
            .resolve_provider(Some(&summary.provider_id), Some(&summary.model))
            .await
            .ok()
            .map(|provider_selection| ContextUsage {
                used_tokens: context_tokens_used,
                context_tokens: provider_selection.model.context_tokens,
                threshold_percent: AUTO_COMPACT_THRESHOLD_PERCENT,
            });
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
            sessions,
            selected_session_id,
            context_usage,
            messages,
            recent_events,
        })
    }

    pub async fn create_session(&self, agent_id: AgentId) -> Result<AgentSessionSummary> {
        let agent = self.agent(agent_id).await?;
        let session = {
            let mut sessions = agent.sessions.lock().await;
            let session = AgentSessionRecord {
                summary: AgentSessionSummary {
                    id: Uuid::new_v4(),
                    title: format!("Chat {}", sessions.len() + 1),
                    created_at: now(),
                    updated_at: now(),
                    message_count: 0,
                },
                messages: Vec::new(),
                history: Vec::new(),
                last_context_tokens: None,
            };
            sessions.push(session.clone());
            session.summary
        };
        self.store.save_agent_session(agent_id, &session).await?;
        Ok(session)
    }

    pub async fn tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
    ) -> Result<ToolTraceDetail> {
        let agent = self.agent(agent_id).await?;
        let (session_id, history) = {
            let sessions = agent.sessions.lock().await;
            let selected_session = selected_session(&sessions, session_id).ok_or_else(|| {
                RuntimeError::SessionNotFound {
                    agent_id,
                    session_id: session_id.unwrap_or_default(),
                }
            })?;
            (
                selected_session.summary.id,
                selected_session.history.clone(),
            )
        };
        let mut tool_name = None;
        let mut arguments = None;
        let mut output = None;

        for item in history {
            match item {
                ModelInputItem::FunctionCall {
                    call_id: item_call_id,
                    name,
                    arguments: raw_arguments,
                } if item_call_id == call_id => {
                    tool_name = Some(name);
                    arguments = Some(parse_tool_arguments(&raw_arguments));
                }
                ModelInputItem::AssistantTurn { tool_calls, .. } => {
                    for tool_call in tool_calls {
                        if tool_call.call_id == call_id {
                            tool_name = Some(tool_call.name);
                            arguments = Some(parse_tool_arguments(&tool_call.arguments));
                            break;
                        }
                    }
                }
                ModelInputItem::FunctionCallOutput {
                    call_id: item_call_id,
                    output: item_output,
                } if item_call_id == call_id => {
                    output = Some(item_output);
                }
                _ => {}
            }
        }

        let tool_name = tool_name.ok_or_else(|| RuntimeError::ToolTraceNotFound {
            agent_id,
            call_id: call_id.clone(),
        })?;
        let output = output.unwrap_or_default();
        let (event_success, duration_ms) = self
            .tool_event_metadata(agent_id, session_id, &call_id)
            .await;
        Ok(ToolTraceDetail {
            call_id,
            tool_name,
            arguments: arguments.unwrap_or_else(|| json!({})),
            success: event_success.unwrap_or(!output.is_empty()),
            output,
            duration_ms,
        })
    }

    pub async fn send_message(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let session_id = self.resolve_session_id(agent_id, session_id).await?;
        let turn_id = self.prepare_turn(agent_id).await?;
        self.spawn_turn(agent_id, session_id, turn_id, message, skill_mentions);
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
        self.persist_agent(&agent).await?;
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
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            runtime
                .run_turn(agent_id, session_id, turn_id, message, skill_mentions)
                .await;
        });
    }

    pub async fn cancel_agent(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        agent.cancel_requested.store(true, Ordering::SeqCst);
        self.set_status(&agent, AgentStatus::Cancelled, None)
            .await?;
        Ok(())
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        let targets = self.descendant_delete_order(agent_id).await?;
        for target_id in targets {
            self.delete_agent_record(target_id).await?;
        }
        Ok(())
    }

    async fn delete_agent_record(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        agent.cancel_requested.store(true, Ordering::SeqCst);
        self.set_status(&agent, AgentStatus::DeletingContainer, None)
            .await?;
        *agent.mcp.write().await = None;
        let in_memory_container_id = agent
            .container
            .write()
            .await
            .take()
            .map(|container| container.id);
        let persisted_container_id = agent.summary.read().await.container_id.clone();
        let preferred_container_id = in_memory_container_id.or(persisted_container_id);
        let deleted = self
            .docker
            .delete_agent_containers(&agent_id.to_string(), preferred_container_id.as_deref())
            .await?;
        if !deleted.is_empty() {
            tracing::info!(
                agent_id = %agent_id,
                count = deleted.len(),
                "removed agent containers"
            );
        }
        let _turn_guard = agent.turn_lock.lock().await;
        self.set_status(&agent, AgentStatus::Deleted, None).await?;
        self.store.delete_agent(agent_id).await?;
        self.agents.write().await.remove(&agent_id);
        self.publish(ServiceEventKind::AgentDeleted { agent_id })
            .await;
        Ok(())
    }

    async fn descendant_delete_order(&self, root_id: AgentId) -> Result<Vec<AgentId>> {
        let summaries = {
            let agents = self.agents.read().await;
            let mut summaries = Vec::with_capacity(agents.len());
            for agent in agents.values() {
                summaries.push(agent.summary.read().await.clone());
            }
            summaries
        };
        if !summaries.iter().any(|summary| summary.id == root_id) {
            return Err(RuntimeError::AgentNotFound(root_id));
        }

        Ok(descendant_delete_order_from_summaries(root_id, &summaries))
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
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        skill_mentions: Vec<String>,
    ) {
        let result = self
            .run_turn_inner(agent_id, session_id, turn_id, message, skill_mentions)
            .await;
        if let Err(err) = result
            && let Ok(agent) = self.agent(agent_id).await
        {
            {
                let mut summary = agent.summary.write().await;
                summary.status = AgentStatus::Failed;
                summary.current_turn = None;
                summary.updated_at = now();
                summary.last_error = Some(err.to_string());
            }
            if let Err(store_err) = self.persist_agent(&agent).await {
                tracing::warn!("failed to persist failed turn state: {store_err}");
            }
            self.publish(ServiceEventKind::Error {
                agent_id: Some(agent_id),
                session_id: Some(session_id),
                turn_id: Some(turn_id),
                message: err.to_string(),
            })
            .await;
            self.publish(ServiceEventKind::TurnCompleted {
                agent_id,
                session_id: Some(session_id),
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

    async fn run_turn_inner(
        self: &Arc<Self>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        message: String,
        mut skill_mentions: Vec<String>,
    ) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let _turn_guard = agent.turn_lock.lock().await;
        self.ensure_agent_container(&agent, AgentStatus::RunningTurn)
            .await?;
        self.publish(ServiceEventKind::TurnStarted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
        })
        .await;

        skill_mentions.extend(extract_skill_mentions(&message));
        if let Err(err) = self
            .maybe_auto_compact(&agent, agent_id, session_id, turn_id)
            .await
        {
            tracing::warn!("auto context compaction failed before user message: {err}");
        }
        self.record_message(
            &agent,
            agent_id,
            session_id,
            MessageRole::User,
            message.clone(),
        )
        .await?;
        self.record_history_item(
            &agent,
            agent_id,
            session_id,
            ModelInputItem::user_text(message.clone()),
        )
        .await?;
        self.publish(ServiceEventKind::AgentMessage {
            agent_id,
            session_id: Some(session_id),
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
                    session_id,
                    turn_id,
                    TurnStatus::Cancelled,
                    AgentStatus::Cancelled,
                )
                .await?;
                return Ok(());
            }

            self.set_status(&agent, AgentStatus::RunningTurn, None)
                .await?;
            let mcp_tools = self.agent_mcp_tools(&agent).await;
            let tools = build_tool_definitions(&mcp_tools);
            let instructions = self
                .build_instructions(&agent, &loaded_skills, &mcp_tools)
                .await?;
            let summary = agent.summary.read().await.clone();
            let model_name = summary.model.clone();
            let provider_id = summary.provider_id.clone();
            let reasoning_effort = summary.reasoning_effort;
            let provider_selection = self
                .store
                .resolve_provider(Some(&provider_id), Some(&model_name))
                .await?;
            if let Err(err) = self
                .maybe_auto_compact(&agent, agent_id, session_id, turn_id)
                .await
            {
                tracing::warn!("auto context compaction failed before model request: {err}");
            }
            let history = self.session_history(&agent, agent_id, session_id).await?;
            let response = self
                .model
                .create_response(
                    &provider_selection.provider,
                    &provider_selection.model,
                    &instructions,
                    &history,
                    &tools,
                    reasoning_effort,
                )
                .await?;

            if let Some(usage) = response.usage {
                {
                    let mut summary = agent.summary.write().await;
                    summary.token_usage.add(&usage);
                    summary.updated_at = now();
                }
                self.persist_agent(&agent).await?;
                self.record_session_context_tokens(
                    &agent,
                    agent_id,
                    session_id,
                    usage.total_tokens,
                )
                .await?;
            }

            let mut tool_calls = Vec::new();
            for item in response.output {
                match item {
                    ModelOutputItem::Message { text } => {
                        if !text.trim().is_empty() {
                            self.record_message(
                                &agent,
                                agent_id,
                                session_id,
                                MessageRole::Assistant,
                                text.clone(),
                            )
                            .await?;
                            self.record_history_item(
                                &agent,
                                agent_id,
                                session_id,
                                ModelInputItem::assistant_text(text.clone()),
                            )
                            .await?;
                            self.publish(ServiceEventKind::AgentMessage {
                                agent_id,
                                session_id: Some(session_id),
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
                        self.record_history_item(
                            &agent,
                            agent_id,
                            session_id,
                            ModelInputItem::FunctionCall {
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: raw_arguments,
                            },
                        )
                        .await?;
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelOutputItem::AssistantTurn {
                        content,
                        reasoning_content,
                        tool_calls: output_tool_calls,
                    } => {
                        let assistant_tool_calls = output_tool_calls
                            .into_iter()
                            .map(|tool_call| {
                                let call_id = if tool_call.call_id.is_empty() {
                                    format!("call_{}", Uuid::new_v4())
                                } else {
                                    tool_call.call_id
                                };
                                let name = tool_call.name;
                                let arguments = tool_call.arguments;
                                let raw_arguments = tool_call.raw_arguments;
                                tool_calls.push((call_id.clone(), name.clone(), arguments));
                                ModelToolCall {
                                    call_id,
                                    name,
                                    arguments: raw_arguments,
                                }
                            })
                            .collect::<Vec<_>>();
                        let has_content =
                            content.as_ref().is_some_and(|text| !text.trim().is_empty());
                        let has_reasoning = reasoning_content
                            .as_ref()
                            .is_some_and(|reasoning| !reasoning.trim().is_empty());
                        if has_content || has_reasoning || !assistant_tool_calls.is_empty() {
                            self.record_history_item(
                                &agent,
                                agent_id,
                                session_id,
                                ModelInputItem::AssistantTurn {
                                    content: content.clone().filter(|text| !text.is_empty()),
                                    reasoning_content: reasoning_content
                                        .filter(|reasoning| !reasoning.trim().is_empty()),
                                    tool_calls: assistant_tool_calls,
                                },
                            )
                            .await?;
                        }
                        if let Some(text) = content.filter(|text| !text.trim().is_empty()) {
                            self.record_message(
                                &agent,
                                agent_id,
                                session_id,
                                MessageRole::Assistant,
                                text.clone(),
                            )
                            .await?;
                            self.publish(ServiceEventKind::AgentMessage {
                                agent_id,
                                session_id: Some(session_id),
                                turn_id: Some(turn_id),
                                role: MessageRole::Assistant,
                                content: text,
                            })
                            .await;
                        }
                    }
                    ModelOutputItem::Other { .. } => {}
                }
            }

            if tool_calls.is_empty() {
                self.finish_turn(
                    &agent,
                    agent_id,
                    session_id,
                    turn_id,
                    TurnStatus::Completed,
                    AgentStatus::Completed,
                )
                .await?;
                return Ok(());
            }

            self.set_status(&agent, AgentStatus::WaitingTool, None)
                .await?;
            for (call_id, name, arguments) in tool_calls {
                let arguments_preview = trace_preview_value(&arguments, 500);
                let inline_arguments = inline_event_arguments(&arguments);
                self.publish(ServiceEventKind::ToolStarted {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id,
                    call_id: call_id.clone(),
                    tool_name: name.clone(),
                    arguments_preview: Some(arguments_preview),
                    arguments: inline_arguments,
                })
                .await;
                let started_at = Instant::now();
                let output = self
                    .execute_tool(&agent, agent_id, turn_id, &name, arguments)
                    .await;
                let duration_ms = u128_to_u64(started_at.elapsed().as_millis());
                let execution = match output {
                    Ok(execution) => execution,
                    Err(err) => ToolExecution {
                        success: false,
                        output: err.to_string(),
                    },
                };
                self.record_history_item(
                    &agent,
                    agent_id,
                    session_id,
                    ModelInputItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: execution.output.clone(),
                    },
                )
                .await?;
                self.publish(ServiceEventKind::ToolCompleted {
                    agent_id,
                    session_id: Some(session_id),
                    turn_id,
                    call_id,
                    tool_name: name,
                    success: execution.success,
                    output_preview: trace_preview_output(&execution.output, 500),
                    duration_ms: Some(duration_ms),
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
                let child_model = self.resolve_research_agent_model().await?;
                let parent_container_id = self.container_id(agent_id).await?;
                let created = self
                    .create_agent_with_container_source(
                        CreateAgentRequest {
                            name,
                            provider_id: Some(child_model.preference.provider_id),
                            model: Some(child_model.preference.model),
                            reasoning_effort: child_model.preference.reasoning_effort,
                            parent_id: Some(agent_id),
                            system_prompt: None,
                        },
                        ContainerSource::CloneFrom {
                            parent_container_id,
                        },
                    )
                    .await?;
                let turn_id = if let Some(message) = message {
                    let session_id = self.resolve_session_id(created.id, None).await?;
                    let turn_id = self.prepare_turn(created.id).await?;
                    self.spawn_turn(created.id, session_id, turn_id, message, Vec::new());
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
                let session_id = optional_string(&arguments, "session_id")
                    .as_deref()
                    .map(parse_session_id)
                    .transpose()?;
                let message = required_string(&arguments, "message")?;
                let session_id = self.resolve_session_id(target, session_id).await?;
                let turn_id = self.prepare_turn(target).await?;
                self.spawn_turn(target, session_id, turn_id, message, Vec::new());
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

    #[cfg(test)]
    async fn execute_tool_for_test(
        self: &Arc<Self>,
        agent_id: AgentId,
        name: &str,
        arguments: Value,
    ) -> Result<ToolExecution> {
        let agent = self.agent(agent_id).await?;
        self.execute_tool(&agent, agent_id, Uuid::new_v4(), name, arguments)
            .await
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
        session_id: SessionId,
        turn_id: TurnId,
        turn_status: TurnStatus,
        agent_status: AgentStatus,
    ) -> Result<()> {
        {
            let mut summary = agent.summary.write().await;
            summary.status = agent_status.clone();
            summary.current_turn = None;
            summary.updated_at = now();
        }
        self.persist_agent(agent).await?;
        self.publish(ServiceEventKind::TurnCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            status: turn_status,
        })
        .await;
        self.publish(ServiceEventKind::AgentStatusChanged {
            agent_id,
            status: agent_status,
        })
        .await;
        Ok(())
    }

    async fn set_status(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        error: Option<String>,
    ) -> Result<()> {
        let agent_id = {
            let mut summary = agent.summary.write().await;
            summary.status = status.clone();
            summary.updated_at = now();
            if let Some(error) = error {
                summary.last_error = Some(error);
            }
            summary.id
        };
        self.persist_agent(agent).await?;
        self.publish(ServiceEventKind::AgentStatusChanged { agent_id, status })
            .await;
        Ok(())
    }

    async fn record_message(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        role: MessageRole,
        content: String,
    ) -> Result<()> {
        let message = AgentMessage {
            role,
            content,
            created_at: now(),
        };
        let (position, session_summary) = {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            let position = session.messages.len();
            session.messages.push(message.clone());
            session.summary.message_count = session.messages.len();
            session.summary.updated_at = message.created_at;
            (position, session.summary.clone())
        };
        self.store
            .save_agent_session(agent_id, &session_summary)
            .await?;
        self.store
            .append_agent_message(agent_id, session_id, position, &message)
            .await?;
        Ok(())
    }

    async fn record_history_item(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        item: ModelInputItem,
    ) -> Result<()> {
        let position = {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            let position = session.history.len();
            session.history.push(item.clone());
            position
        };
        self.store
            .append_agent_history_item(agent_id, session_id, position, &item)
            .await?;
        Ok(())
    }

    async fn replace_session_history(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        history: Vec<ModelInputItem>,
    ) -> Result<()> {
        self.store
            .replace_agent_history(agent_id, session_id, &history)
            .await?;
        {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            session.history = history.clone();
            session.last_context_tokens = None;
        }
        Ok(())
    }

    async fn record_session_context_tokens(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        tokens: u64,
    ) -> Result<()> {
        {
            let mut sessions = agent.sessions.lock().await;
            let session = sessions
                .iter_mut()
                .find(|session| session.summary.id == session_id)
                .ok_or(RuntimeError::SessionNotFound {
                    agent_id,
                    session_id,
                })?;
            session.last_context_tokens = Some(tokens);
        }
        self.store
            .save_session_context_tokens(agent_id, session_id, tokens)
            .await?;
        Ok(())
    }

    async fn maybe_auto_compact(
        self: &Arc<Self>,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<()> {
        let last_context_tokens = self
            .session_context_tokens(agent, agent_id, session_id)
            .await?;
        let Some(tokens_before) = last_context_tokens else {
            return Ok(());
        };
        let summary = agent.summary.read().await.clone();
        let provider_selection = self
            .store
            .resolve_provider(Some(&summary.provider_id), Some(&summary.model))
            .await?;
        if !should_auto_compact(tokens_before, provider_selection.model.context_tokens) {
            return Ok(());
        }

        let history = self.session_history(agent, agent_id, session_id).await?;
        if history.is_empty() {
            self.record_session_context_tokens(agent, agent_id, session_id, 0)
                .await?;
            return Ok(());
        }
        let mut compact_input = history.clone();
        compact_input.push(ModelInputItem::user_text(COMPACT_PROMPT));
        let instructions = self.build_instructions(agent, &[], &[]).await?;
        let response = self
            .model
            .create_response(
                &provider_selection.provider,
                &provider_selection.model,
                &instructions,
                &compact_input,
                &[],
                summary.reasoning_effort,
            )
            .await?;

        if let Some(usage) = response.usage {
            {
                let mut summary = agent.summary.write().await;
                summary.token_usage.add(&usage);
                summary.updated_at = now();
            }
            self.persist_agent(agent).await?;
        }

        let summary_text = compact_summary_from_output(&response.output).ok_or_else(|| {
            RuntimeError::InvalidInput("compact response did not include a summary".to_string())
        })?;
        let replacement = build_compacted_history(&history, &summary_text);
        self.replace_session_history(agent, agent_id, session_id, replacement)
            .await?;
        self.publish(ServiceEventKind::ContextCompacted {
            agent_id,
            session_id,
            turn_id,
            tokens_before,
            summary_preview: preview(&summary_text, COMPACT_SUMMARY_PREVIEW_CHARS),
        })
        .await;
        Ok(())
    }

    async fn session_context_tokens(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Option<u64>> {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .map(|session| session.last_context_tokens)
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })
    }

    async fn persist_agent(&self, agent: &AgentRecord) -> Result<()> {
        let summary = agent.summary.read().await.clone();
        self.store
            .save_agent(&summary, agent.system_prompt.as_deref())
            .await?;
        Ok(())
    }

    async fn resolve_session_id(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<SessionId> {
        let agent = self.agent(agent_id).await?;
        let sessions = agent.sessions.lock().await;
        selected_session(&sessions, session_id)
            .map(|session| session.summary.id)
            .ok_or_else(|| RuntimeError::SessionNotFound {
                agent_id,
                session_id: session_id.unwrap_or_default(),
            })
    }

    async fn resolve_research_agent_model(&self) -> Result<ResolvedAgentModel> {
        let config = self.store.load_agent_config().await?;
        self.resolve_agent_model_preference(config.research_agent.as_ref())
            .await
    }

    async fn resolve_agent_model_preference(
        &self,
        preference: Option<&AgentModelPreference>,
    ) -> Result<ResolvedAgentModel> {
        if let Some(preference) = preference
            && (preference.provider_id.trim().is_empty() || preference.model.trim().is_empty())
        {
            return Err(RuntimeError::InvalidInput(
                "research agent provider and model are required".to_string(),
            ));
        }
        let selection = self
            .store
            .resolve_provider(
                preference.map(|item| item.provider_id.as_str()),
                preference.map(|item| item.model.as_str()),
            )
            .await?;
        let reasoning_effort = normalize_reasoning_effort(
            selection.provider.kind,
            &selection.model,
            preference.and_then(|item| item.reasoning_effort),
            true,
        )?;
        Ok(resolved_agent_model(selection, reasoning_effort))
    }

    async fn session_history(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Vec<ModelInputItem>> {
        let sessions = agent.sessions.lock().await;
        sessions
            .iter()
            .find(|session| session.summary.id == session_id)
            .map(|session| session.history.clone())
            .ok_or(RuntimeError::SessionNotFound {
                agent_id,
                session_id,
            })
    }

    async fn ensure_agent_container(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
    ) -> Result<String> {
        self.ensure_agent_container_with_source(agent, ready_status, &ContainerSource::FreshImage)
            .await
    }

    async fn ensure_agent_container_with_source(
        &self,
        agent: &Arc<AgentRecord>,
        ready_status: AgentStatus,
        container_source: &ContainerSource,
    ) -> Result<String> {
        if let Some(container_id) = agent
            .container
            .read()
            .await
            .as_ref()
            .map(|container| container.id.clone())
        {
            return Ok(container_id);
        }

        let (agent_id, preferred_container_id) = {
            let summary = agent.summary.read().await;
            (summary.id, summary.container_id.clone())
        };
        let mut container_guard = agent.container.write().await;
        if let Some(container_id) = container_guard
            .as_ref()
            .map(|container| container.id.clone())
        {
            return Ok(container_id);
        }

        self.set_status(agent, AgentStatus::StartingContainer, None)
            .await?;
        let container_result = match container_source {
            ContainerSource::FreshImage => {
                self.docker
                    .ensure_agent_container(
                        &agent_id.to_string(),
                        preferred_container_id.as_deref(),
                    )
                    .await
            }
            ContainerSource::CloneFrom {
                parent_container_id,
            } => {
                if preferred_container_id.is_some() {
                    self.docker
                        .ensure_agent_container(
                            &agent_id.to_string(),
                            preferred_container_id.as_deref(),
                        )
                        .await
                } else {
                    self.docker
                        .create_agent_container_from_parent(
                            &agent_id.to_string(),
                            parent_container_id,
                        )
                        .await
                }
            }
        };
        let container = match container_result {
            Ok(container) => container,
            Err(err) => {
                let message = err.to_string();
                drop(container_guard);
                if let Err(store_err) = self
                    .set_status(agent, AgentStatus::Failed, Some(message))
                    .await
                {
                    tracing::warn!("failed to persist container startup failure: {store_err}");
                }
                return Err(err.into());
            }
        };

        let container_id = container.id.clone();
        {
            let mut summary = agent.summary.write().await;
            summary.container_id = Some(container_id.clone());
            summary.updated_at = now();
        }
        self.persist_agent(agent).await?;
        *container_guard = Some(container.clone());
        drop(container_guard);

        let mcp = McpAgentManager::start(
            self.docker.clone(),
            container.id,
            self.store.list_mcp_servers().await?,
        )
        .await;
        *agent.mcp.write().await = Some(Arc::new(mcp));
        self.set_status(agent, ready_status, None).await?;
        Ok(container_id)
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
        if let Some(container_id) = agent
            .container
            .read()
            .await
            .as_ref()
            .map(|container| container.id.clone())
        {
            return Ok(container_id);
        }
        let ready_status = agent.summary.read().await.status.clone();
        self.ensure_agent_container(&agent, ready_status).await
    }

    async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<mai_mcp::McpTool> {
        let Some(manager) = agent.mcp.read().await.clone() else {
            return Vec::new();
        };
        manager.tools().await
    }

    async fn tool_event_metadata(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        call_id: &str,
    ) -> (Option<bool>, Option<u64>) {
        let events = self.recent_events.lock().await;
        events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                ServiceEventKind::ToolCompleted {
                    agent_id: event_agent_id,
                    session_id: event_session_id,
                    call_id: event_call_id,
                    success,
                    duration_ms,
                    ..
                } if *event_agent_id == agent_id
                    && event_session_id == &Some(session_id)
                    && event_call_id == call_id =>
                {
                    Some((Some(*success), *duration_ms))
                }
                _ => None,
            })
            .unwrap_or((None, None))
    }

    async fn publish(&self, kind: ServiceEventKind) {
        let event = ServiceEvent {
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst),
            timestamp: now(),
            kind,
        };
        if let Err(err) = self.store.append_service_event(&event).await {
            tracing::warn!("failed to persist service event: {err}");
        }
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

fn parse_session_id(value: &str) -> Result<SessionId> {
    Uuid::parse_str(value)
        .map_err(|err| RuntimeError::InvalidInput(format!("invalid session_id `{value}`: {err}")))
}

fn resolved_agent_model(
    selection: ProviderSelection,
    reasoning_effort: Option<ReasoningEffort>,
) -> ResolvedAgentModel {
    let effective = resolved_agent_model_preference(selection.clone(), reasoning_effort);
    ResolvedAgentModel {
        preference: AgentModelPreference {
            provider_id: selection.provider.id.clone(),
            model: selection.model.id.clone(),
            reasoning_effort,
        },
        effective,
    }
}

fn resolved_agent_model_preference(
    selection: ProviderSelection,
    reasoning_effort: Option<ReasoningEffort>,
) -> ResolvedAgentModelPreference {
    ResolvedAgentModelPreference {
        provider_id: selection.provider.id,
        provider_name: selection.provider.name,
        provider_kind: selection.provider.kind,
        model: selection.model.id,
        model_name: selection.model.name,
        reasoning_effort,
        context_tokens: selection.model.context_tokens,
        output_tokens: selection.model.output_tokens,
    }
}

fn descendant_delete_order_from_summaries(
    root_id: AgentId,
    summaries: &[AgentSummary],
) -> Vec<AgentId> {
    let mut children: HashMap<AgentId, Vec<&AgentSummary>> = HashMap::new();
    for summary in summaries {
        if let Some(parent_id) = summary.parent_id {
            children.entry(parent_id).or_default().push(summary);
        }
    }
    for values in children.values_mut() {
        values.sort_by_key(|summary| summary.created_at);
    }

    let mut order = Vec::new();
    push_delete_order(root_id, &children, &mut order);
    order
}

fn push_delete_order(
    agent_id: AgentId,
    children: &HashMap<AgentId, Vec<&AgentSummary>>,
    order: &mut Vec<AgentId>,
) {
    if let Some(child_summaries) = children.get(&agent_id) {
        for child in child_summaries {
            push_delete_order(child.id, children, order);
        }
    }
    order.push(agent_id);
}

fn normalize_reasoning_effort(
    provider_kind: ProviderKind,
    model: &ModelConfig,
    effort: Option<ReasoningEffort>,
    default_when_missing: bool,
) -> Result<Option<ReasoningEffort>> {
    if !model.supports_reasoning {
        return Ok(None);
    }
    match effort {
        Some(ReasoningEffort::None) => Ok(None),
        Some(effort) if supported_reasoning_efforts(provider_kind, model).contains(&effort) => {
            Ok(Some(effort))
        }
        Some(effort) => Err(RuntimeError::InvalidInput(format!(
            "reasoning effort `{}` is not supported by model `{}`",
            reasoning_effort_label(effort),
            model.id
        ))),
        None if default_when_missing => Ok(default_reasoning_effort(provider_kind, model)),
        None => Ok(None),
    }
}

fn supported_reasoning_efforts(
    provider_kind: ProviderKind,
    model: &ModelConfig,
) -> Vec<ReasoningEffort> {
    if !model.supports_reasoning {
        return Vec::new();
    }
    match provider_kind {
        ProviderKind::Deepseek => vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ],
        ProviderKind::Openai => model.reasoning_efforts.clone(),
    }
}

fn default_reasoning_effort(
    provider_kind: ProviderKind,
    model: &ModelConfig,
) -> Option<ReasoningEffort> {
    let supported = supported_reasoning_efforts(provider_kind, model);
    model
        .default_reasoning_effort
        .filter(|effort| supported.contains(effort))
        .or_else(|| supported.first().copied())
}

fn reasoning_effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

fn parse_tool_arguments(raw_arguments: &str) -> Value {
    serde_json::from_str(raw_arguments).unwrap_or_else(|_| json!({ "raw": raw_arguments }))
}

fn trace_preview_value(value: &Value, max: usize) -> String {
    let redacted = redacted_preview_value(value);
    let serialized =
        serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| redacted.to_string());
    preview(&serialized, max)
}

fn trace_preview_output(output: &str, max: usize) -> String {
    serde_json::from_str::<Value>(output)
        .map(|value| trace_preview_value(&value, max))
        .unwrap_or_else(|_| preview(&redact_preview_string(output), max))
}

fn inline_event_arguments(value: &Value) -> Option<Value> {
    let redacted = redacted_preview_value(value);
    let serialized = serde_json::to_string(&redacted).ok()?;
    (serialized.len() <= 2_000).then_some(redacted)
}

fn redacted_preview_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if is_sensitive_key(key) {
                    out.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else {
                    out.insert(key.clone(), redacted_preview_value(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(redacted_preview_value)
                .chain(
                    (items.len() > 20)
                        .then(|| Value::String(format!("<{} more items>", items.len() - 20))),
                )
                .collect(),
        ),
        Value::String(value) => Value::String(redact_preview_string(value)),
        _ => value.clone(),
    }
}

fn redact_preview_string(value: &str) -> String {
    if value.len() > 240 && looks_like_base64(value) {
        return format!("<base64 elided: {} chars>", value.len());
    }
    if value.len() > 800 {
        return format!("{}...", value.chars().take(800).collect::<String>());
    }
    value.to_string()
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("authorization")
        || key.contains("api_key")
        || key.ends_with("_key")
        || key.contains("base64")
}

fn looks_like_base64(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.len() > 240
        && trimmed.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'\n' | b'\r')
        })
}

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}

fn recovered_summary(mut summary: AgentSummary) -> (AgentSummary, bool) {
    let mut changed = false;
    if summary.current_turn.take().is_some() {
        changed = true;
    }
    if matches!(
        summary.status,
        AgentStatus::Created
            | AgentStatus::StartingContainer
            | AgentStatus::RunningTurn
            | AgentStatus::WaitingTool
            | AgentStatus::DeletingContainer
    ) {
        summary.status = AgentStatus::Failed;
        summary.last_error = Some("interrupted by server restart".to_string());
        summary.updated_at = now();
        changed = true;
    }
    (summary, changed)
}

fn default_session_record() -> AgentSessionRecord {
    let now = now();
    AgentSessionRecord {
        summary: AgentSessionSummary {
            id: Uuid::new_v4(),
            title: "Chat 1".to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        },
        messages: Vec::new(),
        history: Vec::new(),
        last_context_tokens: None,
    }
}

fn selected_session(
    sessions: &[AgentSessionRecord],
    session_id: Option<SessionId>,
) -> Option<&AgentSessionRecord> {
    if let Some(session_id) = session_id {
        return sessions
            .iter()
            .find(|session| session.summary.id == session_id);
    }
    sessions
        .iter()
        .max_by(|left, right| {
            left.summary
                .updated_at
                .cmp(&right.summary.updated_at)
                .then_with(|| left.summary.created_at.cmp(&right.summary.created_at))
        })
        .or_else(|| sessions.first())
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

fn should_auto_compact(last_context_tokens: u64, context_tokens: u64) -> bool {
    if last_context_tokens == 0 || context_tokens == 0 {
        return false;
    }
    last_context_tokens.saturating_mul(100)
        >= context_tokens.saturating_mul(AUTO_COMPACT_THRESHOLD_PERCENT)
}

fn compact_summary_from_output(output: &[ModelOutputItem]) -> Option<String> {
    output.iter().rev().find_map(|item| {
        let text = match item {
            ModelOutputItem::Message { text } => text,
            ModelOutputItem::AssistantTurn {
                content: Some(text),
                ..
            } => text,
            _ => return None,
        };
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

fn build_compacted_history(history: &[ModelInputItem], summary: &str) -> Vec<ModelInputItem> {
    let mut replacement = recent_user_messages(history, COMPACT_USER_MESSAGE_MAX_CHARS)
        .into_iter()
        .map(ModelInputItem::user_text)
        .collect::<Vec<_>>();
    replacement.push(ModelInputItem::user_text(compact_summary_message(summary)));
    replacement
}

fn compact_summary_message(summary: &str) -> String {
    format!("{}\n{}", COMPACT_SUMMARY_PREFIX, summary.trim())
}

fn is_compact_summary(text: &str) -> bool {
    text.starts_with(COMPACT_SUMMARY_PREFIX)
}

fn recent_user_messages(history: &[ModelInputItem], max_chars: usize) -> Vec<String> {
    let mut selected = Vec::new();
    let mut remaining = max_chars;
    for item in history.iter().rev() {
        if remaining == 0 {
            break;
        }
        let Some(text) = user_message_text(item) else {
            continue;
        };
        if is_compact_summary(text.trim()) {
            continue;
        }
        if text.chars().count() <= remaining {
            selected.push(text.to_string());
            remaining = remaining.saturating_sub(text.chars().count());
        } else {
            selected.push(take_last_chars(text, remaining));
            break;
        }
    }
    selected.reverse();
    selected
}

fn user_message_text(item: &ModelInputItem) -> Option<&str> {
    let ModelInputItem::Message { role, content } = item else {
        return None;
    };
    if role != "user" {
        return None;
    }
    content.iter().find_map(|item| match item {
        ModelContentItem::InputText { text } => Some(text.as_str()),
        ModelContentItem::OutputText { .. } => None,
    })
}

fn take_last_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn event_agent_id(event: &ServiceEvent) -> Option<AgentId> {
    match &event.kind {
        ServiceEventKind::AgentCreated { agent } | ServiceEventKind::AgentUpdated { agent } => {
            Some(agent.id)
        }
        ServiceEventKind::AgentStatusChanged { agent_id, .. }
        | ServiceEventKind::AgentDeleted { agent_id }
        | ServiceEventKind::TurnStarted { agent_id, .. }
        | ServiceEventKind::TurnCompleted { agent_id, .. }
        | ServiceEventKind::ToolStarted { agent_id, .. }
        | ServiceEventKind::ToolCompleted { agent_id, .. }
        | ServiceEventKind::ContextCompacted { agent_id, .. }
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
- Child agent model selection is controlled by Research Agent settings, falling back to the service default model when unset.
- Use available skills only when explicitly requested by the user or when clearly relevant.
- MCP tools are exposed as ordinary function tools whose names begin with `mcp__`.
- Be concise with final answers and include important file paths or command outputs when they matter.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{
        ModelConfig, ProviderConfig, ProviderKind, ProvidersConfigRequest, ReasoningEffort,
    };
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_model(id: &str) -> ModelConfig {
        ModelConfig {
            id: id.to_string(),
            name: Some(id.to_string()),
            context_tokens: 400_000,
            output_tokens: 128_000,
            supports_tools: true,
            supports_reasoning: true,
            reasoning_efforts: vec![
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            default_reasoning_effort: Some(ReasoningEffort::Medium),
            options: serde_json::Value::Null,
            headers: Default::default(),
        }
    }

    fn non_reasoning_model(id: &str) -> ModelConfig {
        ModelConfig {
            supports_reasoning: false,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
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
                supports_reasoning: true,
                reasoning_efforts: vec![ReasoningEffort::High, ReasoningEffort::Max],
                default_reasoning_effort: Some(ReasoningEffort::High),
                options: serde_json::Value::Null,
                headers: Default::default(),
            }],
            default_model: "deepseek-v4-pro".to_string(),
            enabled: true,
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
                    write_mock_response(&mut stream, response).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
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
        let headers = String::from_utf8_lossy(&buffer[..header_end]);
        let content_length = content_length(&headers);
        while buffer.len() < header_end + content_length {
            let read = stream.read(&mut chunk).await.expect("read request body");
            assert!(read > 0, "mock request closed before body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        serde_json::from_slice(&buffer[header_end..header_end + content_length])
            .expect("request json")
    }

    async fn write_mock_response(stream: &mut tokio::net::TcpStream, response: Value) {
        let body = serde_json::to_string(&response).expect("response json");
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(reply.as_bytes())
            .await
            .expect("write response");
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
            name: "compact-agent".to_string(),
            status: AgentStatus::Idle,
            container_id: container_id.map(ToOwned::to_owned),
            provider_id: "mock".to_string(),
            provider_name: "Mock".to_string(),
            model: "mock-model".to_string(),
            reasoning_effort: Some(ReasoningEffort::Medium),
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
            ConfigStore::open_with_config_path(
                &dir.path().join("runtime.sqlite3"),
                &dir.path().join("config.toml"),
            )
            .await
            .expect("open store"),
        )
    }

    async fn save_agent_with_session(store: &ConfigStore, summary: &AgentSummary) {
        store.save_agent(summary, None).await.expect("save agent");
        save_test_session(store, summary.id, Uuid::new_v4()).await;
    }

    async fn test_runtime(dir: &tempfile::TempDir, store: Arc<ConfigStore>) -> Arc<AgentRuntime> {
        AgentRuntime::new(
            DockerClient::new_with_binary("unused", fake_docker_path(dir)),
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime")
    }

    fn fake_docker_path(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("fake-docker.sh");
        let log_path = fake_docker_log_path(dir);
        let script = format!(
            r#"#!/bin/sh
LOG={}
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
    echo "created-container"
    exit 0
    ;;
  rm|rmi|start|exec)
    echo "$*" >> "$LOG"
    exit 0
    ;;
  *)
    echo "$*" >> "$LOG"
    exit 0
    ;;
esac
"#,
            test_shell_quote(&log_path.to_string_lossy())
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

    fn fake_docker_log_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("fake-docker.log")
    }

    fn fake_docker_log(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(fake_docker_log_path(dir)).unwrap_or_default()
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

    #[test]
    fn descendant_delete_order_deletes_children_before_parents() {
        let parent = Uuid::new_v4();
        let older_child = Uuid::new_v4();
        let younger_child = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        let unrelated = Uuid::new_v4();
        let base = now();
        let summaries = vec![
            test_agent_summary_at(parent, None, base),
            test_agent_summary_at(
                younger_child,
                Some(parent),
                base + chrono::Duration::seconds(2),
            ),
            test_agent_summary_at(
                older_child,
                Some(parent),
                base + chrono::Duration::seconds(1),
            ),
            test_agent_summary_at(
                grandchild,
                Some(older_child),
                base + chrono::Duration::seconds(3),
            ),
            test_agent_summary_at(unrelated, None, base + chrono::Duration::seconds(4)),
        ];

        assert_eq!(
            descendant_delete_order_from_summaries(parent, &summaries),
            vec![grandchild, older_child, younger_child, parent]
        );
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
        let events = runtime.recent_events.lock().await;
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
        assert!(!should_auto_compact(79, 100));
        assert!(should_auto_compact(80, 100));
        assert!(should_auto_compact(330_000, 400_000));
        assert!(!should_auto_compact(80, 0));
    }

    #[test]
    fn compact_summary_uses_last_non_empty_assistant_output() {
        let output = vec![
            ModelOutputItem::Message {
                text: "first".to_string(),
            },
            ModelOutputItem::AssistantTurn {
                content: Some("  second  ".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
            },
        ];

        assert_eq!(
            compact_summary_from_output(&output).as_deref(),
            Some("second")
        );
        assert_eq!(compact_summary_from_output(&[]), None);
    }

    #[test]
    fn compacted_history_keeps_recent_user_messages_and_summary_only() {
        let history = vec![
            ModelInputItem::user_text("first user"),
            ModelInputItem::assistant_text("assistant old"),
            ModelInputItem::user_text(compact_summary_message("old summary")),
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

        let compacted = build_compacted_history(&history, "new summary");
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
                if matches!(&content[0], ModelContentItem::InputText { text } if text.contains("new summary") && is_compact_summary(text))
        ));
    }

    #[test]
    fn recent_user_messages_truncates_from_oldest_side() {
        let history = vec![
            ModelInputItem::user_text("abcdef"),
            ModelInputItem::user_text("ghij"),
        ];

        assert_eq!(recent_user_messages(&history, 7), vec!["def", "ghij"]);
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
            name: "restored".to_string(),
            status: AgentStatus::RunningTurn,
            container_id: Some("old-container".to_string()),
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
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let agents = runtime.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, AgentStatus::Failed);
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
            .publish(ServiceEventKind::AgentStatusChanged {
                agent_id,
                status: AgentStatus::Failed,
            })
            .await;
        let events = runtime.recent_events.lock().await;
        assert_eq!(events.back().expect("event").sequence, 42);
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
            name: "trace".to_string(),
            status: AgentStatus::Completed,
            container_id: None,
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
            ResponsesClient::new(),
            Arc::new(
                ConfigStore::open_with_config_path(&db_path, &config_path)
                    .await
                    .expect("reopen store"),
            ),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
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
    }

    #[tokio::test]
    async fn tool_trace_finds_calls_stored_inside_assistant_turns() {
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
            name: "assistant-turn-trace".to_string(),
            status: AgentStatus::Completed,
            container_id: None,
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
                &ModelInputItem::AssistantTurn {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ModelToolCall {
                        call_id: "call_nested".to_string(),
                        name: "container_exec".to_string(),
                        arguments: r#"{"command":"pwd"}"#.to_string(),
                    }],
                },
            )
            .await
            .expect("save assistant turn");
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
            ResponsesClient::new(),
            Arc::new(
                ConfigStore::open_with_config_path(&db_path, &config_path)
                    .await
                    .expect("reopen store"),
            ),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
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
            .save_session_context_tokens(agent_id, session_id, 80)
            .await
            .expect("save tokens");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");
        let agent = runtime.agent(agent_id).await.expect("agent");

        let compacted = runtime
            .maybe_auto_compact(&agent, agent_id, session_id, Uuid::new_v4())
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
            Some(80)
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
                "usage": { "input_tokens": 78, "output_tokens": 2, "total_tokens": 80 }
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
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
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
            )
            .await
            .expect("turn");

        let requests = requests.lock().await.clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0]["tools"].as_array().expect("first tools").len(),
            8
        );
        assert!(
            requests[1].get("tools").is_none(),
            "compact request should not send tools"
        );
        assert_eq!(
            requests[2]["tools"].as_array().expect("second tools").len(),
            8
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
                    && matches!(&content[0], ModelContentItem::InputText { text } if is_compact_summary(text) && text.contains("summary after tool output"))
        )));
        assert!(
            !session
                .history
                .iter()
                .any(|item| matches!(item, ModelInputItem::FunctionCallOutput { .. }))
        );
        assert_eq!(session.history.last().and_then(user_message_text), None);
        assert!(matches!(
            session.history.last(),
            Some(ModelInputItem::Message { role, content })
                if role == "assistant"
                    && matches!(&content[0], ModelContentItem::OutputText { text } if text == "final answer")
        ));
        assert!(
            runtime
                .recent_events
                .lock()
                .await
                .iter()
                .any(|event| matches!(
                    event.kind,
                    ServiceEventKind::ContextCompacted {
                        tokens_before: 80,
                        ..
                    }
                ))
        );
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
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
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
            )
            .await;

        assert!(result.is_err());
        let events = runtime.recent_events.lock().await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            ServiceEventKind::ToolCompleted {
                call_id,
                tool_name,
                success: true,
                ..
            } if call_id == "call_1" && tool_name == "list_agents"
        )));
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
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let agent = runtime
            .create_agent(CreateAgentRequest {
                name: Some("chat-agent".to_string()),
                provider_id: Some("openai".to_string()),
                model: Some("gpt-5.5".to_string()),
                reasoning_effort: Some(ReasoningEffort::High),
                parent_id: None,
                system_prompt: None,
            })
            .await;
        assert!(
            agent.is_err(),
            "unused docker cannot start, but agent is persisted"
        );
        let agent = runtime.list_agents().await[0].clone();
        assert_eq!(agent.reasoning_effort, Some(ReasoningEffort::High));
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
            Some(80)
        );

        let reopened = store.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(reopened.agents[0].sessions.len(), 2);
        assert_eq!(
            reopened.agents[0].summary.reasoning_effort,
            Some(ReasoningEffort::High)
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
                    name: "deepseek-context".to_string(),
                    status: AgentStatus::Idle,
                    container_id: None,
                    provider_id: "deepseek".to_string(),
                    provider_name: "DeepSeek".to_string(),
                    model: "deepseek-v4-pro".to_string(),
                    reasoning_effort: Some(ReasoningEffort::High),
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
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
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
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let config = runtime.agent_config().await.expect("config");
        assert_eq!(config.research_agent, None);
        let effective = config.effective_research_agent.expect("effective default");
        assert_eq!(effective.provider_id, "alt");
        assert_eq!(effective.model, "alt-default");
        assert_eq!(effective.reasoning_effort, Some(ReasoningEffort::Medium));

        let updated = runtime
            .update_agent_config(AgentConfigRequest {
                research_agent: Some(AgentModelPreference {
                    provider_id: "openai".to_string(),
                    model: "gpt-5.4".to_string(),
                    reasoning_effort: Some(ReasoningEffort::High),
                }),
            })
            .await
            .expect("update");
        assert_eq!(
            updated.effective_research_agent.expect("effective").model,
            "gpt-5.4"
        );

        let invalid = runtime
            .update_agent_config(AgentConfigRequest {
                research_agent: Some(AgentModelPreference {
                    provider_id: "openai".to_string(),
                    model: "gpt-5.4".to_string(),
                    reasoning_effort: Some(ReasoningEffort::Max),
                }),
            })
            .await;
        assert!(matches!(invalid, Err(RuntimeError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn spawn_agent_uses_global_default_when_research_config_missing() {
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
                    name: "parent".to_string(),
                    status: AgentStatus::Idle,
                    container_id: None,
                    provider_id: "openai".to_string(),
                    provider_name: "OpenAI".to_string(),
                    model: "gpt-5.4".to_string(),
                    reasoning_effort: Some(ReasoningEffort::High),
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
        assert_eq!(child.provider_id, "alt");
        assert_eq!(child.model, "alt-default");
        assert_eq!(child.reasoning_effort, Some(ReasoningEffort::Medium));
        let docker_log = fake_docker_log(&dir);
        assert!(docker_log.contains("commit parent-container mai-team-snapshot-"));
        assert!(docker_log.contains(&format!("create --name mai-team-{}", child.id)));
        assert!(docker_log.contains("rmi -f mai-team-snapshot-"));
    }

    #[tokio::test]
    async fn spawn_agent_uses_research_config_over_parent_and_legacy_args() {
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
                research_agent: Some(AgentModelPreference {
                    provider_id: "alt".to_string(),
                    model: "alt-research".to_string(),
                    reasoning_effort: Some(ReasoningEffort::Low),
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
                    name: "parent".to_string(),
                    status: AgentStatus::Idle,
                    container_id: None,
                    provider_id: "openai".to_string(),
                    provider_name: "OpenAI".to_string(),
                    model: "gpt-5.5".to_string(),
                    reasoning_effort: Some(ReasoningEffort::High),
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
        assert_eq!(child.provider_id, "alt");
        assert_eq!(child.model, "alt-research");
        assert_eq!(child.reasoning_effort, Some(ReasoningEffort::Low));
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
            name: "model-switch".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some(ReasoningEffort::Low),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
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
                    reasoning_effort: Some(ReasoningEffort::High),
                },
            )
            .await
            .expect("update");

        assert_eq!(updated.model, "gpt-5.4");
        assert_eq!(updated.reasoning_effort, Some(ReasoningEffort::High));
        let event = events.recv().await.expect("event");
        assert!(matches!(
            event.kind,
            ServiceEventKind::AgentUpdated { agent } if agent.id == agent_id
                && agent.model == "gpt-5.4"
                && agent.reasoning_effort == Some(ReasoningEffort::High)
        ));
        let snapshot = store.load_runtime_snapshot(10).await.expect("snapshot");
        assert_eq!(snapshot.agents[0].summary.model, "gpt-5.4");
        assert_eq!(
            snapshot.agents[0].summary.reasoning_effort,
            Some(ReasoningEffort::High)
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
            name: "reasoning-switch".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some(ReasoningEffort::Medium),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            Arc::clone(&store),
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
        )
        .await
        .expect("runtime");

        let invalid = runtime
            .update_agent(
                agent_id,
                UpdateAgentRequest {
                    provider_id: None,
                    model: Some("gpt-5.4".to_string()),
                    reasoning_effort: Some(ReasoningEffort::Max),
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
                    reasoning_effort: Some(ReasoningEffort::High),
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
            name: "busy".to_string(),
            status: AgentStatus::Idle,
            container_id: None,
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some(ReasoningEffort::Medium),
            created_at: timestamp,
            updated_at: timestamp,
            current_turn: None,
            last_error: None,
            token_usage: TokenUsage::default(),
        };
        store.save_agent(&summary, None).await.expect("save agent");
        let runtime = AgentRuntime::new(
            DockerClient::new("unused"),
            ResponsesClient::new(),
            store,
            RuntimeConfig {
                repo_root: dir.path().to_path_buf(),
            },
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

        let preview = trace_preview_value(&value, 1_000);

        assert!(preview.contains("echo ok"));
        assert!(preview.contains("<redacted>"));
        assert!(!preview.contains("secret"));
        assert!(!preview.contains(&"a".repeat(120)));
    }
}
