//! T29 的项目引用替换、应用与还原。

use std::time::Duration;

use mai_protocol::{
    ApplyProjectReferencesRequest, ProjectId, ProjectReferenceApplicationState,
    ProjectReferenceBinding, ProjectReferenceConfiguration, ProjectReferenceStatus,
    ReferenceApplyPolicy, ReferenceStatusQuery, ReplaceProjectReferencesRequest,
};
use serde::Serialize;
use serde_json::json;
use tokio::time::{Instant, sleep};

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::host::RuntimeHost;

use super::support;

/// 收敛轮询间隔。
const POLL: Duration = Duration::from_millis(500);

/// 一次“已保存并申请应用”的公共入口事实。
#[derive(Debug, Clone, Serialize)]
pub(super) struct SavedApply {
    pub configuration: ProjectReferenceConfiguration,
    pub initial_status: ProjectReferenceStatus,
}

/// 读取项目配置。
pub(super) async fn configuration(
    host: &RuntimeHost,
    project: ProjectId,
) -> Result<ProjectReferenceConfiguration> {
    support::bounded(
        host,
        "project_reference_configuration",
        host.runtime.project_reference_configuration(project),
    )
    .await
}

/// 只改 private-fixture alias，source_id 与启用状态保持原配置。
pub(super) fn retarget(
    source: &ProjectReferenceConfiguration,
    previous_alias: &str,
    alias: &str,
) -> Result<Vec<ProjectReferenceBinding>> {
    let Some(private) = source
        .bindings
        .iter()
        .find(|binding| binding.alias == previous_alias)
    else {
        return Err(error(
            "t29_private_binding_missing",
            "T18 项目缺少 private-fixture 绑定，无法构造引用竞争",
        ));
    };
    if !private.enabled {
        return Err(error(
            "t29_private_binding_disabled",
            "T18 项目的 private-fixture 绑定处于禁用状态",
        ));
    }
    Ok(source
        .bindings
        .iter()
        .map(|binding| {
            if binding.id == private.id {
                ProjectReferenceBinding {
                    alias: alias.to_string(),
                    ..binding.clone()
                }
            } else {
                binding.clone()
            }
        })
        .collect())
}

/// CAS 保存目标并通过公开 `apply_project_references` 申请 WhenIdle 应用。
pub(super) async fn save_and_apply(
    host: &RuntimeHost,
    project: ProjectId,
    expected_revision: u64,
    bindings: Vec<ProjectReferenceBinding>,
) -> Result<SavedApply> {
    let configuration = support::bounded(
        host,
        "replace_project_references",
        host.runtime.replace_project_references(
            project,
            ReplaceProjectReferencesRequest {
                expected_revision,
                bindings,
            },
        ),
    )
    .await?;
    if configuration.revision <= expected_revision {
        return Err(error(
            "t29_revision_not_advanced",
            format!(
                "替换引用后修订没有前进：{expected_revision} -> {}",
                configuration.revision
            ),
        ));
    }
    let initial_status = support::bounded(
        host,
        "apply_project_references",
        host.runtime.apply_project_references(
            project,
            ApplyProjectReferencesRequest {
                expected_revision: configuration.revision,
                policy: ReferenceApplyPolicy::WhenIdle,
            },
        ),
    )
    .await?;
    Ok(SavedApply {
        configuration,
        initial_status,
    })
}

/// 等待 durable 配置、目标修订与所有仍存在物理环境真实收敛。
pub(super) async fn wait_applied(
    host: &RuntimeHost,
    project: ProjectId,
    revision: u64,
    evidence: &mut Evidence,
) -> Result<ProjectReferenceStatus> {
    let limit = support::operation_limit(host);
    let deadline = Instant::now() + limit;
    loop {
        let status = support::bounded(
            host,
            "project_reference_status",
            host.runtime
                .project_reference_status(project, ReferenceStatusQuery::default()),
        )
        .await?;
        let settled = status.configuration.revision == revision
            && status.target_revision == revision
            && status.state == ProjectReferenceApplicationState::Applied
            && status.counts.failed == 0
            && status.counts.waiting == 0;
        if settled {
            evidence.fact(format!("t29.applied_revision_{revision}"), json!(&status));
            return Ok(status);
        }
        if status.state == ProjectReferenceApplicationState::Failed
            || status.counts.failed > 0
            || Instant::now() >= deadline
        {
            return Err(error(
                "t29_apply_not_converged",
                format!(
                    "目标修订 {revision} 未收敛（limit={limit:?}）：state={} counts={:?}",
                    status.state, status.counts
                ),
            ));
        }
        sleep(POLL).await;
    }
}

/// 最终还原原 alias 并等待真实 Applied；不改 source_id 或条目 ID。
pub(super) async fn restore(
    host: &RuntimeHost,
    handles: &super::super::support::ProjectHandles,
    original: &ProjectReferenceConfiguration,
    evidence: &mut Evidence,
) -> Result<u64> {
    let mut current = configuration(host, handles.project).await?;
    let mut target_revision = current.revision;
    if current.bindings != original.bindings {
        if current.revision != original.revision {
            // 先让在途目标收敛，避免在活动操作上叠加第二个 CAS。
            wait_applied(host, handles.project, current.revision, evidence).await?;
            current = configuration(host, handles.project).await?;
        }
        let restored = support::bounded(
            host,
            "restore_replace_project_references",
            host.runtime.replace_project_references(
                handles.project,
                ReplaceProjectReferencesRequest {
                    expected_revision: current.revision,
                    bindings: original.bindings.clone(),
                },
            ),
        )
        .await?;
        target_revision = restored.revision;
        support::bounded(
            host,
            "restore_apply_project_references",
            host.runtime.apply_project_references(
                handles.project,
                ApplyProjectReferencesRequest {
                    expected_revision: restored.revision,
                    policy: ReferenceApplyPolicy::WhenIdle,
                },
            ),
        )
        .await?;
    }
    let status = wait_applied(host, handles.project, target_revision, evidence).await?;
    evidence.fact("t29.restored_status", json!(&status));
    Ok(target_revision)
}
