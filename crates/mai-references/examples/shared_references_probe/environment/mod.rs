//! 真实物理环境：初始卷与可写层标记、晋升（provision）、事实采集。
//!
//! 这里只做三件事，且全部走真实 `DockerClient`：
//! - 建/复用 Agent 环境（工作区卷、项目只读 view 卷、等价匿名卷、参考只读 bind）；
//! - 写/读三类数据标记（可写层 `/usr/local`、工作区未提交文件、匿名卷文件）；
//! - 采集数据库 + Docker 事实，供验收点判定与跨轮比较。
//!
//! 子模块按职责拆分，子模块项经 `pub use` 显式导出，对外路径与报表字段保持不变：
//! - `provision`：建/复用真实环境（卷、只读 view、等价匿名卷、参考只读 bind）；
//! - `markers`：三类数据标记的写入与只读校验；
//! - `facts`：Agent 事实采集与当前快照；
//! - `sources`：登记参考源的事实采集；
//! - `container`：容器内命令执行与文本读取薄封装。

mod container;
mod facts;
mod markers;
mod provision;
mod sources;

pub use container::{exec, shell_quote};
pub use facts::{AgentFacts, collect_agent_facts, current_snapshot};
pub use markers::{ensure_markers, marker_content};
pub use provision::provision_agent;
pub use sources::{SourceFacts, absent_source_facts, collect_source_facts};

use chrono::Utc;
use mai_docker::{
    AgentEnvironmentPlan, ContainerEnvironmentFacts, ContainerMount, ContainerMountFacts,
    ContainerVolumeMount, DockerClient, ExecOutput, ExistingEnvironment, WorkspaceMountPlan,
    agent_workspace_volume, project_cache_volume,
};
use mai_protocol::{AppliedReference, AppliedReferenceSnapshot, ReferenceApplyPhase};
use mai_references::{
    AgentEnvironmentLease, plan_from_environment, reference_mounts, render_model_manifest,
};
use mai_store::PersistedReferenceBinding;
use serde::Serialize;
use uuid::Uuid;

use crate::error::{ProbeError, Result};
use crate::fixtures::AgentFixture;

/// 等价匿名卷的容器内目标。
pub const ANON_VOLUME_TARGET: &str = "/opt/mai-acceptance-anon";

/// 项目只读视图卷的容器内目标（与产品 review view 同路径）。
pub const REVIEW_VIEW_TARGET: &str = "/project/repo";

/// 可写层标记（commit 保留可写层才存在）。
pub const MARKER_USR_LOCAL: &str = "/usr/local/mai-acceptance-marker.txt";

/// 工作区未提交文件标记（存在卷里）。
pub const MARKER_WORKSPACE: &str = "/workspace/repo/acceptance-marker.txt";

/// 匿名卷文件标记。
pub const MARKER_ANON: &str = "/opt/mai-acceptance-anon/marker.txt";

/// 让 root 能够按 agent 找到等价匿名卷。
pub fn anon_volume_name(agent_key: &str) -> String {
    format!("mai-team-acceptance-anon-{agent_key}")
}

/// 依赖注入用的探针句柄；命令与验收点共享同一套对象。
pub struct Probe {
    pub config: crate::config::ProbeConfig,
    pub docker: DockerClient,
    pub store: std::sync::Arc<mai_store::MaiStore>,
    pub catalog: std::sync::Arc<mai_references::ReferenceCatalog>,
    pub applications: std::sync::Arc<mai_references::ReferenceApplications>,
    pub host: std::sync::Arc<crate::host::ProbeHost>,
    pub policy: mai_references::ReferenceDeploymentPolicy,
    pub fixtures: crate::fixtures::FixtureSet,
    pub sources: Sources,
}

/// 登记源句柄。
#[derive(Debug, Clone)]
pub struct Sources {
    pub linux: Option<Uuid>,
    pub private_fixture: Option<Uuid>,
}

impl Sources {
    /// 尚未登记任何源。
    pub fn empty() -> Self {
        Self {
            linux: None,
            private_fixture: None,
        }
    }

    /// 按 alias 取源 ID；未知 alias 返回 `None`。
    pub fn by_alias(&self, config: &crate::config::ProbeConfig, alias: &str) -> Option<Uuid> {
        if alias == config.linux_alias {
            self.linux
        } else if alias == config.private_alias {
            self.private_fixture
        } else {
            None
        }
    }
}

/// 把 `Result` 从 `Option` 里解出来，缺失时报出清晰错误。
pub fn required<T>(value: Option<T>, what: &str) -> Result<T> {
    value.ok_or_else(|| ProbeError::new("probe_state_missing", format!("缺少{what}")))
}

/// Agent 事实里的已应用参考路径集合（用于“空清除”与“无 A 私有夹具”判定）。
pub fn reference_targets(facts: &AgentFacts) -> Vec<String> {
    match &facts.container {
        Some(container) => container
            .mounts
            .iter()
            .filter(|mount| {
                mount.target == "/references" || mount.target.starts_with("/references/")
            })
            .map(|mount| mount.target.clone())
            .collect(),
        None => Vec::new(),
    }
}

/// 宿主内容摘要（与真实验证器同一套扫描与摘要域）；不写任何文件。
pub fn host_digest(probe: &Probe, path: &std::path::Path) -> Result<String> {
    let text = path.to_string_lossy().into_owned();
    let root = mai_references::validate_source_root(&probe.policy, &text)?;
    let scan = mai_references::scan_source_tree(&root, &probe.policy)?;
    Ok(scan.digest().to_string())
}
