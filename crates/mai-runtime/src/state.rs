use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};

use mai_docker::ContainerHandle;
use mai_mcp::McpAgentManager;
use mai_protocol::{
    AgentId, AgentMessage, AgentSessionSummary, AgentSummary, ArtifactInfo, ModelInputItem,
    PlanHistoryEntry, ProjectId, ProjectSummary, SessionId, TaskId, TaskPlan, TaskReview,
    TaskSummary, TurnId,
};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::projects::mcp::ProjectMcpManagerHandle;
use crate::projects::review::pool::ProjectReviewPool;

pub(crate) struct RuntimeState {
    pub(crate) agents: RwLock<HashMap<AgentId, Arc<AgentRecord>>>,
    pub(crate) tasks: RwLock<HashMap<TaskId, Arc<TaskRecord>>>,
    pub(crate) projects: RwLock<HashMap<ProjectId, Arc<ProjectRecord>>>,
    pub(crate) project_skill_locks: RwLock<HashMap<ProjectId, Arc<RwLock<()>>>>,
    pub(crate) project_mcp_managers: RwLock<HashMap<ProjectId, ProjectMcpManagerHandle>>,
}

impl RuntimeState {
    pub(crate) fn new(
        agents: HashMap<AgentId, Arc<AgentRecord>>,
        tasks: HashMap<TaskId, Arc<TaskRecord>>,
        projects: HashMap<ProjectId, Arc<ProjectRecord>>,
    ) -> Self {
        Self {
            agents: RwLock::new(agents),
            tasks: RwLock::new(tasks),
            projects: RwLock::new(projects),
            project_skill_locks: RwLock::new(HashMap::new()),
            project_mcp_managers: RwLock::new(HashMap::new()),
        }
    }
}

pub(crate) struct ProjectRecord {
    pub(crate) summary: RwLock<ProjectSummary>,
    pub(crate) sidecar: RwLock<Option<ContainerHandle>>,
    pub(crate) review_worker: Mutex<Option<ProjectReviewWorker>>,
    pub(crate) review_pool: Mutex<ProjectReviewPool>,
    pub(crate) review_notify: Arc<Notify>,
}

impl ProjectRecord {
    pub(crate) fn new(summary: ProjectSummary) -> Self {
        Self {
            summary: RwLock::new(summary),
            sidecar: RwLock::new(None),
            review_worker: Mutex::new(None),
            review_pool: Mutex::new(ProjectReviewPool::default()),
            review_notify: Arc::new(Notify::new()),
        }
    }
}

pub(crate) struct ProjectReviewWorker {
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) pool_abort_handle: futures::future::AbortHandle,
    pub(crate) selector_abort_handle: Option<futures::future::AbortHandle>,
}

pub(crate) struct TaskRecord {
    pub(crate) summary: RwLock<TaskSummary>,
    pub(crate) plan: RwLock<TaskPlan>,
    pub(crate) plan_history: RwLock<Vec<PlanHistoryEntry>>,
    pub(crate) reviews: RwLock<Vec<TaskReview>>,
    pub(crate) artifacts: RwLock<Vec<ArtifactInfo>>,
    pub(crate) workflow_lock: Mutex<()>,
}

pub(crate) struct AgentRecord {
    pub(crate) summary: RwLock<AgentSummary>,
    pub(crate) sessions: Mutex<Vec<AgentSessionRecord>>,
    pub(crate) container: RwLock<Option<ContainerHandle>>,
    pub(crate) mcp: RwLock<Option<Arc<McpAgentManager>>>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) turn_lock: Mutex<()>,
    pub(crate) cancel_requested: AtomicBool,
    pub(crate) active_turn: StdMutex<Option<TurnControl>>,
    pub(crate) pending_inputs: Mutex<VecDeque<QueuedAgentInput>>,
}

#[derive(Clone)]
pub(crate) struct TurnControl {
    pub(crate) turn_id: TurnId,
    pub(crate) session_id: SessionId,
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) abort_handle: Option<futures::future::AbortHandle>,
}

#[derive(Clone)]
pub(crate) struct TurnGuard {
    pub(crate) turn_id: TurnId,
    pub(crate) cancellation_token: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct AgentSessionRecord {
    pub(crate) summary: AgentSessionSummary,
    pub(crate) messages: Vec<AgentMessage>,
    pub(crate) history: Vec<ModelInputItem>,
    pub(crate) last_context_tokens: Option<u64>,
    pub(crate) last_turn_response: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedAgentInput {
    pub(crate) session_id: Option<SessionId>,
    pub(crate) message: String,
    pub(crate) skill_mentions: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct CollabInput {
    pub(crate) message: Option<String>,
    pub(crate) skill_mentions: Vec<String>,
}
