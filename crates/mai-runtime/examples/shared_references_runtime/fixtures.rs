//! 前一领域驱动 seed 的夹具身份：固定 UUIDv5 命名空间、项目、Agent 与参考源别名。
//!
//! 驱动只**读取**这些事实：不重建夹具、不改写夹具记录（`lifecycle` 例外：它在 Runtime
//! 启动前给父 Agent 冻结一个被授权 Profile，见 `lifecycle.rs`），也不删除任何既有资源。

use std::sync::Arc;

use mai_protocol::ReferenceSource;
use mai_protocol::{AgentId, ProjectId, ProjectReferenceBinding, ProjectReferenceConfiguration};
use mai_references::ReferenceCatalog;
use mai_store::MaiStore;
use uuid::Uuid;

use crate::error::{Result, error};

/// 夹具 ID 的固定命名空间（与领域驱动一致）。
pub const FIXTURE_NAMESPACE: Uuid = Uuid::from_u128(0x9d5c7e6ac1b24f3a9e77a1b2c3d4e5f6);

/// 夹具项目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    A,
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

/// project A 的 Maintainer：本驱动默认的可信父 Agent（有真实环境且已 Ready）。
pub const AGENT_MAINTAINER: &str = "a-maintainer";

/// project A 的既有 child（真实环境）。
pub const AGENT_CHILD: &str = "a-child";

/// project B 的 Executor：没有任何 project A 参考源。
pub const AGENT_B_EXECUTOR: &str = "b-executor";

/// 非项目 Agent：没有任何项目参考源。
pub const AGENT_NON_PROJECT: &str = "nonproject-executor";

/// project A 的两个参考源别名（夹具契约）。
pub const ALIAS_LINUX: &str = "linux";
pub const ALIAS_PRIVATE: &str = "private-fixture";

/// 固定命名空间下的夹具 ID。
pub fn fixture_uuid(kind: &str, name: &str) -> Uuid {
    Uuid::new_v5(&FIXTURE_NAMESPACE, format!("{kind}:{name}").as_bytes())
}

/// 夹具 Agent ID。
pub fn agent_id(key: &str) -> AgentId {
    fixture_uuid("agent", key)
}

/// 读取项目参考配置（不修改）。
pub async fn project_configuration(
    store: &Arc<MaiStore>,
    project: ProjectKind,
) -> Result<ProjectReferenceConfiguration> {
    store
        .project_reference_configuration(project.id())
        .await
        .map_err(|cause| {
            error(
                "project_configuration_unreadable",
                format!("无法读取 {} 的参考配置：{cause}", project.slug()),
            )
        })
}

/// 按 alias 取绑定；缺失时返回 `None`（由调用方决定是 fail 还是 boundary）。
pub fn binding<'a>(
    configuration: &'a ProjectReferenceConfiguration,
    alias: &str,
) -> Option<&'a ProjectReferenceBinding> {
    configuration
        .bindings
        .iter()
        .find(|binding| binding.alias == alias)
}

/// 按 alias 取登记源；缺少绑定或源记录都显式失败。
pub async fn source_for_alias(
    catalog: &Arc<ReferenceCatalog>,
    configuration: &ProjectReferenceConfiguration,
    alias: &str,
) -> Result<ReferenceSource> {
    let binding = binding(configuration, alias).ok_or_else(|| {
        error(
            "fixture_binding_missing",
            format!("project A 缺少 alias `{alias}` 的绑定；前一领域驱动的夹具不完整"),
        )
    })?;
    catalog.source(binding.source_id).await.map_err(|cause| {
        error(
            "fixture_source_missing",
            format!(
                "alias `{alias}` 引用的参考源 {} 读取失败：{cause}",
                binding.source_id
            ),
        )
    })
}

/// project A 的容器内参考路径（`/references/<alias>`）。
pub fn container_path(alias: &str) -> String {
    format!("/references/{alias}")
}

/// 夹具合同校验：project A 必须绑定 Linux 快照与 A 私有夹具，且都处于启用状态。
pub fn ensure_project_a_contract(configuration: &ProjectReferenceConfiguration) -> Result<()> {
    for alias in [ALIAS_LINUX, ALIAS_PRIVATE] {
        let Some(binding) = binding(configuration, alias) else {
            return Err(error(
                "fixture_binding_missing",
                format!("project A 缺少 alias `{alias}` 的绑定"),
            ));
        };
        if !binding.enabled {
            return Err(error(
                "fixture_binding_disabled",
                format!("project A 的 alias `{alias}` 处于禁用状态，夹具契约要求启用"),
            ));
        }
    }
    Ok(())
}
