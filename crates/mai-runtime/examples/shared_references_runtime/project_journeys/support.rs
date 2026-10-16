//! 项目旅程的共享构件：引用绑定复制、项目创建与就绪等待、真实工具结论断言。

use std::time::Duration;

use mai_docker::{ContainerEnvironmentFacts, ContainerMountFacts};
use mai_protocol::{
    AgentId, CreateProjectRequest, ProjectCloneStatus, ProjectId, ProjectReferenceBinding,
    ProjectReferenceConfiguration, ProjectStatus, ProjectSummary,
};
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::fixtures;
use crate::host::RuntimeHost;
use crate::probe::ProbeOutcome;

use super::fixture::GitHubFixture;

/// 就绪轮询间隔。
const READY_POLL: Duration = Duration::from_millis(500);

/// 已创建项目的身份。
#[derive(Debug, Clone)]
pub struct ProjectHandles {
    pub project: ProjectId,
    pub maintainer: AgentId,
    pub summary: ProjectSummary,
}

/// 从 project A 夹具配置复制 linux/private-fixture 绑定：source 不变，绑定 id 换新。
///
/// 新项目的引用必须指向**已登记**的同一个参考源，否则产品会在 `catalog.snapshot` 预检拒绝；
/// 这里显式复用夹具 source_id，并把这一步作为证据记录。
pub fn reference_bindings(
    configuration: &ProjectReferenceConfiguration,
) -> Result<Vec<ProjectReferenceBinding>> {
    let mut bindings = Vec::new();
    for alias in [fixtures::ALIAS_LINUX, fixtures::ALIAS_PRIVATE] {
        let binding = fixtures::binding(configuration, alias).ok_or_else(|| {
            error(
                "fixture_binding_missing",
                format!("project A 夹具缺少 alias `{alias}` 的绑定，无法为验收项目构造引用"),
            )
        })?;
        if !binding.enabled {
            return Err(error(
                "fixture_binding_disabled",
                format!("project A 夹具的 alias `{alias}` 处于禁用状态"),
            ));
        }
        bindings.push(ProjectReferenceBinding {
            id: uuid::Uuid::new_v4(),
            source_id: binding.source_id,
            alias: binding.alias.clone(),
            description: binding.description.clone(),
            enabled: true,
        });
    }
    Ok(bindings)
}

/// 经公开产品入口创建项目；`reference_bindings` 就是"创建时已经带引用"的请求。
pub async fn create_project(
    host: &RuntimeHost,
    fixture: &GitHubFixture,
    tag: &str,
    reference_bindings: Vec<ProjectReferenceBinding>,
) -> Result<ProjectSummary> {
    let request = CreateProjectRequest {
        reference_bindings,
        name: format!("acceptance-project-{tag}"),
        git_account_id: None,
        installation_id: fixture.installation_id,
        repository_id: 0,
        repository_full_name: Some(fixture.repository_full_name.clone()),
        owner: fixture.owner.clone(),
        repo: fixture.name.clone(),
        branch: Some(fixture.default_branch.clone()),
        docker_image: Some(host.config.agent_image.clone()),
        auto_review_enabled: true,
        reviewer_extra_prompt: Some(format!(
            "隔离验收运行 {tag}：只使用 /project/repo 与 /workspace/repo 的固定视图。"
        )),
    };
    host.runtime.create_project(request).await.map_err(|cause| {
        error(
            "project_create_failed",
            format!("create_project 未受理（GitHub 夹具/引用预检/持久化）：{cause}"),
        )
    })
}

