use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use uuid::Uuid;

mod agent_state;

pub use agent_state::{
    AgentLastTurn, AgentResourceState, AgentRuntimeActivity, AgentRuntimeLifecycle,
    AgentRuntimeState, AgentState, AgentTurnOutcomeKind,
};
pub use pl_protocol::{
    CredentialDescriptorDto, ErrorSeverity, McpAvailabilityDescriptor, McpHealthSnapshot,
    McpServerDescriptor, ModelCapabilitiesDto, ModelCatalogDescriptor, ModelDescriptor,
    ModelPricingDto, ModelReasoningDescriptor, PROVIDER_CATALOG_SCHEMA_VERSION,
    ProviderCatalogSnapshot, ProviderConnectionModeDescriptor, ProviderPresetDescriptor,
    ProviderServiceCapabilitiesDescriptor, SESSION_EVENT_SCHEMA_VERSION, SessionAgentPart,
    SessionAgentSnapshot, SessionAttachment, SessionContextCompaction, SessionEventEnvelope,
    SessionEventKind, SessionEventPosition, SessionMessage, SessionMessageRole,
    SessionMessageStatus, SessionPart, SessionPartContent, SessionPartDelta, SessionPartDeltaField,
    SessionPartStatus, SessionResyncReason, SessionRuntimeSnapshot, SessionRuntimeUsage,
    SessionStreamFrame, SessionSubscriptionRequest, SessionTextChannel, SessionTimelineEvent,
    SessionTimelineEventKind, SessionToolPart, SessionTurn, SessionTurnStatus, SessionViewSnapshot,
    WebSearchProviderCapabilitiesDescriptor, WebSearchResolutionDescriptor,
    session_events_typescript,
};

