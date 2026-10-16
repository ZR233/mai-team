//! 持久化夹具：两个项目、八个 Agent（角色全覆盖 + parent/child/grandchild + 非项目）。
//!
//! 夹具 ID 用固定命名空间做 UUIDv5 派生，因此每次运行都指向同一批产品记录，不需要
//! 重置数据库；`seed` 只做幂等 upsert，不删除任何用户数据与 Docker 资源。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentId, AgentResourceSnapshot, AgentResourceState, AgentRole, AgentSummary,
    ProjectCloneStatus, ProjectId, ProjectReviewStatus, ProjectStatus, ProjectSummary,
    RuntimeUsageSnapshot,
};
use uuid::Uuid;

use crate::config::{ProbeConfig, ProvisionChoice};

/// 夹具 ID 的固定命名空间。
pub const FIXTURE_NAMESPACE: Uuid = Uuid::from_u128(0x9d5c7e6ac1b24f3a9e77a1b2c3d4e5f6);

/// 夹具项目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    /// A 项目：绑定 Linux 快照与自己的私有夹具。
    A,
    /// B 项目：必须看不到 A 的私有夹具。
    B,
}

impl ProjectKind {
    /// 项目 ID。
    pub fn id(self) -> ProjectId {
        fixture_uuid("project", self.slug())
    }

    /// 项目短名。
    pub fn slug(self) -> &'static str {
        match self {
            Self::A => "project-a",
            Self::B => "project-b",
        }
    }
}

/// 一个夹具 Agent。
#[derive(Debug, Clone)]
pub struct AgentFixture {
    /// 稳定 key，用于证据索引。
    pub key: &'static str,
    pub role: AgentRole,
    pub project: Option<ProjectKind>,
    pub parent: Option<&'static str>,
    /// 缺省（canonical）覆盖集合里的 Agent 必须建真实环境。
    pub required: bool,
    /// 是否挂载项目只读 view 卷 `/project/repo`。
    pub review_view: bool,
}

impl AgentFixture {
    /// Agent ID。
    pub fn id(&self) -> AgentId {
        fixture_uuid("agent", self.key)
    }

    /// 产品 Agent 名。
    pub fn name(&self) -> String {
        format!("acceptance-{}", self.key)
    }

    /// 角色标签。
    pub fn role_label(&self) -> String {
        self.role.to_string()
    }
}

/// 夹具集合。
#[derive(Debug, Clone)]
pub struct FixtureSet {
    pub agents: Vec<AgentFixture>,
}

impl FixtureSet {
    /// 固定夹具清单。
    pub fn new() -> Self {
        Self {
            agents: vec![
                AgentFixture {
                    key: "a-planner",
                    role: AgentRole::Planner,
                    project: Some(ProjectKind::A),
                    parent: None,
                    required: false,
                    review_view: true,
                },
                AgentFixture {
                    key: "a-explorer",
                    role: AgentRole::Explorer,
                    project: Some(ProjectKind::A),
                    parent: None,
                    required: false,
                    review_view: true,
                },
                AgentFixture {
                    key: "a-maintainer",
                    role: AgentRole::Executor,
                    project: Some(ProjectKind::A),
                    parent: None,
                    required: true,
                    review_view: true,
                },
                AgentFixture {
                    key: "a-reviewer",
                    role: AgentRole::Reviewer,
                    project: Some(ProjectKind::A),
                    parent: None,
                    required: false,
                    review_view: true,
                },
                AgentFixture {
                    key: "a-child",
                    role: AgentRole::Executor,
                    project: Some(ProjectKind::A),
                    parent: Some("a-maintainer"),
                    required: true,
                    review_view: true,
                },
                AgentFixture {
                    key: "a-grandchild",
                    role: AgentRole::Executor,
                    project: Some(ProjectKind::A),
                    parent: Some("a-child"),
                    required: false,
                    review_view: true,
                },
                AgentFixture {
                    key: "b-executor",
                    role: AgentRole::Executor,
                    project: Some(ProjectKind::B),
                    parent: None,
                    required: true,
                    review_view: true,
                },
                AgentFixture {
                    key: "nonproject-executor",
                    role: AgentRole::Executor,
                    project: None,
                    parent: None,
                    required: true,
                    review_view: false,
                },
            ],
        }
    }

