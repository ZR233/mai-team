use std::collections::HashMap;
use std::sync::Arc;

use crate::mcp::ContainerMcpRuntime;
use mai_docker::ContainerHandle;
use mai_protocol::{
    AgentId, AgentSummary, ArtifactInfo, PlanHistoryEntry, ProjectId, ProjectSummary, TaskId,
    TaskPlan, TaskReview, TaskSummary,
};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::projects::review::context::ProjectReviewContext;
use crate::projects::review::pool::ProjectReviewPool;
#[cfg(test)]
use crate::projects::review::relay_queue::ProjectReviewRelayQueue;

pub(crate) struct RuntimeState {
    pub(crate) agents: RwLock<HashMap<AgentId, Arc<AgentRecord>>>,
    pub(crate) tasks: RwLock<HashMap<TaskId, Arc<TaskRecord>>>,
    pub(crate) projects: RwLock<HashMap<ProjectId, Arc<ProjectRecord>>>,
    pub(crate) project_skill_locks: RwLock<HashMap<ProjectId, Arc<RwLock<()>>>>,
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
        }
    }
}

pub(crate) struct ProjectRecord {
    pub(crate) summary: RwLock<ProjectSummary>,
    pub(crate) sidecar: RwLock<Option<ContainerHandle>>,
    pub(crate) repo_sync_lock: Mutex<()>,
    pub(crate) review_cycle_lock: Mutex<()>,
    pub(crate) review_worker: Mutex<Option<ProjectReviewWorker>>,
    pub(crate) review_pool: Mutex<ProjectReviewPool>,
    pub(crate) review_notify: Arc<Notify>,
    #[cfg(test)]
    pub(crate) relay_review_queue: Mutex<ProjectReviewRelayQueue>,
    #[cfg(test)]
    pub(crate) relay_review_notify: Arc<Notify>,
}

impl ProjectRecord {
    pub(crate) fn new(summary: ProjectSummary) -> Self {
        Self {
            summary: RwLock::new(summary),
            sidecar: RwLock::new(None),
            repo_sync_lock: Mutex::new(()),
            review_cycle_lock: Mutex::new(()),
            review_worker: Mutex::new(None),
            review_pool: Mutex::new(ProjectReviewPool::default()),
            review_notify: Arc::new(Notify::new()),
            #[cfg(test)]
            relay_review_queue: Mutex::new(ProjectReviewRelayQueue::default()),
            #[cfg(test)]
            relay_review_notify: Arc::new(Notify::new()),
        }
    }
}

pub(crate) struct ProjectReviewWorker {
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) pool_abort_handle: futures::future::AbortHandle,
    pub(crate) selector_abort_handle: Option<futures::future::AbortHandle>,
    #[cfg(test)]
    pub(crate) relay_selector_abort_handle: Option<futures::future::AbortHandle>,
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
    pub(crate) runtime_agent_id: RwLock<pl_core::AgentId>,
    pub(crate) summary: RwLock<AgentSummary>,
    pub(crate) container: RwLock<Option<ContainerHandle>>,
    pub(crate) mcp: RwLock<Option<Arc<ContainerMcpRuntime>>>,
    pub(crate) review_context: RwLock<Option<Arc<ProjectReviewContext>>>,
    pub(crate) system_prompt: Option<String>,
}
