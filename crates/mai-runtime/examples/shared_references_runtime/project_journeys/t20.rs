//! T20：真实自动 PR 审查 —— Reviewer 可读引用、`/project/repo` 冻结在夹具 BASE 且只读。
//!
//! 触发方式全部是产品公开入口：`AgentRuntime::enqueue_project_review`（项目本身也已开启
//! `auto_review_enabled`，discovery 调度会扫描同一个 PR）。随后真实审查生命周期创建 Reviewer
//! Agent 与容器，装配 `/project/repo`（缓存卷只读子路径）与 `/references/<alias>`。
//!
//! Reviewer 的初始消息由产品生成，不是 probe，所以 fixture provider 只会文本收尾；驱动在
//! Reviewer 拿到容器后**再发一条真实 probe 消息**，用它触发的真实 `read_file`/`exec`/`apply_patch`
//! 工具读取挂载。probe 带 provider 侧延迟，使冻结视图的读取发生在引用 ensure/更新之后，
//! 而 Reviewer 仍在审查生命周期内。

#[path = "t20/references.rs"]
mod references;
#[path = "t20/review_view.rs"]
mod review_view;

use mai_docker::project_cache_volume;
use mai_protocol::UpdateProjectRequest;
use mai_runtime::ProjectReviewQueueRequest;
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::fixtures;
use crate::host::RuntimeHost;
use crate::probe::{self, Probe, ProbeCall};

use super::fixture::{self, GitHubFixture};
use super::reviewer;
use super::support::{self, ProjectHandles};
use review_view::{ViewFacts, view_facts, view_json, view_targets};

/// probe 在 provider 侧的延迟：它让 Reviewer 的 turn 一直处于运行态，
/// 使审查生命周期不会先回收 Agent，引用 ensure/更新得以及时插入同一窗口。
const PROBE_WINDOW_SECS: u64 = 20;