    /// 按覆盖模式取本轮需要建真实环境的 Agent。
    pub fn selected(&self, choice: ProvisionChoice) -> Vec<&AgentFixture> {
        self.agents
            .iter()
            .filter(|agent| match choice {
                ProvisionChoice::Canonical => agent.required,
                ProvisionChoice::All => true,
            })
            .collect()
    }

    /// 根据 key 找夹具。
    pub fn agent(&self, key: &str) -> Option<&AgentFixture> {
        self.agents.iter().find(|agent| agent.key == key)
    }
}

impl Default for FixtureSet {
    fn default() -> Self {
        Self::new()
    }
}

/// 固定命名空间下的夹具 ID。
pub fn fixture_uuid(kind: &str, name: &str) -> Uuid {
    Uuid::new_v5(&FIXTURE_NAMESPACE, format!("{kind}:{name}").as_bytes())
}

/// 项目夹具摘要。
pub fn project_summary(
    config: &ProbeConfig,
    kind: ProjectKind,
    now: DateTime<Utc>,
) -> ProjectSummary {
    let maintainer = match kind {
        ProjectKind::A => fixture_uuid("agent", "a-maintainer"),
        ProjectKind::B => fixture_uuid("agent", "b-executor"),
    };
    ProjectSummary {
        id: kind.id(),
        name: format!("mai-acceptance/{}", kind.slug()),
        status: ProjectStatus::Ready,
        owner: "mai-acceptance".to_string(),
        repo: kind.slug().to_string(),
        repository_full_name: format!("mai-acceptance/{}", kind.slug()),
        git_account_id: None,
        repository_id: match kind {
            ProjectKind::A => 9_001,
            ProjectKind::B => 9_002,
        },
        installation_id: 0,
        installation_account: "mai-acceptance".to_string(),
        branch: "main".to_string(),
        docker_image: config.agent_image.clone(),
        clone_status: ProjectCloneStatus::Ready,
        maintainer_agent_id: maintainer,
        created_at: now,
        updated_at: now,
        last_error: None,
        // 验收不启动自动审查：Review Thread 与项目工作区不在本驱动覆盖范围。
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

/// Agent 夹具摘要；`container_id` 与 `created_at` 从既有记录继承，避免幂等 upsert 抹掉投影。
pub fn agent_summary(
    config: &ProbeConfig,
    fixtures: &FixtureSet,
    agent: &AgentFixture,
    existing: Option<&AgentSummary>,
    now: DateTime<Utc>,
) -> AgentSummary {
    let project = agent.project.map(|kind| kind.id());
    let resource_state = if existing.is_some() {
        existing.map(|summary| summary.resource.state)
    } else {
        None
    };
    let parent_id = agent.parent.map(|key| {
        fixtures
            .agent(key)
            .map(AgentFixture::id)
            .unwrap_or_else(|| fixture_uuid("agent", key))
    });
    AgentSummary {
        id: agent.id(),
        parent_id,
        task_id: None,
        project_id: project,
        role: Some(agent.role),
        profile_id: None,
        workspace: existing.and_then(|summary| summary.workspace.clone()),
        review_run_id: None,
        name: agent.name(),
        resource: AgentResourceSnapshot {
            // 未建环境时保持 Provisioning，建好环境后由 provision 写回 Ready。
            state: resource_state.unwrap_or(AgentResourceState::Provisioning),
            error: None,
        },
        runtime: None,
        last_turn: None,
        container_id: existing.and_then(|summary| summary.container_id.clone()),
        docker_image: config.agent_image.clone(),
        provider_id: "acceptance".to_string(),
        provider_name: "Acceptance Fixture".to_string(),
        model: "acceptance-model".to_string(),
        reasoning_effort: None,
        created_at: existing.map(|summary| summary.created_at).unwrap_or(now),
        updated_at: now,
        usage: RuntimeUsageSnapshot::default(),
    }
}

/// 既有 Agent 摘要按 ID 索引。
pub fn existing_agents(existing: &[AgentSummary]) -> BTreeMap<AgentId, AgentSummary> {
    existing
        .iter()
        .map(|summary| (summary.id, summary.clone()))
        .collect()
}