pub type AgentId = Uuid;
pub type EnvironmentId = Uuid;
pub type ProjectId = Uuid;
pub type SessionId = Uuid;
pub type TaskId = Uuid;
pub type TurnId = Uuid;

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TaskStatus {
    Planning,
    AwaitingApproval,
    Executing,
    Reviewing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectStatus {
    Creating,
    Ready,
    Failed,
    Deleting,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectCloneStatus {
    Pending,
    Cloning,
    Ready,
    Failed,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString, Default,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReviewStatus {
    #[default]
    #[strum(serialize = "disabled", serialize = "")]
    Disabled,
    Idle,
    Selecting,
    Syncing,
    Running,
    Waiting,
    Queued,
    Preparing,
    RetryWaiting,
    Reconciling,
    Failed,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReviewOutcome {
    ReviewSubmitted,
    NoEligiblePr,
    Failed,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReviewDecision {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReviewRunStatus {
    Syncing,
    Running,
    Completed,
    Failed,
    Succeeded,
    RetryableFailed,
    PermanentFailed,
    Interrupted,
    Cancelled,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReviewJobStatus {
    Queued,
    Preparing,
    Running,
    RetryWaiting,
    SubmissionPending,
    Reconciling,
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}

impl ProjectReviewJobStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Superseded
        )
    }
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReviewJobSource {
    Automatic,
    Webhook,
    Manual,
    Legacy,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectReviewFailureCategory {
    Provider,
    ProviderCapacity,
    Github,
    Workspace,
    Validation,
    Timeout,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectReviewFailure {
    pub category: ProjectReviewFailureCategory,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub http_status: Option<u16>,
    pub message: String,
    pub retry: pl_protocol::RetryDisposition,
}

impl From<pl_protocol::TurnFailure> for ProjectReviewFailure {
    fn from(failure: pl_protocol::TurnFailure) -> Self {
        let category = match failure.category {
            pl_protocol::TurnFailureCategory::Provider => ProjectReviewFailureCategory::Provider,
            pl_protocol::TurnFailureCategory::ProviderCapacity => {
                ProjectReviewFailureCategory::ProviderCapacity
            }
            pl_protocol::TurnFailureCategory::Tool => ProjectReviewFailureCategory::Internal,
            pl_protocol::TurnFailureCategory::Validation => {
                ProjectReviewFailureCategory::Validation
            }
            pl_protocol::TurnFailureCategory::Internal => ProjectReviewFailureCategory::Internal,
        };
        Self {
            category,
            code: failure.code,
            http_status: failure.http_status,
            message: failure.message,
            retry: failure.retry,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectReviewSubmissionIntent {
    pub job_id: Uuid,
    pub head_sha: String,
    pub event: ProjectReviewDecision,
    pub body_hash: String,
    pub comment_count: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectReviewSubmissionReceipt {
    pub github_review_id: u64,
    pub event: ProjectReviewDecision,
    pub head_sha: String,
    #[serde(default)]
    pub html_url: Option<String>,
    pub submitted_at: DateTime<Utc>,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PlanStatus {
    Missing,
    Ready,
    NeedsRevision,
    Approved,
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

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    #[serde(default)]
    pub token_usage: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: AgentId,
    pub parent_id: Option<AgentId>,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub role: Option<AgentRole>,
    pub name: String,
    pub state: AgentState,
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
    pub token_usage: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetail {
    #[serde(flatten)]
    pub summary: AgentSummary,
    pub sessions: Vec<AgentSessionSummary>,
    pub selected_session_id: SessionId,
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
pub struct EnvironmentSummary {
    pub id: EnvironmentId,
    pub name: String,
    pub status: TaskStatus,
    pub root_agent_id: AgentId,
    pub conversation_count: usize,
    pub docker_image: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentDetail {
    #[serde(flatten)]
    pub summary: EnvironmentSummary,
    pub root_agent: AgentDetail,
    pub current_conversation_id: SessionId,
    #[serde(default)]
    pub selected_conversation_id: Option<SessionId>,
    pub conversations: Vec<AgentSessionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: ProjectId,
    pub name: String,
    pub status: ProjectStatus,
    pub owner: String,
    pub repo: String,
    #[serde(default)]
    pub repository_full_name: String,
    #[serde(default)]
    pub git_account_id: Option<String>,
    pub repository_id: u64,
    pub installation_id: u64,
    pub installation_account: String,
    #[serde(default)]
    pub branch: String,
    pub docker_image: String,
    pub clone_status: ProjectCloneStatus,
    pub maintainer_agent_id: AgentId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub auto_review_enabled: bool,
    #[serde(default)]
    pub reviewer_extra_prompt: Option<String>,
    #[serde(default)]
    pub review_status: ProjectReviewStatus,
    #[serde(default)]
    pub current_reviewer_agent_id: Option<AgentId>,
    #[serde(default)]
    pub last_review_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_review_finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_review_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_review_outcome: Option<ProjectReviewOutcome>,
    #[serde(default)]
    pub review_last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReviewRunSummary {
    pub id: Uuid,
    #[serde(default)]
    pub job_id: Option<Uuid>,
    #[serde(default)]
    pub attempt_index: u32,
    pub project_id: ProjectId,
    #[serde(default)]
    pub reviewer_agent_id: Option<AgentId>,
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    pub status: ProjectReviewRunStatus,
    #[serde(default)]
    pub outcome: Option<ProjectReviewOutcome>,
    #[serde(default)]
    pub review_event: Option<ProjectReviewDecision>,
    #[serde(default)]
    pub pr: Option<u64>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub failure: Option<ProjectReviewFailure>,
    #[serde(default)]
    pub token_usage: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectReviewJobSummary {
    pub id: Uuid,
    pub project_id: ProjectId,
    pub pr: u64,
    pub head_sha: String,
    pub source: ProjectReviewJobSource,
    #[serde(default)]
    pub delivery_id: Option<String>,
    pub reason: String,
    pub status: ProjectReviewJobStatus,
    pub attempt_count: u32,
    pub max_attempts: u32,
    #[serde(default)]
    pub first_retryable_failure_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reviewer_agent_id: Option<AgentId>,
    #[serde(default)]
    pub active_run_id: Option<Uuid>,
    #[serde(default)]
    pub lease_owner: Option<String>,
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub failure: Option<ProjectReviewFailure>,
    #[serde(default)]
    pub submission_intent: Option<ProjectReviewSubmissionIntent>,
    #[serde(default)]
    pub submission_receipt: Option<ProjectReviewSubmissionReceipt>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReviewJobDetail {
    #[serde(flatten)]
    pub summary: ProjectReviewJobSummary,
    #[serde(default)]
    pub attempts: Vec<ProjectReviewRunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReviewJobsResponse {
    pub jobs: Vec<ProjectReviewJobSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReviewRunDetail {
    #[serde(flatten)]
    pub summary: ProjectReviewRunSummary,
    #[serde(default)]
    pub messages: Vec<AgentMessage>,
    #[serde(default)]
    pub events: Vec<SessionEventEnvelope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReviewRunsResponse {
    pub runs: Vec<ProjectReviewRunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectReviewQueueResponse {
    pub queued: Vec<u64>,
    pub deduped: Vec<u64>,
    pub ignored: Vec<u64>,
    #[serde(default)]
    pub jobs: Vec<ProjectReviewJobSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub summary: ProjectSummary,
    pub maintainer_agent: AgentDetail,
    pub agents: Vec<AgentSummary>,
    pub selected_agent_id: AgentId,
    pub selected_agent: AgentDetail,
    #[serde(default)]
    pub auth_status: String,
    #[serde(default)]
    pub mcp_status: String,
    #[serde(default)]
    pub review_runs: Vec<ProjectReviewRunSummary>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateEnvironmentRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub docker_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEnvironmentResponse {
    pub environment: EnvironmentSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateProjectRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub git_account_id: Option<String>,
    #[serde(default)]
    pub installation_id: u64,
    #[serde(default)]
    pub repository_id: u64,
    #[serde(default)]
    pub repository_full_name: Option<String>,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub docker_image: Option<String>,
    #[serde(default)]
    pub auto_review_enabled: bool,
    #[serde(default)]
    pub reviewer_extra_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProjectRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub docker_image: Option<String>,
    #[serde(default)]
    pub auto_review_enabled: Option<bool>,
    #[serde(default)]
    pub reviewer_extra_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectResponse {
    pub project: ProjectSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProjectResponse {
    pub project: ProjectSummary,
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
    #[serde(default)]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Project,
    Repo,
    User,
    System,
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
    #[serde(default)]
    pub source_path: Option<PathBuf>,
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
pub struct SkillActivationInfo {
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub path: PathBuf,
    pub scope: SkillScope,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AgentProfileScope {
    Project,
    Repo,
    User,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub spawn_agents: bool,
    #[serde(default)]
    pub close_agents: bool,
    #[serde(default)]
    pub communication: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub slot: String,
    pub version: u64,
    pub path: PathBuf,
    #[serde(default)]
    pub source_path: Option<PathBuf>,
    pub scope: AgentProfileScope,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub prompt: String,
    #[serde(default)]
    pub default_model_role: Option<String>,
    #[serde(default)]
    pub default_skills: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentProfileSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub slot: String,
    pub version: u64,
    pub path: PathBuf,
    #[serde(default)]
    pub source_path: Option<PathBuf>,
    pub scope: AgentProfileScope,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub default_model_role: Option<String>,
    #[serde(default)]
    pub default_skills: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    pub hash: String,
}

impl From<&AgentProfile> for AgentProfileSummary {
    fn from(profile: &AgentProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            description: profile.description.clone(),
            slot: profile.slot.clone(),
            version: profile.version,
            path: profile.path.clone(),
            source_path: profile.source_path.clone(),
            scope: profile.scope,
            enabled: profile.enabled,
            default_model_role: profile.default_model_role.clone(),
            default_skills: profile.default_skills.clone(),
            mcp_servers: profile.mcp_servers.clone(),
            capabilities: profile.capabilities.clone(),
            hash: profile.hash.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentProfileErrorInfo {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentProfilesResponse {
    pub roots: Vec<PathBuf>,
    pub profiles: Vec<AgentProfileSummary>,
    pub errors: Vec<AgentProfileErrorInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session: AgentSessionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLogEntry {
    pub id: Uuid,
    pub agent_id: AgentId,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    pub level: String,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub details: Value,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLogsResponse {
    pub logs: Vec<AgentLogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceSummary {
    pub call_id: String,
    pub agent_id: AgentId,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    pub tool_name: String,
    pub success: bool,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub output_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceListResponse {
    pub tool_calls: Vec<ToolTraceSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceDetail {
    pub agent_id: AgentId,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub output: String,
    pub success: bool,
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    pub output_preview: String,
    #[serde(default)]
    pub output_artifacts: Vec<ToolOutputArtifactInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutputArtifactInfo {
    pub id: String,
    pub call_id: String,
    pub agent_id: AgentId,
    pub name: String,
    pub stream: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
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
pub enum ProviderWireProtocol {
    #[default]
    Responses,
    ChatCompletions,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnectionMode {
    WebSocket,
    #[default]
    Http,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderTransportConfig {
    pub protocol: ProviderWireProtocol,
    pub connection_mode: ProviderConnectionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderTransportSummary {
    pub protocol: ProviderWireProtocol,
    pub connection_mode: ProviderConnectionMode,
    pub connection_modes: Vec<ProviderConnectionModeDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub context_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    #[serde(default = "default_effective_context_window_percent")]
    pub effective_context_window_percent: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<u64>,
    #[serde(default = "default_true")]
    pub supports_tools: bool,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub request_policy: ModelRequestPolicy,
    #[serde(default)]
    pub reasoning: Option<ModelReasoningConfig>,
    #[serde(default)]
    pub options: Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl ModelConfig {
    pub fn effective_context_tokens(&self) -> u64 {
        let percent = self.effective_context_window_percent.min(100);
        let context_tokens = if self.context_tokens > 0 {
            Some(self.context_tokens)
        } else {
            self.max_context_tokens
        };
        context_tokens
            .map(|value| value.saturating_mul(percent) / 100)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCapabilities {
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default)]
    pub parallel_tools: bool,
    #[serde(default)]
    pub reasoning_replay: bool,
    #[serde(default)]
    pub strict_schema: bool,
    #[serde(default)]
    pub web_search: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            tools: true,
            parallel_tools: false,
            reasoning_replay: false,
            strict_schema: false,
            web_search: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSchemaPolicy {
    #[default]
    Standard,
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRequestPolicy {
    #[serde(default)]
    pub max_tokens_field: ModelMaxTokensField,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub tool_schema: ToolSchemaPolicy,
    #[serde(default, skip_serializing_if = "is_null")]
    pub extra_body: Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl Default for ModelRequestPolicy {
    fn default() -> Self {
        Self {
            max_tokens_field: ModelMaxTokensField::default(),
            store: None,
            tool_schema: ToolSchemaPolicy::Standard,
            extra_body: Value::Null,
            headers: BTreeMap::new(),
        }
    }
}

/// 模型请求允许使用的输出 token 字段。
///
/// `Omit` 是有意义的协议策略，不能退化成 Chat Completions 的默认字段。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelMaxTokensField {
    Omit,
    MaxOutputTokens,
    MaxCompletionTokens,
    #[default]
    MaxTokens,
}

fn default_effective_context_window_percent() -> u64 {
    95
}

fn is_null(value: &Value) -> bool {
    value.is_null()
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
    pub preset_id: Option<String>,
    #[serde(default)]
    pub transport: ProviderTransportConfig,
    pub capabilities: ProviderCapabilitySelection,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// 只写的 provider 请求头；实例边界未变化时，`None` 表示由服务端保留原值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_headers: Option<BTreeMap<String, String>>,
    pub catalog: ProviderModelCatalogConfig,
    pub default_model: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Provider 实例服务能力的配置来源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProviderCapabilitySelection {
    PresetDefaults,
    Explicit(ProviderServiceCapabilitiesDescriptor),
}

/// mai HTTP 边界中的 provider 模型目录引用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProviderModelCatalogConfig {
    Bundled {
        catalog_id: String,
        #[serde(default)]
        additional_models: Vec<ModelConfig>,
    },
    Explicit {
        #[serde(default)]
        models: Vec<ModelConfig>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderSummary {
    pub id: String,
    pub preset_id: Option<String>,
    pub transport: ProviderTransportSummary,
    pub capability_selection: ProviderCapabilitySelection,
    pub service_capabilities: ProviderServiceCapabilitiesDescriptor,
    pub name: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub catalog: ProviderModelCatalogConfig,
    /// 服务端通过 PL 目录解析后的唯一有效模型列表。
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderTestRequest {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    #[serde(default = "default_true")]
    pub deep: bool,
}

impl Default for ProviderTestRequest {
    fn default() -> Self {
        Self {
            model: None,
            reasoning_effort: None,
            deep: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderTestResponse {
    pub ok: bool,
    pub provider_id: String,
    pub provider_name: String,
    pub transport: ProviderTransportConfig,
    pub model: String,
    pub base_url: String,
    pub latency_ms: u64,
    pub output_preview: String,
    pub usage: Option<TokenUsage>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct McpServersConfigRequest {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
    /// 需要显式删除的 write-only secret；空值本身只表示保留已有值。
    #[serde(default)]
    pub clear_secrets: BTreeMap<String, McpServerSecretClearRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct McpServerSecretClearRequest {
    #[serde(default)]
    pub bearer_token: bool,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub headers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentModelPreference {
    pub provider_id: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
    pub transport: ProviderTransportConfig,
    pub model: String,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub context_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    #[serde(default = "default_effective_context_window_percent")]
    pub effective_context_window_percent: u64,
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
pub struct ProviderSecret {
    pub id: String,
    pub transport: ProviderTransportConfig,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub http_headers: BTreeMap<String, String>,
    pub models: Vec<ModelConfig>,
    pub default_model: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaiProductEventEnvelope {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: MaiProductEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MaiProductEventKind {
    AgentCreated {
        agent: AgentSummary,
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
    ProjectCreated {
        project: ProjectSummary,
    },
    ProjectUpdated {
        project: ProjectSummary,
    },
    ProjectDeleted {
        project_id: ProjectId,
    },
    GithubWebhookReceived {
        delivery_id: String,
        event: String,
        action: Option<String>,
        repository_full_name: Option<String>,
        installation_id: Option<u64>,
    },
    ProjectReviewQueued {
        project_id: ProjectId,
        delivery_id: String,
        pr: u64,
        reason: String,
    },
    OperationFailed {
        scope: String,
        agent_id: Option<AgentId>,
        message: String,
    },
    PlanUpdated {
        task_id: TaskId,
        plan: TaskPlan,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelOutputItem {
    Message {
        text: String,
    },
    Reasoning {
        content: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerScope {
    #[default]
    Agent,
    Project,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    #[serde(default)]
    pub scope: McpServerScope,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSearchSettings {
    pub mode: String,
    #[serde(default)]
    pub context_size: Option<String>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub location: Option<WebSearchLocationSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSearchLocationSettings {
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSearchSettingsResponse {
    pub config: WebSearchSettings,
    pub roles: BTreeMap<String, WebSearchResolutionDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerPublicConfig {
    pub scope: McpServerScope,
    pub enabled: bool,
    pub required: bool,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub header_names: Vec<String>,
    pub bearer_token_env: Option<String>,
    pub has_bearer_token: bool,
    pub startup_timeout_secs: Option<u64>,
    pub tool_timeout_secs: Option<u64>,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerAggregate {
    pub descriptor: McpServerDescriptor,
    pub enabled: bool,
    pub availability: String,
    pub ready_agents: usize,
    pub failed_agents: usize,
    pub checking_agents: usize,
    pub total_agents: usize,
    pub tool_count: usize,
    pub config: Option<McpServerPublicConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServersResponse {
    pub servers: Vec<McpServerAggregate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuiltinMcpServersRequest {
    pub servers: BTreeMap<String, bool>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            scope: McpServerScope::Agent,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubSettingsResponse {
    pub has_token: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubSettingsRequest {
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GithubAppSettingsResponse {
    pub app_id: Option<String>,
    pub base_url: String,
    #[serde(default)]
    pub public_url: Option<String>,
    pub has_private_key: bool,
    #[serde(default)]
    pub app_slug: Option<String>,
    #[serde(default)]
    pub app_html_url: Option<String>,
    #[serde(default)]
    pub owner_login: Option<String>,
    #[serde(default)]
    pub owner_type: Option<String>,
    #[serde(default)]
    pub install_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAppInstallationStartRequest {
    pub origin: String,
    #[serde(default)]
    pub return_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAppInstallationStartResponse {
    pub state: String,
    pub install_url: String,
    pub app: GithubAppSettingsResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GithubAppSettingsRequest {
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub private_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub app_slug: Option<String>,
    #[serde(default)]
    pub app_html_url: Option<String>,
    #[serde(default)]
    pub owner_login: Option<String>,
    #[serde(default)]
    pub owner_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelaySettingsRequest {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelaySettingsResponse {
    pub enabled: bool,
    pub url: String,
    pub has_token: bool,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAppManifestStartRequest {
    pub origin: String,
    pub account_type: GithubAppManifestAccountType,
    #[serde(default)]
    pub org: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubAppManifestAccountType {
    Personal,
    Organization,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAppManifestStartResponse {
    pub state: String,
    pub action_url: String,
    pub manifest: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubInstallationSummary {
    pub id: u64,
    pub account_login: String,
    pub account_type: String,
    #[serde(default)]
    pub repository_selection: Option<String>,
    #[serde(default)]
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubInstallationsResponse {
    pub installations: Vec<GithubInstallationSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRepositorySummary {
    pub id: u64,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub private: bool,
    pub clone_url: String,
    pub html_url: String,
    #[serde(default)]
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRepositoriesResponse {
    pub repositories: Vec<GithubRepositorySummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDefaultsResponse {
    pub default_docker_image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepositoryPackagesResponse {
    #[serde(default)]
    pub packages: Vec<RepositoryPackageSummary>,
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryPackageSummary {
    pub name: String,
    pub image: String,
    pub tag: String,
    pub html_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitProvider {
    #[default]
    Github,
    GithubAppRelay,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitTokenKind {
    Classic,
    FineGrainedPat,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitAccountStatus {
    #[default]
    Unverified,
    Verifying,
    Verified,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitAccountSummary {
    pub id: String,
    #[serde(default)]
    pub provider: GitProvider,
    pub label: String,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub token_kind: GitTokenKind,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub status: GitAccountStatus,
    #[serde(default)]
    pub is_default: bool,
    pub has_token: bool,
    #[serde(default)]
    pub last_verified_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub installation_id: Option<u64>,
    #[serde(default)]
    pub installation_account: Option<String>,
    #[serde(default)]
    pub relay_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitAccountsResponse {
    #[serde(default)]
    pub accounts: Vec<GitAccountSummary>,
    #[serde(default)]
    pub default_account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitAccountRequest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub provider: GitProvider,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub installation_id: Option<u64>,
    #[serde(default)]
    pub installation_account: Option<String>,
    #[serde(default)]
    pub relay_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitAccountResponse {
    pub account: GitAccountSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitAccountDefaultRequest {
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayClientHello {
    pub node_id: String,
    pub version: String,
    pub token: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayResponse {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RelayError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayEvent {
    pub sequence: u64,
    pub delivery_id: String,
    pub kind: RelayEventKind,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelayEventKind {
    PullRequest,
    Push,
    CheckRun,
    CheckSuite,
    Installation,
    InstallationRepositories,
    Other(String),
}

impl RelayEventKind {
    pub fn from_github_event(value: &str) -> Self {
        match value {
            "pull_request" => Self::PullRequest,
            "push" => Self::Push,
            "check_run" => Self::CheckRun,
            "check_suite" => Self::CheckSuite,
            "installation" => Self::Installation,
            "installation_repositories" => Self::InstallationRepositories,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_github_event(&self) -> &str {
        match self {
            Self::PullRequest => "pull_request",
            Self::Push => "push",
            Self::CheckRun => "check_run",
            Self::CheckSuite => "check_suite",
            Self::Installation => "installation",
            Self::InstallationRepositories => "installation_repositories",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayAck {
    pub delivery_id: String,
    pub status: RelayAckStatus,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelayAckStatus {
    Processed,
    Ignored,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayEnvelope {
    Hello(RelayClientHello),
    Request(RelayRequest),
    Response(RelayResponse),
    Event(RelayEvent),
    Ack(RelayAck),
    Ping { id: String },
    Pong { id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RelayStatusResponse {
    pub enabled: bool,
    pub connected: bool,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub queued_deliveries: Option<u64>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelayUpdateReleaseInfo {
    pub name: String,
    pub body: String,
    pub published_at: String,
    pub html_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelayUpdateStatusResponse {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub can_update: bool,
    #[serde(default)]
    pub release: Option<RelayUpdateReleaseInfo>,
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub restart_scheduled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelayUpdateActionResponse {
    pub status: RelayUpdateStatusResponse,
    pub message: String,
    pub restart_scheduled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelayUpdateCheckRequest {
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelayUpdateApplyRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelayUpdateRollbackRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RelayUpdateRestartRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayGithubRepositoriesRequest {
    pub installation_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayGithubRepositoryGetRequest {
    pub installation_id: u64,
    pub repository_full_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayGithubInstallationTokenRequest {
    pub installation_id: u64,
    #[serde(default)]
    pub repository_id: Option<u64>,
    #[serde(default)]
    pub include_packages: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayGithubInstallationTokenResponse {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayGithubRepositoryPackagesRequest {
    pub installation_id: u64,
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAppInstallationPackagesRequest {
    pub installation_id: u64,
    pub owner: String,
    pub repo: String,
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
    use pretty_assertions::assert_eq;
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

    #[test]
    fn provider_test_request_accepts_empty_body() {
        let request: ProviderTestRequest = serde_json::from_value(json!({})).expect("request");

        assert_eq!(request.model, None);
        assert_eq!(request.reasoning_effort, None);
        assert!(request.deep);
    }

    #[test]
    fn provider_wire_protocol_serializes_canonical_names() {
        assert_eq!(
            serde_json::to_string(&ProviderWireProtocol::Responses).expect("serialize"),
            "\"responses\""
        );
        assert_eq!(
            serde_json::from_str::<ProviderWireProtocol>("\"chat_completions\"")
                .expect("deserialize"),
            ProviderWireProtocol::ChatCompletions
        );
    }

    #[test]
    fn provider_transport_keeps_protocol_and_connection_orthogonal() {
        let transport = ProviderTransportConfig {
            protocol: ProviderWireProtocol::Responses,
            connection_mode: ProviderConnectionMode::WebSocket,
        };

        assert_eq!(
            serde_json::to_value(transport).expect("serialize provider transport"),
            json!({ "protocol": "responses", "connection_mode": "web_socket" })
        );
    }

    #[test]
    fn project_review_queue_response_serializes_queue_summary() {
        let response = ProjectReviewQueueResponse {
            queued: vec![7],
            deduped: vec![8],
            ignored: vec![9],
            jobs: Vec::new(),
        };

        assert_eq!(
            json!({
                "queued": [7],
                "deduped": [8],
                "ignored": [9],
                "jobs": [],
            }),
            serde_json::to_value(response).expect("serialize response")
        );
    }

    #[test]
    fn project_review_job_round_trip_preserves_failure_intent_and_receipt() {
        let created_at = DateTime::parse_from_rfc3339("2026-07-22T00:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let job_id = Uuid::new_v4();
        let job = ProjectReviewJobSummary {
            id: job_id,
            project_id: Uuid::new_v4(),
            pr: 1665,
            head_sha: "abc123".to_string(),
            source: ProjectReviewJobSource::Webhook,
            delivery_id: Some("delivery-1".to_string()),
            reason: "pull_request_synchronize".to_string(),
            status: ProjectReviewJobStatus::Reconciling,
            attempt_count: 2,
            max_attempts: 5,
            first_retryable_failure_at: Some(created_at),
            next_attempt_at: None,
            reviewer_agent_id: Some(Uuid::new_v4()),
            active_run_id: Some(Uuid::new_v4()),
            lease_owner: Some("mai-review:1".to_string()),
            lease_expires_at: Some(created_at),
            failure: Some(ProjectReviewFailure {
                category: ProjectReviewFailureCategory::ProviderCapacity,
                code: Some("server_is_overloaded".to_string()),
                http_status: Some(503),
                message: "provider overloaded".to_string(),
                retry: pl_protocol::RetryDisposition::Retryable {
                    retry_after_ms: Some(30_000),
                },
            }),
            submission_intent: Some(ProjectReviewSubmissionIntent {
                job_id,
                head_sha: "abc123".to_string(),
                event: ProjectReviewDecision::Approve,
                body_hash: "body-hash".to_string(),
                comment_count: 0,
                created_at,
            }),
            submission_receipt: Some(ProjectReviewSubmissionReceipt {
                github_review_id: 42,
                event: ProjectReviewDecision::Approve,
                head_sha: "abc123".to_string(),
                html_url: Some("https://github.example/review/42".to_string()),
                submitted_at: created_at,
            }),
            created_at,
            updated_at: created_at,
            finished_at: None,
        };

        let decoded: ProjectReviewJobSummary =
            serde_json::from_value(serde_json::to_value(&job).expect("serialize job"))
                .expect("deserialize job");

        assert_eq!(decoded, job);
    }

    #[test]
    fn create_project_request_accepts_git_account_repository_only() {
        let request: CreateProjectRequest = serde_json::from_value(json!({
            "name": "Mai Team",
            "git_account_id": "account-1",
            "repository_full_name": "owner/repo",
            "branch": "main",
            "docker_image": "ubuntu:latest"
        }))
        .expect("request");

        assert_eq!(request.installation_id, 0);
        assert_eq!(request.repository_id, 0);
        assert_eq!(request.repository_full_name.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn relay_envelope_round_trips() {
        let envelope = RelayEnvelope::Request(RelayRequest {
            id: "1".to_string(),
            method: "github.installations.list".to_string(),
            params: json!({}),
        });
        let value = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(value["type"], "request");
        let decoded: RelayEnvelope = serde_json::from_value(value).expect("deserialize");
        match decoded {
            RelayEnvelope::Request(request) => {
                assert_eq!(request.id, "1");
                assert_eq!(request.method, "github.installations.list");
            }
            other => panic!("unexpected envelope: {other:?}"),
        }
    }

    #[test]
    fn relay_github_app_installation_start_round_trips() {
        let request = GithubAppInstallationStartRequest {
            origin: "http://127.0.0.1:8080".to_string(),
            return_hash: Some("#projects".to_string()),
        };
        let value = serde_json::to_value(&request).expect("serialize");
        let decoded: GithubAppInstallationStartRequest =
            serde_json::from_value(value).expect("deserialize");
        assert_eq!(decoded.origin, "http://127.0.0.1:8080");
        assert_eq!(decoded.return_hash.as_deref(), Some("#projects"));

        let response = GithubAppInstallationStartResponse {
            state: "state-1".to_string(),
            install_url: "https://github.com/apps/mai/installations/select_target?state=state-1"
                .to_string(),
            app: GithubAppSettingsResponse {
                app_id: Some("123".to_string()),
                base_url: "https://api.github.com".to_string(),
                public_url: Some("https://relay.example".to_string()),
                has_private_key: true,
                app_slug: Some("mai".to_string()),
                app_html_url: None,
                owner_login: None,
                owner_type: None,
                install_url: Some(
                    "https://github.com/apps/mai/installations/select_target".to_string(),
                ),
            },
        };
        let value = serde_json::to_value(&response).expect("serialize");
        let decoded: GithubAppInstallationStartResponse =
            serde_json::from_value(value).expect("deserialize");
        assert_eq!(decoded.state, "state-1");
        assert!(decoded.app.has_private_key);
        assert_eq!(
            decoded.app.public_url.as_deref(),
            Some("https://relay.example")
        );
    }

    #[test]
    fn relay_settings_request_and_response_round_trip_without_token_secret() {
        let request = RelaySettingsRequest {
            enabled: true,
            url: Some("https://relay.example".to_string()),
            token: Some("secret".to_string()),
            node_id: Some("mai-server-a".to_string()),
        };
        let value = serde_json::to_value(&request).expect("serialize");
        let decoded: RelaySettingsRequest = serde_json::from_value(value).expect("deserialize");
        assert!(decoded.enabled);
        assert_eq!(decoded.url.as_deref(), Some("https://relay.example"));
        assert_eq!(decoded.token.as_deref(), Some("secret"));
        assert_eq!(decoded.node_id.as_deref(), Some("mai-server-a"));

        let response = RelaySettingsResponse {
            enabled: true,
            url: "https://relay.example".to_string(),
            has_token: true,
            node_id: "mai-server-a".to_string(),
        };
        let value = serde_json::to_value(&response).expect("serialize");
        assert!(value.get("token").is_none());
        let decoded: RelaySettingsResponse = serde_json::from_value(value).expect("deserialize");
        assert!(decoded.enabled);
        assert_eq!(decoded.url, "https://relay.example");
        assert!(decoded.has_token);
        assert_eq!(decoded.node_id, "mai-server-a");
    }

    #[test]
    fn relay_update_dtos_round_trip() {
        let status = RelayUpdateStatusResponse {
            current_version: "0.1.0".to_string(),
            latest_version: "0.2.0".to_string(),
            has_update: true,
            can_update: true,
            release: Some(RelayUpdateReleaseInfo {
                name: "v0.2.0".to_string(),
                body: "Relay update".to_string(),
                published_at: "2026-05-15T00:00:00Z".to_string(),
                html_url: "https://github.com/ZR233/mai-team/releases/tag/v0.2.0".to_string(),
            }),
            cached: false,
            warning: Some("download ready".to_string()),
            restart_scheduled: false,
        };
        let decoded: RelayUpdateStatusResponse =
            serde_json::from_value(serde_json::to_value(&status).expect("serialize"))
                .expect("deserialize");
        assert_eq!(decoded, status);

        let action = RelayUpdateActionResponse {
            status,
            message: "updated".to_string(),
            restart_scheduled: true,
        };
        let decoded: RelayUpdateActionResponse =
            serde_json::from_value(serde_json::to_value(&action).expect("serialize"))
                .expect("deserialize");
        assert_eq!(decoded, action);

        let request = RelayUpdateCheckRequest { force: true };
        let decoded: RelayUpdateCheckRequest =
            serde_json::from_value(serde_json::to_value(&request).expect("serialize"))
                .expect("deserialize");
        assert_eq!(decoded, request);
    }

    #[test]
    fn github_installation_summary_exposes_subscribed_events() {
        let summary: GithubInstallationSummary = serde_json::from_value(json!({
            "id": 42,
            "account_login": "ZR233",
            "account_type": "User",
            "repository_selection": "all",
            "events": ["pull_request", "check_run"]
        }))
        .expect("summary");

        assert_eq!(
            summary.events,
            vec!["pull_request".to_string(), "check_run".to_string()]
        );
    }

    #[test]
    fn relay_event_kind_maps_github_events() {
        assert_eq!(
            RelayEventKind::from_github_event("pull_request").as_github_event(),
            "pull_request"
        );
        assert_eq!(
            RelayEventKind::from_github_event("workflow_job").as_github_event(),
            "workflow_job"
        );
    }
}