/// T20 场景。
pub async fn scenario(
    host: &RuntimeHost,
    fixture: &GitHubFixture,
    created: &ProjectHandles,
    evidence: &mut Evidence,
) -> Result<()> {
    let tag = probe::run_tag();
    let project_id = created.project;

    // 创建与并发生命周期场景先不启动审查；在本场景通过产品入口启用调度。
    let enabled = host
        .runtime
        .update_project(
            project_id,
            UpdateProjectRequest {
                auto_review_enabled: Some(true),
                ..Default::default()
            },
        )
        .await?;
    evidence.fact(
        "t20.auto_review_enabled",
        json!(enabled.auto_review_enabled),
    );
    // 1) 真实自动审查触发。
    let queue = host
        .runtime
        .enqueue_project_review(ProjectReviewQueueRequest {
            project_id,
            pr: fixture.pull_request,
            head_sha: Some(fixture.head_sha.clone()),
            delivery_id: Some(format!("acceptance-{tag}")),
            reason: "acceptance".to_string(),
        })
        .await?;
    evidence.fact(
        "t20.review_queue",
        json!({
            "queued": &queue.queued,
            "deduped": &queue.deduped,
            "ignored": &queue.ignored,
            "jobs": queue.jobs.iter().map(|job| json!({
                "id": job.id,
                "pr": job.pr,
                "head_sha": &job.head_sha,
                "source": job.source,
                "reason": &job.reason,
                "status": job.status,
                "attempt_count": job.attempt_count,
            })).collect::<Vec<_>>(),
        }),
    );
    evidence.check(
        "t20.review_signal_accepted",
        !queue.queued.is_empty() || !queue.deduped.is_empty(),
        format!(
            "自动审查信号被受理：queued={:?} deduped={:?}（同一 PR/head 只会有一个 Job）",
            queue.queued, queue.deduped
        ),
        format!(
            "自动审查信号被忽略：ignored={:?}；项目 auto_review_enabled={}",
            queue.ignored, created.summary.auto_review_enabled
        ),
    );
    let job = queue.jobs.first().cloned();
    if let Some(job) = job.as_ref() {
        evidence.check(
            "t20.review_job_targets_fixture_head",
            job.head_sha == fixture.head_sha && job.pr == fixture.pull_request,
            format!(
                "审查 Job {} 对准夹具 PR #{} head {}",
                job.id, job.pr, job.head_sha
            ),
            format!(
                "审查 Job 目标与夹具不一致：pr={} head={}（夹具 PR #{} head {}）",
                job.pr, job.head_sha, fixture.pull_request, fixture.head_sha
            ),
        );
    }

    // 2) 等真实 Reviewer Agent 与它的执行环境。
    let Some(reviewer) = reviewer::wait_for_reviewer(
        host,
        project_id,
        host.config
            .operation_timeout
            .min(std::time::Duration::from_secs(90)),
    )
    .await?
    else {
        record_runs(host, project_id, evidence, job.as_ref()).await;
        evidence.fail(
            "t20.reviewer_created",
            format!(
                "在 {:?} 内没有观察到带真实容器的 Reviewer Agent；自动审查生命周期没有把 Job 推进到 Reviewer 装配，\
                 真实原因见 t20.review_runs / t20.review_job 事实（不把缺少 Reviewer 当作通过）",
                host.config.operation_timeout
            ),
        );
        return Ok(());
    };
    evidence.fact(
        "t20.reviewer",
        json!({
            "agent_id": reviewer.id,
            "name": &reviewer.name,
            "role": reviewer.role,
            "project_id": reviewer.project_id,
            "review_run_id": reviewer.review_run_id,
            "container_id": &reviewer.container_id,
            "model": &reviewer.model,
            "provider_id": &reviewer.provider_id,
        }),
    );
    evidence.pass(
        "t20.reviewer_created",
        format!(
            "真实 Reviewer {}（run {:?}）已创建并发布容器 {:?}",
            reviewer.id, reviewer.review_run_id, reviewer.container_id
        ),
    );

    // 3) 冻结视图的物理事实（引用 ensure 之前）。
    let private_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
    let review_path = fixture::REVIEW_CONTAINER_PATH;
    let base_label_path = fixture.base_label_container_path();
    let head_label_path = fixture.head_label_container_path();
    let targets = view_targets();
    let Some(before): Option<ViewFacts> = view_facts(host, reviewer.id, evidence).await else {
        evidence.fail(
            "t20.reviewer_view_available",
            "Reviewer 容器事实不可读，冻结视图无法判定",
        );
        return Ok(());
    };
    let view_before = support::mounts_json(&before.facts, &targets);
    evidence.fact("t20.reviewer_view_before", view_json(&before, &targets));
    let cache_volume = project_cache_volume(&project_id.to_string());
    let expected_subpath = reviewer
        .review_run_id
        .map(|run_id| fixture::review_snapshot_subpath(&run_id.to_string()));
    let review_mount = support::mount(&before.facts, review_path);
    evidence.check(
        "t20.project_repo_mount_is_read_only_snapshot",
        review_mount.is_some_and(|mount| {
            mount.read_only
                && mount.volume_name.as_deref() == Some(cache_volume.as_str())
                && expected_subpath
                    .as_deref()
                    .is_none_or(|expected| mount.volume_subpath.as_deref() == Some(expected))
        }),
        format!(
            "/project/repo 是项目缓存卷 {cache_volume} 的只读子路径快照（run {:?}）",
            reviewer.review_run_id
        ),
        format!(
            "/project/repo 挂载不符合只读快照契约：{review_mount:?}（期望卷 {cache_volume}，子路径 {expected_subpath:?}）"
        ),
    );
    evidence.check(
        "t20.reviewer_reference_mounts",
        crate::host::reference_targets_of(&before.facts).contains(&private_target),
        format!("Reviewer 容器挂载包含 {private_target}（项目引用随环境装配）"),
        format!(
            "Reviewer 容器缺少 {private_target}：{:?}",
            crate::host::reference_targets_of(&before.facts)
        ),
    );

    // 4) 真实 probe：引用可读 + 冻结视图内容 + 只读；provider 侧延迟给我们插入 ensure 的窗口。
    let probe = Probe::new(
        format!("t20-reviewer-view-{tag}"),
        vec![
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{private_target}/README.md") }),
            ),
            ProbeCall::new(
                "exec",
                json!({ "command": format!("cat {base_label_path}") }),
            ),
            ProbeCall::new(
                "exec",
                json!({
                    "command": format!(
                        "if test -e {head_label_path}; then echo head_label_present; else echo head_label_absent; fi"
                    )
                }),
            ),
            ProbeCall::new(
                "exec",
                json!({ "command": "cd /workspace/repo && git rev-parse HEAD" }),
            ),
            ProbeCall::new(
                "exec",
                json!({ "command": "cd /workspace/repo && git rev-parse origin/main" }),
            ),
            ProbeCall::new(
                "exec",
                json!({
                    "command": format!(
                        "cd /workspace/repo && git cat-file -p origin/main:{}",
                        fixture.base_label_path
                    )
                }),
            ),
            ProbeCall::new(
                "apply_patch",
                json!({
                    "input": format!(
                        "*** Begin Patch\n*** Add File: {review_path}/runtime-write-probe.txt\n+probe write attempt\n*** End Patch"
                    )
                }),
            ),
            ProbeCall::new(
                "exec",
                json!({
                    "command": format!("touch {review_path}/runtime-exec-probe.txt; echo write_rc=$?")
                }),
            ),
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{review_path}/README.md") }),
            ),
        ],
    )
    .with_provider_delay(PROBE_WINDOW_SECS);
    let input_id = match reviewer::send(host, reviewer.id, &probe).await {
        Ok(input_id) => input_id,
        Err(cause) => {
            evidence.fail("t20.reviewer_probe_accepted", cause.message());
            record_runs(host, project_id, evidence, job.as_ref()).await;
            return Ok(());
        }
    };
    evidence.fact("t20.reviewer_probe_input", json!(&input_id));

    // 5) 窗口内：引用更新/ensure，并核对 Reviewer 的冻结视图物理事实没有变化。
    references::ensure(host, project_id, evidence).await;
    match view_facts(host, reviewer.id, evidence).await {
        Some(after) => {
            let view_after = support::mounts_json(&after.facts, &targets);
            evidence.fact("t20.reviewer_view_after", view_json(&after, &targets));
            evidence.check(
                "t20.reference_update_keeps_review_view",
                after.container_id == before.container_id && view_after == view_before,
                format!(
                    "引用更新/ensure 后 Reviewer 视图不变（容器 {}，/project/repo 与引用挂载事实逐字段一致）",
                    after.container_id
                ),
                format!(
                    "引用更新/ensure 改变了 Reviewer 视图：容器 {} -> {}，挂载 {} -> {}",
                    before.container_id, after.container_id, view_before, view_after
                ),
            );
        }
        None => evidence.boundary(
            "t20.reference_update_keeps_review_view",
            "引用更新/ensure 之后 Reviewer 已被审查生命周期回收，无法再次读取物理事实；\
             视图不变的结论只由 ensure 之前的挂载事实与 probe 的真实读取支撑",
        ),
    }

    // 6) 收集真实工具结论。
    let collected = reviewer::collect(
        host,
        reviewer.id,
        &probe,
        &input_id,
        host.config.turn_timeout,
    )
    .await?;
    evidence.fact("t20.reviewer_probe", json!(&collected.outcome));
    if let Some(disappeared) = collected.disappeared.as_deref() {
        evidence.limitation(format!(
            "Reviewer 在收集 probe 结论时被审查生命周期回收（{disappeared}）；已观察到的真实工具结论仍然有效"
        ));
    }
    support::assert_calls(
        evidence,
        "t20.reviewer_first_tool",
        &collected.outcome,
        &[
            ("read_file(/references/private-fixture/README.md)", true),
            ("exec(cat /project/repo 基线标记)", true),
            ("exec(/project/repo 无 PR head 标记)", true),
            ("exec(git rev-parse HEAD@/workspace/repo)", true),
            ("exec(git rev-parse origin/main@/workspace/repo)", true),
            ("exec(git cat-file origin/main:基线标记)", true),
            ("apply_patch(/project/repo) 必须被拒", false),
            ("exec(touch /project/repo) 必须被拒", true),
        ],
    );
    review_view::assert_view_contents(evidence, fixture, &collected.outcome);

    record_runs(host, project_id, evidence, job.as_ref()).await;
    evidence.limitation(
        "T20 只记录审查 Job/Run 的即时持久化状态，不等待其终态：fixture provider 对产品生成的初始消息只做文本收尾，\
         真实 Reviewer 无法产出最终 JSON，Job 会按产品策略重试/失败。本场景判定的是 Reviewer 装配、引用可读与冻结视图，\
         不是评审提交闭环。"
            .to_string(),
    );
    evidence.limitation(
        "本场景没有触发任何 GitHub 评审提交：夹具的提交端点只写本地日志（<root>/fixture/requests）。\
         若 root 需要覆盖提交路径，应让夹具 provider 在 Reviewer 的第一轮工具调用里发出 gh api 提交，\
         并由夹具核对 marker/head，而不是由驱动伪造提交结果。"
            .to_string(),
    );
    stop_auto_review(host, project_id, evidence).await;
    Ok(())
}

