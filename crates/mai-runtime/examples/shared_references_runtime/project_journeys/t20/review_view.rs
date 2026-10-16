//! Reviewer 冻结视图的物理事实与真实工具内容断言。
//!
//! `/project/repo` 的 `.git` 元数据在产品里是故意不可用的（reviewer system prompt 明确禁止在
//! `/project/repo` 里跑 git），因此"冻结在夹具 BASE"由两条真实证据共同支撑：
//! 1. `exec` 读到的 BASE 标记文件内容 == 夹具清单里 BASE commit 的 blob 内容；
//! 2. `exec` 在可写的 `/workspace/repo` 里读到的 `origin/main` == 夹具 BASE commit SHA，
//!    且该 commit 对象里的同一标记内容与 `/project/repo` 的读取一致。

use mai_docker::ContainerEnvironmentFacts;
use mai_protocol::AgentId;
use serde_json::json;

use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe::ProbeOutcome;

use super::super::fixture::GitHubFixture;
use super::super::support;

/// 一次 Reviewer 视图快照。
pub(super) struct ViewFacts {
    pub(super) container_id: String,
    pub(super) facts: ContainerEnvironmentFacts,
}

/// 读取 Reviewer 当前容器事实；容器已被回收时返回 `None` 并记录局限。
pub(super) async fn view_facts(
    host: &RuntimeHost,
    reviewer: AgentId,
    evidence: &mut Evidence,
) -> Option<ViewFacts> {
    let container_id = match host.container_id(reviewer).await {
        Ok(Some(container_id)) => container_id,
        Ok(None) => return None,
        Err(cause) => {
            evidence.limitation(format!("读取 Reviewer 容器身份失败：{}", cause.message()));
            return None;
        }
    };
    match host.container_facts(&container_id).await {
        Ok(facts) => Some(ViewFacts {
            container_id,
            facts,
        }),
        Err(cause) => {
            evidence.limitation(format!(
                "Reviewer 容器 {container_id} 事实不可读（可能正在被回收）：{}",
                cause.message()
            ));
            None
        }
    }
}

/// 用真实工具输出核对冻结视图的内容与 BASE SHA 对应关系。
pub(super) fn assert_view_contents(
    evidence: &mut Evidence,
    fixture: &GitHubFixture,
    outcome: &ProbeOutcome,
) {
    let base_label_output = output_at(outcome, 1);
    evidence.check(
        "t20.project_repo_base_label",
        base_label_output.contains(&fixture.base_label),
        format!(
            "/project/repo 里的 {} 内容是夹具 BASE 标记 `{}`",
            fixture.base_label_path, fixture.base_label
        ),
        format!(
            "/project/repo 的 {} 没有 BASE 标记 `{}`：{}",
            fixture.base_label_path,
            fixture.base_label,
            support::first_line(&base_label_output)
        ),
    );
    let head_label_output = output_at(outcome, 2);
    evidence.check(
        "t20.project_repo_excludes_pr_head",
        head_label_output.contains("head_label_absent"),
        format!(
            "只在 PR head 存在的 {} 在 /project/repo 中不存在（视图不是 PR head）",
            fixture.head_label_path
        ),
        format!(
            "/project/repo 出现了 PR head 专有标记：{}",
            support::first_line(&head_label_output)
        ),
    );
    let workspace_head = output_at(outcome, 3);
    evidence.check(
        "t20.workspace_repo_is_pr_head",
        workspace_head.contains(&fixture.head_sha),
        format!(
            "/workspace/repo 的 HEAD 就是夹具 PR head {}",
            fixture.head_sha
        ),
        format!(
            "/workspace/repo 的 HEAD 不是夹具 PR head {}：{}",
            fixture.head_sha,
            support::first_line(&workspace_head)
        ),
    );
    let base_ref = output_at(outcome, 4);
    evidence.check(
        "t20.project_repo_base_sha_matches_fixture",
        base_ref.contains(&fixture.base_sha),
        format!(
            "真实 fetch 得到的 origin/main 等于夹具 BASE commit {}",
            fixture.base_sha
        ),
        format!(
            "origin/main 不等于夹具 BASE {}：{}",
            fixture.base_sha,
            support::first_line(&base_ref)
        ),
    );
    let base_blob = output_at(outcome, 5);
    evidence.check(
        "t20.base_sha_owns_project_repo_content",
        base_blob.contains(&fixture.base_label),
        format!(
            "夹具 BASE commit 对象里的 {} 内容与 /project/repo 的读取一致（内容与 SHA 同一来源）",
            fixture.base_label_path
        ),
        format!(
            "夹具 BASE commit 对象里读不到 {}：{}",
            fixture.base_label_path,
            support::first_line(&base_blob)
        ),
    );
    let write_exec = output_at(outcome, 7);
    evidence.check(
        "t20.project_repo_is_physically_read_only",
        write_exec.contains("write_rc=") && !write_exec.contains("write_rc=0"),
        format!(
            "容器内 root shell 写 /project/repo 被拒绝：{}",
            support::first_line(&write_exec)
        ),
        format!(
            "容器内写 /project/repo 没有被拒绝（写成功会破坏冻结基线）：{}",
            support::first_line(&write_exec)
        ),
    );
    let repo_read = output_at(outcome, 8);
    match outcome.call(8) {
        Some(call) if call.success == Some(true) => evidence.pass(
            "t20.project_repo_read_file",
            format!(
                "read_file 也能读 /project/repo：{}",
                support::first_line(&repo_read)
            ),
        ),
        Some(call) => evidence.boundary(
            "t20.project_repo_read_file",
            format!(
                "read_file 不能直接读 /project/repo（success={:?}）：reviewer 的 workspace receipt 读根只包含 \
                 /workspace/repo 与已应用引用，/project/repo 的读取走真实 exec；本场景的冻结视图判定使用 exec 证据：{}",
                call.success,
                support::first_line(&repo_read)
            ),
        ),
        None => {}
    }
}

/// 读取某个索引上的真实工具输出。
pub(super) fn output_at(outcome: &ProbeOutcome, index: usize) -> String {
    outcome
        .call(index)
        .map(|call| call.output.clone())
        .unwrap_or_default()
}

/// 视图对比用的挂载目标集合。
pub(super) fn view_targets() -> Vec<String> {
    vec![
        super::super::fixture::REVIEW_CONTAINER_PATH.to_string(),
        crate::fixtures::container_path(crate::fixtures::ALIAS_PRIVATE),
        crate::fixtures::container_path(crate::fixtures::ALIAS_LINUX),
    ]
}

/// 视图事实的 JSON 投影（容器身份 + 关键挂载）。
pub(super) fn view_json(view: &ViewFacts, targets: &[String]) -> serde_json::Value {
    json!({
        "container_id": &view.container_id,
        "reference_targets": crate::host::reference_targets_of(&view.facts),
        "view": support::mounts_json(&view.facts, targets),
    })
}
