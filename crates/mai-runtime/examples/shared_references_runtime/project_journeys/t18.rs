//! T18：创建项目时请求就带 linux/private-fixture 引用，Maintainer 首次工具即可读。
//!
//! 全程走真实入口：`AgentRuntime::create_project`（含 `catalog.snapshot` 预检）→ 真实克隆 →
//! `start_reference_environment` 首次容器装配 → 真实注册的 `read_file` 工具。
//! 判定证据只有两类：真实 Docker 挂载事实与真实工具 trace。

use mai_protocol::{ProjectReferenceApplicationState, ReferenceStatusQuery};
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::{self, RuntimeHost};
use crate::probe::{self, Probe, ProbeCall};

use super::fixture::GitHubFixture;
use super::support::{self, ProjectHandles};

/// T18 场景；夹具/入口不可用时返回 `None`，由上层跳过 T20。
pub async fn scenario(
    host: &RuntimeHost,
    fixture: &GitHubFixture,
    evidence: &mut Evidence,
) -> Result<Option<ProjectHandles>> {
    let tag = probe::run_tag();
    let fixture_configuration =
        fixtures::project_configuration(&host.store, ProjectKind::A).await?;
    if let Err(cause) = fixtures::ensure_project_a_contract(&fixture_configuration) {
        evidence.blocked(
            "t18.fixture_contract",
            format!("project A 夹具契约不满足：{}", cause.message()),
        );
        return Ok(None);
    }
    evidence.pass(
        "t18.fixture_contract",
        "project A 绑定 linux + private-fixture 且都启用，可作为新项目引用的登记源",
    );
    let bindings = support::reference_bindings(&fixture_configuration)?;
    let expected_linux = fixtures::binding(&fixture_configuration, fixtures::ALIAS_LINUX)
        .map(|binding| binding.source_id);
    let expected_private = fixtures::binding(&fixture_configuration, fixtures::ALIAS_PRIVATE)
        .map(|binding| binding.source_id);
    evidence.fact("t18.request_reference_bindings", json!(&bindings));

    let created = match support::create_project(host, fixture, &tag, bindings).await {
        Ok(project) => project,
        Err(cause) => {
            evidence.fail("t18.project_created", cause.message());
            return Ok(None);
        }
    };
    evidence.fact("t18.project_summary", json!(&created));
    evidence.pass(
        "t18.project_created",
        format!(
            "create_project 受理并返回项目 {}（{}），仓库 {}/{}，auto_review={}",
            created.id, created.name, created.owner, created.repo, created.auto_review_enabled
        ),
    );

    // 1) 创建请求里的引用已经持久化，且指向夹具登记的同一批参考源。
    let stored = host
        .runtime
        .project_reference_configuration(created.id)
        .await?;
    evidence.fact("t18.project_configuration", json!(&stored));
    let linux = fixtures::binding(&stored, fixtures::ALIAS_LINUX);
    let private = fixtures::binding(&stored, fixtures::ALIAS_PRIVATE);
    let source_matches = |binding: Option<&mai_protocol::ProjectReferenceBinding>,
                          expected: Option<uuid::Uuid>| {
        binding.is_some_and(|binding| {
            binding.enabled && !binding.alias.is_empty() && Some(binding.source_id) == expected
        })
    };
    evidence.check(
        "t18.creation_request_carries_references",
        source_matches(linux, expected_linux) && source_matches(private, expected_private),
        format!(
            "新项目的持久化配置带 linux/private-fixture 两个启用绑定，source 与夹具一致（revision {}）",
            stored.revision
        ),
        format!(
            "新项目配置缺少期望的引用：linux={linux:?} private={private:?}（期望 source {:?}/{:?}）",
            expected_linux, expected_private
        ),
    );

    // 2) 真实克隆 + 首次容器装配完成。
    let summary = match support::wait_ready(host, created.id, host.config.operation_timeout).await {
        Ok(summary) => summary,
        Err(cause) => {
            evidence.fail("t18.project_ready", cause.message());
            return Ok(None);
        }
    };
    evidence.fact("t18.project_ready", json!(&summary));
    evidence.pass(
        "t18.project_ready",
        format!(
            "项目 {} 真实就绪（clone={}，maintainer={}）",
            summary.id, summary.clone_status, summary.maintainer_agent_id
        ),
    );

    let handles = ProjectHandles {
        project: created.id,
        maintainer: summary.maintainer_agent_id,
        summary: summary.clone(),
    };

    // 3) Maintainer 的首次物理 binding：不发任何预热 turn，直接看真实挂载。
    let linux_target = fixtures::container_path(fixtures::ALIAS_LINUX);
    let private_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
    let Some(container_id) = host.container_id(handles.maintainer).await? else {
        evidence.fail(
            "t18.maintainer_binding_published",
            format!(
                "Maintainer {} 没有发布的执行环境绑定，首次工具无法判定",
                handles.maintainer
            ),
        );
        return Ok(Some(handles));
    };
    let facts = host.container_facts(&container_id).await?;
    let targets = host::reference_targets_of(&facts);
    let published = host
        .store
        .reference_binding(handles.maintainer)
        .await
        .map_err(|cause| {
            crate::error::error(
                "binding_unreadable",
                format!("读取 Maintainer 的执行环境绑定失败：{cause}"),
            )
        })?;
    evidence.fact(
        "t18.maintainer_container",
        json!({
            "container_id": &container_id,
            "generation": facts.generation,
            "image_reference": &facts.image_reference,
            "reference_targets": &targets,
            "published_binding": published.as_ref().map(|binding| json!({
                "generation": binding.generation,
                "container_id": &binding.container_id,
                "applied_revision": binding.applied_revision,
                "fingerprint": &binding.fingerprint,
            })),
            "reference_mounts": support::mounts_json(
                &facts,
                &[linux_target.clone(), private_target.clone()],
            ),
        }),
    );
    evidence.check(
        "t18.maintainer_binding_published",
        published.as_ref().is_some_and(|binding| {
            binding.container_id == container_id && Some(binding.generation) == facts.generation
        }),
        format!(
            "Maintainer {} 的首次装配已发布物理容器 {container_id}，绑定与容器事实一致",
            handles.maintainer
        ),
        format!(
            "Maintainer 的持久绑定与真实容器不一致：binding={:?} container={container_id}",
            published
                .as_ref()
                .map(|binding| binding.container_id.clone())
        ),
    );
    evidence.check(
        "t18.maintainer_reference_mounts",
        targets.contains(&linux_target) && targets.contains(&private_target),
        format!("首次容器挂载包含 {linux_target} 与 {private_target}"),
        format!("首次容器挂载缺少参考路径：{targets:?}"),
    );
    let read_only = support::mount(&facts, &linux_target)
        .zip(support::mount(&facts, &private_target))
        .is_some_and(|(linux, private)| linux.read_only && private.read_only);
    evidence.check(
        "t18.maintainer_reference_mounts_read_only",
        read_only,
        "两个参考源挂载都是只读（Docker 事实）",
        "参考源挂载不是只读，基线不可信",
    );

    // 4) 首次工具：真实 read_file（不预热、不改夹具）。
    let read = Probe::new(
        format!("t18-maintainer-first-{tag}"),
        vec![
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{private_target}/README.md") }),
            ),
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{linux_target}/Makefile") }),
            ),
        ],
    );
    let outcome = probe::run(host, handles.maintainer, &read, host.config.turn_timeout).await?;
    evidence.fact("t18.maintainer_first_read", json!(&outcome));
    support::assert_calls(
        evidence,
        "t18.maintainer_first_tool_read",
        &outcome,
        &[
            ("read_file(private-fixture/README.md)", true),
            ("read_file(linux/Makefile)", true),
        ],
    );

    // 5) 项目参考应用状态：只记录事实并核对没有失败。
    let status = host
        .runtime
        .project_reference_status(created.id, ReferenceStatusQuery::default())
        .await?;
    let applied = status.state == ProjectReferenceApplicationState::Applied;
    let failed = status.counts.failed;
    evidence.fact("t18.reference_status", json!(&status));
    evidence.check(
        "t18.reference_application_applied",
        applied && failed == 0,
        format!(
            "项目引用状态 {}（applied={} waiting={} failed={}）",
            status.state, status.counts.applied, status.counts.waiting, failed
        ),
        format!(
            "项目引用没有收敛到 applied：state={} failed={}",
            status.state, failed
        ),
    );

    Ok(Some(handles))
}