/// 记录持久化的审查 Run/Job 事实（不参与成败判定，只供 root 核对调度终态）。
async fn record_runs(
    host: &RuntimeHost,
    project_id: mai_protocol::ProjectId,
    evidence: &mut Evidence,
    job: Option<&mai_protocol::ProjectReviewJobSummary>,
) {
    match host
        .runtime
        .list_project_review_runs(project_id, 0, 20)
        .await
    {
        Ok(runs) => evidence.fact("t20.review_runs", json!(&runs)),
        Err(cause) => evidence.limitation(format!("读取审查 Run 列表失败：{cause}")),
    }
    if let Some(job) = job {
        match host
            .runtime
            .get_project_review_job(project_id, job.id)
            .await
        {
            Ok(detail) => evidence.fact("t20.review_job", json!(&detail.summary)),
            Err(cause) => evidence.limitation(format!("读取审查 Job 详情失败：{cause}")),
        }
    }
}

/// 关闭自动审查，避免隔离库在验收结束后继续重试同一个夹具 PR。
async fn stop_auto_review(
    host: &RuntimeHost,
    project_id: mai_protocol::ProjectId,
    evidence: &mut Evidence,
) {
    let request = UpdateProjectRequest {
        auto_review_enabled: Some(false),
        ..Default::default()
    };
    match host.runtime.update_project(project_id, request).await {
        Ok(summary) => evidence.fact(
            "t20.auto_review_disabled",
            json!({
                "auto_review_enabled": summary.auto_review_enabled,
                "review_status": summary.review_status,
            }),
        ),
        Err(cause) => evidence.limitation(format!(
            "验收结束时关闭自动审查失败（{cause}）；隔离库可能继续重试同一夹具 PR"
        )),
    }
}
