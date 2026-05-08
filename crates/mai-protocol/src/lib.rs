use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use uuid::Uuid;

pub type AgentId = Uuid;
pub type SessionId = Uuid;
pub type TaskId = Uuid;
pub type TurnId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Created,
    StartingContainer,
    Idle,
    RunningTurn,
    WaitingTool,
    Completed,
    Failed,
    Cancelled,
    DeletingContainer,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Planning,
    AwaitingApproval,
    Executing,
    Reviewing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Missing,
    Ready,
    NeedsRevision,
    Approved,
}

impl AgentStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Deleted
        )
    }

    pub fn can_start_turn(&self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Completed | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpStartupStatus {
    Starting,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    WaitingTool,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoListStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub step: String,
    pub status: TodoListStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInputOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInputQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<UserInputOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub role: MessageRole,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionSummary {
    pub id: SessionId,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub context_tokens: u64,
    pub threshold_percent: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: AgentId,
    pub parent_id: Option<AgentId>,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub role: Option<AgentRole>,
    pub name: String,
    pub status: AgentStatus,
    pub container_id: Option<String>,
    #[serde(default)]
    pub docker_image: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub current_turn: Option<TurnId>,
    pub last_error: Option<String>,
    pub token_usage: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetail {
    #[serde(flatten)]
    pub summary: AgentSummary,
    pub sessions: Vec<AgentSessionSummary>,
    pub selected_session_id: SessionId,
    #[serde(default)]
    pub context_usage: Option<ContextUsage>,
    pub messages: Vec<AgentMessage>,
    pub recent_events: Vec<ServiceEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    pub status: PlanStatus,
    pub title: Option<String>,
    pub markdown: Option<String>,
    pub version: u64,
    pub saved_by_agent_id: Option<AgentId>,
    pub saved_at: Option<DateTime<Utc>>,
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revision_feedback: Option<String>,
    #[serde(default)]
    pub revision_requested_at: Option<DateTime<Utc>>,
}

impl Default for TaskPlan {
    fn default() -> Self {
        Self {
            status: PlanStatus::Missing,
            title: None,
            markdown: None,
            version: 0,
            saved_by_agent_id: None,
            saved_at: None,
            approved_at: None,
            revision_feedback: None,
            revision_requested_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanHistoryEntry {
    pub version: u64,
    pub title: Option<String>,
    pub markdown: Option<String>,
    pub saved_at: Option<DateTime<Utc>>,
    pub saved_by_agent_id: Option<AgentId>,
    #[serde(default)]
    pub revision_feedback: Option<String>,
    #[serde(default)]
    pub revision_requested_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReview {
    pub id: Uuid,
    pub task_id: TaskId,
    pub reviewer_agent_id: AgentId,
    pub round: u64,
    pub passed: bool,
    pub findings: String,
    pub summary: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
    pub plan_status: PlanStatus,
    pub plan_version: u64,
    pub planner_agent_id: AgentId,
    pub current_agent_id: Option<AgentId>,
    pub agent_count: usize,
    pub review_rounds: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub final_report: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub summary: TaskSummary,
    pub plan: TaskPlan,
    #[serde(default)]
    pub plan_history: Vec<PlanHistoryEntry>,
    pub reviews: Vec<TaskReview>,
    pub agents: Vec<AgentSummary>,
    pub selected_agent_id: AgentId,
    pub selected_agent: AgentDetail,
    #[serde(default)]
    pub artifacts: Vec<ArtifactInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    pub name: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub docker_image: Option<String>,
    pub parent_id: Option<AgentId>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentResponse {
    pub agent: AgentSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateTaskRequest {
    pub title: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub docker_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskResponse {
    pub task: TaskSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveTaskPlanResponse {
    pub task: TaskSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPlanRevisionRequest {
    pub feedback: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPlanRevisionResponse {
    pub task: TaskSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAgentRequest {
    pub provider_id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAgentResponse {
    pub agent: AgentSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    #[serde(default)]
    pub skill_mentions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Repo,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillInterface {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub short_description: Option<String>,
    #[serde(default)]
    pub icon_small: Option<PathBuf>,
    #[serde(default)]
    pub icon_large: Option<PathBuf>,
    #[serde(default)]
    pub brand_color: Option<String>,
    #[serde(default)]
    pub default_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillToolDependency {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillDependencies {
    #[serde(default)]
    pub tools: Vec<SkillToolDependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillPolicy {
    #[serde(default)]
    pub allow_implicit_invocation: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub short_description: Option<String>,
    pub path: PathBuf,
    pub scope: SkillScope,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub interface: Option<SkillInterface>,
    #[serde(default)]
    pub dependencies: Option<SkillDependencies>,
    #[serde(default)]
    pub policy: Option<SkillPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillErrorInfo {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillsListResponse {
    pub roots: Vec<PathBuf>,
    pub skills: Vec<SkillMetadata>,
    pub errors: Vec<SkillErrorInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillsConfigRequest {
    #[serde(default)]
    pub config: Vec<SkillConfigEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillConfigEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session: AgentSessionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceDetail {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub output: String,
    pub success: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUploadRequest {
    pub path: String,
    pub content_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUploadResponse {
    pub path: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub id: String,
    pub agent_id: AgentId,
    pub task_id: TaskId,
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Openai,
    Deepseek,
    Mimo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub context_tokens: u64,
    pub output_tokens: u64,
    #[serde(default = "default_true")]
    pub supports_tools: bool,
    #[serde(default)]
    pub reasoning: Option<ModelReasoningConfig>,
    #[serde(default)]
    pub options: Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelReasoningConfig {
    #[serde(default)]
    pub default_variant: Option<String>,
    #[serde(default)]
    pub variants: Vec<ModelReasoningVariant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelReasoningVariant {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub request: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default)]
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    pub default_model: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderSummary {
    pub id: String,
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub models: Vec<ModelConfig>,
    pub default_model: String,
    pub enabled: bool,
    pub has_api_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProvidersResponse {
    pub providers: Vec<ProviderSummary>,
    pub default_provider_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProvidersConfigRequest {
    pub providers: Vec<ProviderConfig>,
    pub default_provider_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct McpServersConfigRequest {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentModelPreference {
    pub provider_id: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Planner,
    Explorer,
    #[default]
    Executor,
    Reviewer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedAgentModelPreference {
    pub provider_id: String,
    pub provider_name: String,
    pub provider_kind: ProviderKind,
    pub model: String,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub context_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfigRequest {
    #[serde(default)]
    pub planner: Option<AgentModelPreference>,
    #[serde(default)]
    pub explorer: Option<AgentModelPreference>,
    #[serde(default)]
    pub executor: Option<AgentModelPreference>,
    #[serde(default)]
    pub reviewer: Option<AgentModelPreference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfigResponse {
    #[serde(default)]
    pub planner: Option<AgentModelPreference>,
    #[serde(default)]
    pub explorer: Option<AgentModelPreference>,
    #[serde(default)]
    pub executor: Option<AgentModelPreference>,
    #[serde(default)]
    pub reviewer: Option<AgentModelPreference>,
    #[serde(default)]
    pub effective_planner: Option<ResolvedAgentModelPreference>,
    #[serde(default)]
    pub effective_explorer: Option<ResolvedAgentModelPreference>,
    #[serde(default)]
    pub effective_executor: Option<ResolvedAgentModelPreference>,
    #[serde(default)]
    pub effective_reviewer: Option<ResolvedAgentModelPreference>,
    #[serde(default)]
    pub validation_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderPreset {
    pub id: String,
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderPresetsResponse {
    pub providers: Vec<ProviderPreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderSecret {
    pub id: String,
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: Option<String>,
    pub models: Vec<ModelConfig>,
    pub default_model: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEvent {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: ServiceEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceEventKind {
    AgentCreated {
        agent: AgentSummary,
    },
    AgentStatusChanged {
        agent_id: AgentId,
        status: AgentStatus,
    },
    AgentUpdated {
        agent: AgentSummary,
    },
    AgentDeleted {
        agent_id: AgentId,
    },
    TaskCreated {
        task: TaskSummary,
    },
    TaskUpdated {
        task: TaskSummary,
    },
    TaskDeleted {
        task_id: TaskId,
    },
    TurnStarted {
        agent_id: AgentId,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: TurnId,
    },
    TurnCompleted {
        agent_id: AgentId,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: TurnId,
        status: TurnStatus,
    },
    ToolStarted {
        agent_id: AgentId,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: TurnId,
        call_id: String,
        tool_name: String,
        #[serde(default)]
        arguments_preview: Option<String>,
        #[serde(default)]
        arguments: Option<Value>,
    },
    ToolCompleted {
        agent_id: AgentId,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: TurnId,
        call_id: String,
        tool_name: String,
        success: bool,
        output_preview: String,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
    ContextCompacted {
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        tokens_before: u64,
        summary_preview: String,
    },
    AgentMessage {
        agent_id: AgentId,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: Option<TurnId>,
        role: MessageRole,
        content: String,
    },
    Error {
        agent_id: Option<AgentId>,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: Option<TurnId>,
        message: String,
    },
    TodoListUpdated {
        agent_id: AgentId,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: TurnId,
        items: Vec<TodoItem>,
    },
    PlanUpdated {
        task_id: TaskId,
        plan: TaskPlan,
    },
    UserInputRequested {
        agent_id: AgentId,
        #[serde(default)]
        session_id: Option<SessionId>,
        turn_id: TurnId,
        header: String,
        questions: Vec<UserInputQuestion>,
    },
    ArtifactCreated {
        artifact: ArtifactInfo,
    },
    McpServerStatusChanged {
        agent_id: AgentId,
        server: String,
        status: McpStartupStatus,
        #[serde(default)]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelInputItem {
    Message {
        role: String,
        content: Vec<ModelContentItem>,
    },
    AssistantTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ModelToolCall>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

impl ModelInputItem {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::Message {
            role: "user".to_string(),
            content: vec![ModelContentItem::InputText { text: text.into() }],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::Message {
            role: "assistant".to_string(),
            content: vec![ModelContentItem::OutputText { text: text.into() }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContentItem {
    InputText { text: String },
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelOutputItem {
    Message {
        text: String,
    },
    AssistantTurn {
        content: Option<String>,
        reasoning_content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ModelOutputToolCall>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: Value,
        raw_arguments: String,
    },
    Other {
        raw: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOutputToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub raw_arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub id: Option<String>,
    pub output: Vec<ModelOutputItem>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerTransport {
    #[default]
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    #[serde(default)]
    pub transport: McpServerTransport,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub bearer_token: Option<String>,
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub startup_timeout_secs: Option<u64>,
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            transport: McpServerTransport::Stdio,
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            url: None,
            headers: std::collections::BTreeMap::new(),
            bearer_token: None,
            bearer_token_env: None,
            enabled: true,
            required: false,
            startup_timeout_secs: None,
            tool_timeout_secs: None,
            enabled_tools: None,
            disabled_tools: Vec::new(),
        }
    }
}

pub fn default_true() -> bool {
    true
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn preview(value: &str, max: usize) -> String {
    let mut out = value.replace('\n', "\\n");
    if out.len() > max {
        let boundary = out
            .char_indices()
            .take_while(|(i, c)| i + c.len_utf8() <= max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        out.truncate(boundary);
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_agent_request_accepts_missing_docker_image() {
        let request: CreateAgentRequest = serde_json::from_value(json!({
            "name": "agent",
            "provider_id": "openai",
            "model": "gpt-5.5"
        }))
        .expect("request");

        assert_eq!(request.docker_image, None);
    }
}
