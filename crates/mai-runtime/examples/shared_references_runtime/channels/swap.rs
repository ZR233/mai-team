//! 参考配置换代（CAS + 等待真正 applied）的最小封装。
//!
//! `applied.rs` 里的同名辅助目前是模块私有（`gate.rs` 正在被 root 适配），因此通道用例只复用
//! 它**公开**的 `assert_fixture_restored`，并在本模块内自带一套收敛等待，保证 `channels` 不依赖
//! 未公开的可见性。若 root 后续把 `applied::wait_for_applied`/`applied::renamed` 公开，这里可以
//! 直接换成复用。

use std::time::Duration;

use mai_protocol::{
    ApplyProjectReferencesRequest, ProjectReferenceApplicationState, ProjectReferenceBinding,
    ProjectReferenceConfiguration, ProjectReferenceStatus, ReferenceApplyPolicy,
    ReplaceProjectReferencesRequest,
};
use tokio::time::sleep;

use crate::error::{Result, error};
use crate::fixtures::ProjectKind;
use crate::host::RuntimeHost;

/// 收敛轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 把某个 alias 改名（alias 决定容器目标路径，因此是真实挂载变化）。
pub fn renamed(
    configuration: &ProjectReferenceConfiguration,
    alias: &str,
    new_alias: &str,
) -> Vec<ProjectReferenceBinding> {
    configuration
        .bindings
        .iter()
        .map(|binding| {
            if binding.alias == alias {
                ProjectReferenceBinding {
                    alias: new_alias.to_string(),
                    ..binding.clone()
                }
            } else {
                binding.clone()
            }
        })
        .collect()
}

/// 用 CAS 保存新的绑定集合并请求 WhenIdle 应用。
pub async fn replace_and_apply(
    host: &RuntimeHost,
    project: ProjectKind,
    expected_revision: u64,
    bindings: Vec<ProjectReferenceBinding>,
    policy: ReferenceApplyPolicy,
    timeout: Duration,
) -> Result<ProjectReferenceConfiguration> {
    let saved = host
        .runtime
        .replace_project_references(
            project.id(),
            ReplaceProjectReferencesRequest {
                expected_revision,
                bindings,
            },
        )
        .await
        .map_err(|cause| {
            error(
                "replace_references_failed",
                format!("保存参考配置失败（CAS 期望 {expected_revision}）：{cause}"),
            )
        })?;
    host.runtime
        .apply_project_references(
            project.id(),
            ApplyProjectReferencesRequest {
                expected_revision: saved.revision,
                policy,
            },
        )
        .await
        .map_err(|cause| {
            error(
                "apply_request_rejected",
                format!("应用请求被拒绝（修订 {}）：{cause}", saved.revision),
            )
        })?;
    wait_for_applied(host, project, saved.revision, timeout).await?;
    Ok(saved)
}

/// 等待项目收敛到目标修订且没有失败条目。
pub async fn wait_for_applied(
    host: &RuntimeHost,
    project: ProjectKind,
    revision: u64,
    limit: Duration,
) -> Result<ProjectReferenceStatus> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let status = host
            .runtime
            .project_reference_status(project.id(), Default::default())
            .await
            .map_err(|cause| {
                error(
                    "reference_status_failed",
                    format!("读取 {} 的参考状态失败：{cause}", project.slug()),
                )
            })?;
        let settled = status.configuration.revision == revision
            && status.state == ProjectReferenceApplicationState::Applied;
        if settled {
            return Ok(status);
        }
        if status.state == ProjectReferenceApplicationState::Failed {
            return Err(error(
                "reference_apply_failed",
                format!("目标修订 {revision} 应用失败：{}", summarize(&status)),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error(
                "reference_apply_timeout",
                format!(
                    "等待目标修订 {revision} 收敛超时 {limit:?}：{}",
                    summarize(&status)
                ),
            ));
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// 把参考状态压成一行摘要（证据与错误消息共用）。
pub fn summarize(status: &ProjectReferenceStatus) -> String {
    format!(
        "revision={} state={} applied={} waiting={} failed={} deferred={} no_environment={}",
        status.configuration.revision,
        status.state,
        status.counts.applied,
        status.counts.waiting,
        status.counts.failed,
        status.counts.deferred,
        status.counts.no_environment
    )
}
