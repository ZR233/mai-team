//! 最终环境计划、实际环境核对与定向清理。
//!
//! 这一层只回答“实际 Docker 状态是否符合期望计划”，不判断某个参考源是否被授权，
//! 也不负责 Thread 生命周期；授权与协调由上层运行时负责。

mod compare;
mod diff;
mod ops;
mod plan;
mod preserve;

use uuid::Uuid;

use crate::error::{DockerError, Result};

pub use compare::{
    compare_environment, compare_required_mounts, parse_cpu_count, parse_docker_size,
};
pub use diff::{EnvironmentDiff, EnvironmentDiffEntry};
pub use plan::{
    AgentEnvironmentPlan, AgentEnvironmentPlanBuilder, AgentMcpSidecarPlan, ContainerNetworkMode,
    WorkspaceMountPlan,
};
pub use preserve::preserved_volume_mounts;

/// 期望环境与实际 Docker 状态的三态结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingEnvironment {
    /// 没有任何属于该计划 generation 的受管容器。
    Absent,
    /// 实际容器已核验符合计划，可以复用。
    Matching(VerifiedContainer),
    /// 存在候选但实际配置与计划不一致；由上层决定何时执行换代，本层不隐式删除。
    ReconfigurationRequired(EnvironmentDiff),
}

impl ExistingEnvironment {
    pub fn matching(self) -> Option<VerifiedContainer> {
        match self {
            Self::Matching(verified) => Some(verified),
            Self::Absent | Self::ReconfigurationRequired(_) => None,
        }
    }

    pub fn diff(&self) -> Option<&EnvironmentDiff> {
        match self {
            Self::ReconfigurationRequired(diff) => Some(diff),
            Self::Absent | Self::Matching(_) => None,
        }
    }
}

/// 已经与计划比对通过、可以发布租约的容器。
///
/// 字段私有，只能由核验路径构造；持有该值即表示挂载、镜像与 HostConfig 已经核对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedContainer {
    id: String,
    name: String,
    image_reference: String,
    image_id: String,
    generation: Option<Uuid>,
    state: String,
    running: bool,
}

impl VerifiedContainer {
    pub(crate) fn verified(facts: &crate::inspect::ContainerEnvironmentFacts) -> Self {
        Self {
            id: facts.id.clone(),
            name: facts.name.clone(),
            image_reference: facts.image_reference.clone(),
            image_id: facts.image_id.clone(),
            generation: facts.generation,
            state: facts.state.clone(),
            running: facts.running,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn image_reference(&self) -> &str {
        &self.image_reference
    }

    pub fn image_id(&self) -> &str {
        &self.image_id
    }

    pub fn generation(&self) -> Option<Uuid> {
        self.generation
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// `docker commit` 保留可写层的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSnapshotRequest {
    pub operation_id: Uuid,
    pub agent_id: String,
    pub container_id: String,
}

impl EnvironmentSnapshotRequest {
    pub fn new(
        operation_id: Uuid,
        agent_id: impl Into<String>,
        container_id: impl Into<String>,
    ) -> Result<Self> {
        let agent_id = agent_id.into();
        let container_id = container_id.into();
        if agent_id.trim().is_empty() || container_id.trim().is_empty() {
            return Err(DockerError::CommandFailed(
                "snapshot request requires an agent id and a container id".to_string(),
            ));
        }
        Ok(Self {
            operation_id,
            agent_id,
            container_id,
        })
    }
}

/// 快照镜像的收据；重启后按 operation 找回同一个确定名字/标签的镜像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSnapshotReceipt {
    pub operation_id: Uuid,
    pub agent_id: String,
    pub container_id: String,
    pub image_name: String,
    pub image_id: String,
}
