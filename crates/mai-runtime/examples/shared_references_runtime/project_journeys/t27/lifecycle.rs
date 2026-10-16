//! T27 的引用配置还原与收敛等待。

use std::time::Duration;

use mai_protocol::{
    ApplyProjectReferencesRequest, ProjectId, ProjectReferenceApplicationState,
    ProjectReferenceConfiguration, ProjectReferenceStatus, ReferenceApplyPolicy,
    ReferenceStatusQuery, ReplaceProjectReferencesRequest,
};
use serde_json::json;
use tokio::time::{Instant, sleep};

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::host::RuntimeHost;

use super::super::support::ProjectHandles;
use super::assertions::checked;
use super::environment;
use super::report::record_environment;

/// 引用应用收敛轮询间隔。
const CONVERGENCE_POLL: Duration = Duration::from_secs(1);

/// 还原原 private-fixture alias，并确认后续 T20 所需的原始挂载恢复。
pub(super) async fn restore_original(
    host: &RuntimeHost,
    handles: &ProjectHandles,
    original: &ProjectReferenceConfiguration,
    evidence: &mut Evidence,
) -> Result<()> {
    let mut current = host
        .runtime
        .project_reference_configuration(handles.project)
        .await
        .map_err(|cause| {
            error(
                "t27_restore_read_failed",
                format!("还原前读取配置失败：{cause}"),
            )
        })?;
    if current.bindings != original.bindings {
        let status = host
            .runtime
            .project_reference_status(
                handles.project,
                mai_protocol::ReferenceStatusQuery::default(),
            )
            .await
            .map_err(|cause| {
                error(
                    "t27_restore_status_unreadable",
                    format!("还原前读取应用状态失败：{cause}"),
                )
            })?;
        if status.target_revision == current.revision {
            // 已有目标修订时先等待其收敛，避免在活动操作上叠加第二个替换。
            wait_for_applied(
                host,
                handles.project,
                current.revision,
                host.config.operation_timeout,
            )
            .await?;
        }
        current = host
            .runtime
            .project_reference_configuration(handles.project)
            .await
            .map_err(|cause| {
                error(
                    "t27_restore_reread_failed",
                    format!("还原前重读配置失败：{cause}"),
                )
            })?;
    }

    let restored = if current.bindings == original.bindings {
        current
    } else {
        let restored = host
            .runtime
            .replace_project_references(
                handles.project,
                ReplaceProjectReferencesRequest {
                    expected_revision: current.revision,
                    bindings: original.bindings.clone(),
                },
            )
            .await
            .map_err(|cause| {
                error(
                    "t27_restore_replace_failed",
                    format!("CAS 还原 private-fixture alias 失败：{cause}"),
                )
            })?;
        host.runtime
            .apply_project_references(
                handles.project,
                ApplyProjectReferencesRequest {
                    expected_revision: restored.revision,
                    policy: ReferenceApplyPolicy::WhenIdle,
                },
            )
            .await
            .map_err(|cause| {
                error(
                    "t27_restore_apply_failed",
                    format!("还原修订 {} 的应用请求失败：{cause}", restored.revision),
                )
            })?;
        restored
    };
    evidence.fact("t27.restored_configuration", json!(&restored));
    let status = wait_for_applied(
        host,
        handles.project,
        restored.revision,
        host.config.operation_timeout,
    )
    .await?;
    evidence.fact("t27.restored_status", json!(&status));

    let Some(container_id) = host.container_id(handles.maintainer).await? else {
        evidence.fail(
            "t27.restore_environment_ready",
            "还原后 Maintainer 没有发布容器，T20 无法继续",
        );
        return Err(error(
            "t27_restore_container_missing",
            "还原后 Maintainer 没有发布容器",
        ));
    };
    let restored_environment =
        environment::inspect_binding(host, handles.maintainer, &container_id).await?;
    record_environment(evidence, "t27.restored_environment", &restored_environment);
    let private_target = crate::fixtures::container_path(crate::fixtures::ALIAS_PRIVATE);
    let targets = crate::host::reference_targets_of(&restored_environment.facts);
    if !checked(
        evidence,
        "t27.restore_environment_ready",
        targets.contains(&private_target)
            && matches!(
                restored_environment.inspected,
                mai_docker::ExistingEnvironment::Matching(_)
            ),
        format!("原 alias {private_target} 已恢复且新环境通过 DockerClient 核验"),
        format!("还原后的挂载或环境核验不满足 T20：targets={targets:?}"),
    ) {
        return Err(error(
            "t27_restore_not_converged",
            "还原后的执行环境没有回到 private-fixture 原挂载",
        ));
    }
    Ok(())
}

/// 等待项目配置与所有物理环境收敛到目标修订。
pub(super) async fn wait_for_applied(
    host: &RuntimeHost,
    project: ProjectId,
    revision: u64,
    limit: Duration,
) -> Result<ProjectReferenceStatus> {
    let deadline = Instant::now() + limit;
    loop {
        let status = host
            .runtime
            .project_reference_status(project, ReferenceStatusQuery::default())
            .await
            .map_err(|cause| {
                error(
                    "t27_status_unreadable",
                    format!("读取项目 {project} 的引用应用状态失败：{cause}"),
                )
            })?;
        let settled = status.configuration.revision == revision
            && status.target_revision == revision
            && status.state == ProjectReferenceApplicationState::Applied
            && status.counts.failed == 0;
        if settled {
            return Ok(status);
        }
        if status.state == ProjectReferenceApplicationState::Failed || status.counts.failed > 0 {
            return Err(error(
                "t27_apply_failed",
                format!(
                    "目标修订 {revision} 应用失败：state={} counts={:?}",
                    status.state, status.counts
                ),
            ));
        }
        if Instant::now() >= deadline {
            return Err(error(
                "t27_apply_timeout",
                format!(
                    "等待目标修订 {revision} 收敛超时 {limit:?}：state={} counts={:?}",
                    status.state, status.counts
                ),
            ));
        }
        sleep(CONVERGENCE_POLL).await;
    }
}