/// 等待项目克隆与首次容器装配完成（真实状态，不猜测）。
pub async fn wait_ready(
    host: &RuntimeHost,
    project_id: ProjectId,
    limit: Duration,
) -> Result<ProjectSummary> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let detail = host
            .runtime
            .get_project(project_id)
            .await
            .map_err(|cause| {
                error(
                    "project_read_failed",
                    format!("读取项目 {project_id} 失败：{cause}"),
                )
            })?;
        let summary = detail.summary;
        match (&summary.status, &summary.clone_status) {
            (ProjectStatus::Ready, ProjectCloneStatus::Ready) => return Ok(summary),
            (
                ProjectStatus::Failed,
                ProjectCloneStatus::Pending
                | ProjectCloneStatus::Cloning
                | ProjectCloneStatus::Ready
                | ProjectCloneStatus::Failed,
            )
            | (
                ProjectStatus::Creating | ProjectStatus::Ready | ProjectStatus::Deleting,
                ProjectCloneStatus::Failed,
            ) => {
                return Err(error(
                    "project_workspace_failed",
                    format!(
                        "项目 {project_id} 进入失败终态（clone={}）：{}",
                        summary.clone_status,
                        summary.last_error.as_deref().unwrap_or("没有错误详情")
                    ),
                ));
            }
            (
                ProjectStatus::Creating | ProjectStatus::Deleting,
                ProjectCloneStatus::Pending
                | ProjectCloneStatus::Cloning
                | ProjectCloneStatus::Ready,
            )
            | (ProjectStatus::Ready, ProjectCloneStatus::Pending | ProjectCloneStatus::Cloning) => {
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error(
                "project_ready_timeout",
                format!(
                    "等待项目 {project_id} 就绪超时（{limit:?}）：status={} clone={}",
                    summary.status, summary.clone_status
                ),
            ));
        }
        sleep(READY_POLL).await;
    }
}

/// 真实容器里某个目标路径的挂载事实。
pub fn mount<'a>(
    facts: &'a ContainerEnvironmentFacts,
    target: &str,
) -> Option<&'a ContainerMountFacts> {
    facts.mounts.iter().find(|mount| mount.target == target)
}

/// 把若干挂载事实投影成 JSON（用于"更新前后不变"的对比与证据）。
pub fn mounts_json(facts: &ContainerEnvironmentFacts, targets: &[String]) -> Value {
    let mut collected = serde_json::Map::new();
    for target in targets {
        let value = match mount(facts, target) {
            Some(mount) => json!(mount),
            None => Value::Null,
        };
        collected.insert(target.clone(), value);
    }
    Value::Object(collected)
}

/// 按序号断言期望的工具调用结果；未进入终态或没有 trace 一律 fail。
pub fn assert_calls(
    evidence: &mut Evidence,
    prefix: &str,
    outcome: &ProbeOutcome,
    expected: &[(&str, bool)],
) {
    if !outcome.terminal {
        evidence.fail(
            format!("{prefix}.turn_terminal"),
            format!(
                "probe {} 没有进入终态（state={} thread={}）",
                outcome.probe_id, outcome.turn_state, outcome.thread_status
            ),
        );
        return;
    }
    for (index, (label, want_success)) in expected.iter().enumerate() {
        let Some(call) = outcome.call(index) else {
            evidence.fail(
                format!("{prefix}.{label}"),
                format!("probe {} 缺少第 {index} 条调用的结论", outcome.probe_id),
            );
            continue;
        };
        match (call.observed, call.success) {
            (false, _) => evidence.fail(
                format!("{prefix}.{label}"),
                format!(
                    "真实工具 trace 里没有 {}（期望 call_id {}）；probe state={}",
                    call.name, call.expected_call_id, outcome.turn_state
                ),
            ),
            (true, Some(success)) if success == *want_success => evidence.pass(
                format!("{prefix}.{label}"),
                format!(
                    "{} 真实执行 success={success}：{}",
                    call.name,
                    first_line(&call.output_preview)
                ),
            ),
            (true, Some(success)) => evidence.fail(
                format!("{prefix}.{label}"),
                format!(
                    "{} 真实执行 success={success}，期望 {want_success}：{}",
                    call.name,
                    first_line(&call.output_preview)
                ),
            ),
            (true, None) => evidence.fail(
                format!("{prefix}.{label}"),
                format!("{} 有 trace 但没有终态详情", call.name),
            ),
        }
    }
}

/// 单行预览，便于证据阅读。
pub fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.len() > 300 {
        format!("{}…", line.chars().take(300).collect::<String>())
    } else {
        line.to_string()
    }
}
