//! T20 的引用更新/ensure 步骤：同绑定 replace（revision 不变）+ 当前修订的 apply。
//!
//! 该步骤发生在 Reviewer 的 probe 窗口内（provider 侧延迟期间），因此 probe 触发的真实工具读取
//! 属于"引用更新之后"的视图。判定只使用产品返回的真实修订与计数。

use mai_protocol::{
    ApplyProjectReferencesRequest, ProjectId, ReferenceApplyPolicy, ReplaceProjectReferencesRequest,
};
use serde_json::json;

use crate::evidence::Evidence;
use crate::host::RuntimeHost;

/// 执行一次幂等的引用更新/ensure，并把结果写进证据。
pub(super) async fn ensure(host: &RuntimeHost, project_id: ProjectId, evidence: &mut Evidence) {
    let before = match host
        .runtime
        .project_reference_configuration(project_id)
        .await
    {
        Ok(configuration) => configuration,
        Err(cause) => {
            evidence.fail(
                "t20.reference_update_idempotent",
                format!("无法读取项目引用配置：{cause}"),
            );
            return;
        }
    };
    let replaced = match host
        .runtime
        .replace_project_references(
            project_id,
            ReplaceProjectReferencesRequest {
                expected_revision: before.revision,
                bindings: before.bindings.clone(),
            },
        )
        .await
    {
        Ok(replaced) => replaced,
        Err(cause) => {
            evidence.fail(
                "t20.reference_update_idempotent",
                format!("同绑定 replace_project_references 失败：{cause}"),
            );
            return;
        }
    };
    evidence.fact(
        "t20.reference_update",
        json!({
            "requested_revision": before.revision,
            "resulting_revision": replaced.revision,
            "bindings": &replaced.bindings,
        }),
    );
    evidence.check(
        "t20.reference_update_idempotent",
        replaced.revision == before.revision,
        format!(
            "以完全相同的绑定执行引用更新：revision 保持 {}（ensure 语义）",
            replaced.revision
        ),
        format!(
            "同绑定 replace 改变了 revision：{} -> {}",
            before.revision, replaced.revision
        ),
    );
    match host
        .runtime
        .apply_project_references(
            project_id,
            ApplyProjectReferencesRequest {
                expected_revision: replaced.revision,
                policy: ReferenceApplyPolicy::WhenIdle,
            },
        )
        .await
    {
        Ok(status) => {
            evidence.check(
                "t20.reference_ensure_requested",
                status.target_revision == replaced.revision && status.counts.failed == 0,
                format!(
                    "已请求 apply 修订 {}（state={} applied={} waiting={} failed=0）",
                    status.target_revision,
                    status.state,
                    status.counts.applied,
                    status.counts.waiting
                ),
                format!(
                    "apply 请求异常：target={} state={} failed={}",
                    status.target_revision, status.state, status.counts.failed
                ),
            );
            evidence.fact("t20.reference_ensure_status", json!(&status));
        }
        Err(cause) => evidence.fail(
            "t20.reference_ensure_requested",
            format!("apply_project_references 失败：{cause}"),
        ),
    }
}
