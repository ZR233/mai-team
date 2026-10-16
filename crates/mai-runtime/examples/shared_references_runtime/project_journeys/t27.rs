//! T27：项目引用换代时保留镜像声明匿名卷、工作区与可写层数据。
//!
//! 该场景接在 T18 之后、T20 之前：新项目必须从创建就使用带
//! `Config.Volumes[/opt/mai-acceptance-anon]` 的固定镜像。写入、换代与读取全部走
//! 真实产品入口和已注册 exec 工具；本模块只记录事实，不伪造 Docker 收据。

#[path = "t27/assertions.rs"]
mod assertions;
#[path = "t27/environment.rs"]
mod environment;
#[path = "t27/lifecycle.rs"]
mod lifecycle;
#[path = "t27/probes.rs"]
mod probes;
#[path = "t27/report.rs"]
mod report;
#[path = "t27/settings.rs"]
mod settings;
#[path = "t27/verification.rs"]
mod verification;

use mai_protocol::{
    ApplyProjectReferencesRequest, ProjectReferenceBinding, ProjectReferenceConfiguration,
    ReferenceApplyPolicy, ReplaceProjectReferencesRequest,
};
use serde_json::json;

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe;

use super::support::{self, ProjectHandles};
use assertions::checked;
use lifecycle::wait_for_applied;
use report::record_environment;
use settings::{ANON_VOLUME_TARGET, EXPECTED_IMAGE_ID};

/// T27 场景。任何失败都会显式 fail/error，且外层仍尝试还原原 alias。
pub async fn scenario(
    host: &RuntimeHost,
    handles: &ProjectHandles,
    evidence: &mut Evidence,
) -> Result<()> {
    let original = host
        .runtime
        .project_reference_configuration(handles.project)
        .await
        .map_err(|cause| {
            error(
                "t27_configuration_unreadable",
                format!("读取项目 {} 的引用配置失败：{cause}", handles.project),
            )
        })?;
    evidence.fact("t27.original_configuration", json!(&original));

    let executed = execute(host, handles, &original, evidence).await;
    let restored = lifecycle::restore_original(host, handles, &original, evidence).await;
    match (executed, restored) {
        (Err(cause), Err(restore_error)) => Err(error(
            "t27_failure_and_restore_failed",
            format!(
                "T27 已失败且还原失败；原失败：{}；还原失败：{restore_error}",
                cause.message()
            ),
        )),
        (Err(cause), Ok(())) => Err(cause),
        (Ok(()), Err(cause)) => Err(cause),
        (Ok(()), Ok(())) => {
            evidence.pass(
                "t27.completed",
                "真实引用换代保留了 workspace、镜像声明匿名卷与可写层数据，且原 alias 已还原",
            );
            Ok(())
        }
    }
}

/// 主体流程独立出来，保证后续还原逻辑总能执行。
async fn execute(
    host: &RuntimeHost,
    handles: &ProjectHandles,
    original: &ProjectReferenceConfiguration,
    evidence: &mut Evidence,
) -> Result<()> {
    let tag = probe::run_tag();
    let expected_workspace = mai_docker::project_agent_workspace_volume(
        &handles.project.to_string(),
        &handles.maintainer.to_string(),
    );

    // 1) 只认 root 固定镜像。项目 summary 与 daemon 的基础镜像 ID 必须同时指向它。
    let base_image_id = host
        .docker
        .image_id(EXPECTED_IMAGE_ID)
        .await
        .map_err(|cause| {
            error(
                "t27_base_image_unavailable",
                format!("Docker 无法 inspect 固定验收镜像 {EXPECTED_IMAGE_ID}：{cause}"),
            )
        })?;
    evidence.fact(
        "t27.image_contract",
        json!({
            "expected_image_id": EXPECTED_IMAGE_ID,
            "daemon_image_id": &base_image_id,
            "project_image": &handles.summary.docker_image,
            "expected_config_volumes": { "/opt/mai-acceptance-anon": {} },
        }),
    );
    let image_matches =
        handles.summary.docker_image == EXPECTED_IMAGE_ID && base_image_id == EXPECTED_IMAGE_ID;
    if !checked(
        evidence,
        "t27.project_uses_acceptance_image",
        image_matches,
        format!("新项目从创建即使用固定镜像 {EXPECTED_IMAGE_ID}（daemon 解析为 {base_image_id}）"),
        format!(
            "新项目镜像不是固定验收镜像：project={} daemon={base_image_id}",
            handles.summary.docker_image
        ),
    ) {
        return Err(error(
            "t27_project_image_mismatch",
            format!(
                "项目镜像 {} / daemon 镜像 {base_image_id} 不等于 {EXPECTED_IMAGE_ID}",
                handles.summary.docker_image
            ),
        ));
    }

    // 2) 换代前的真实容器、卷身份与 DockerClient 环境核验。
    let before_container = host
        .container_id(handles.maintainer)
        .await?
        .ok_or_else(|| error("t27_container_missing", "Maintainer 没有已发布容器"))?;
    let before = environment::inspect_binding(host, handles.maintainer, &before_container).await?;
    record_environment(evidence, "t27.before_environment", &before);

    let workspace_before = environment::volume_identity(&before.facts, "/workspace");
    let workspace_initial = workspace_before
        .clone()
        .is_some_and(|identity| identity.matches_name(&expected_workspace));
    let anon_before = environment::volume_identity(&before.facts, ANON_VOLUME_TARGET);
    let anon_initial = anon_before
        .as_ref()
        .is_some_and(|identity| identity.is_generated_anonymous() && identity.read_write);
    let before_verified = matches!(
        before.inspected,
        mai_docker::ExistingEnvironment::Matching(_)
    );
    let mut valid = checked(
        evidence,
        "t27.initial_workspace_canonical_volume",
        workspace_initial,
        format!("换代前 /workspace 是 canonical 卷 {expected_workspace}"),
        format!(
            "换代前 /workspace 不是 canonical 卷 {expected_workspace}：{:?}",
            workspace_before
        ),
    );
    valid &= checked(
        evidence,
        "t27.image_declared_anonymous_volume_active",
        anon_initial,
        format!(
            "固定镜像在 {ANON_VOLUME_TARGET} 生成了 Docker 匿名卷：{:?}",
            anon_before
        ),
        format!(
            "{ANON_VOLUME_TARGET} 缺失镜像声明匿名卷，实际挂载：{:?}",
            anon_before
        ),
    );
    valid &= checked(
        evidence,
        "t27.before_environment_matches_plan",
        before_verified
            && before.facts.image_id == base_image_id
            && before.binding.effective_image_id == before.facts.image_id
            && before.binding.container_id == before_container
            && before.facts.generation == Some(before.binding.generation),
        format!(
            "DockerClient.inspect_environment 核验旧容器 {} 符合计划，且 effective image 是基础镜像",
            before.facts.id
        ),
        format!(
            "旧容器环境或绑定身份核验失败（matching={before_verified} image={} expected={base_image_id} container={} binding={} generation={:?}）",
            before.facts.image_id,
            before_container,
            before.binding.container_id,
            before.binding.generation
        ),
    );
    if !valid {
        return Err(error(
            "t27_initial_environment_invalid",
            "项目初始容器缺少 canonical workspace、镜像声明匿名卷，或环境计划核验失败",
        ));
    }

    // 3) 只通过已注册 exec 工具写入三类数据。基础工具缺失时真实失败，不降级。
    let availability = probe::run(
        host,
        handles.maintainer,
        &probes::availability(&tag),
        host.config.turn_timeout,
    )
    .await?;
    evidence.fact("t27.debian_tools_probe", json!(&availability));
    support::assert_calls(
        evidence,
        "t27.debian_tools_available",
        &availability,
        &[("exec(command -v dpkg-deb/dpkg)", true)],
    );
    if !availability
        .call(0)
        .is_some_and(|call| call.observed && call.success == Some(true))
    {
        return Err(error(
            "t27_debian_tools_missing",
            "验收镜像缺少 dpkg-deb 或 dpkg，无法执行真实本地包安装",
        ));
    }

    let write = probe::run(
        host,
        handles.maintainer,
        &probes::write_data(&tag),
        host.config.turn_timeout,
    )
    .await?;
    evidence.fact("t27.write_probe", json!(&write));
    support::assert_calls(
        evidence,
        "t27.write_three_data_classes",
        &write,
        &[("exec(写 /usr/local + 安装 deb + workspace + anon)", true)],
    );
    if !write
        .call(0)
        .is_some_and(|call| call.observed && call.success == Some(true))
    {
        return Err(error(
            "t27_data_write_failed",
            "真实 exec 未完成三类数据写入或本地 deb 安装失败",
        ));
    }

    // 4) CAS 改 private-fixture alias，显式 apply 并等待真实收敛。
    let private_binding_present = original
        .bindings
        .iter()
        .any(|binding| binding.alias == crate::fixtures::ALIAS_PRIVATE);
    if !private_binding_present {
        evidence.fail(
            "t27.private_binding_present",
            "项目缺少 private-fixture 绑定，无法构造引用换代",
        );
        return Err(error(
            "t27_private_binding_missing",
            "T27 需要 private-fixture alias 触发真实挂载变化",
        ));
    };
    let gated_alias = format!(
        "private-fixture-t27-{}",
        tag.rsplit('-').next().expect("验收 tag 包含随机后缀")
    );
    let gated_bindings = original
        .bindings
        .iter()
        .map(|binding| {
            if binding.alias == crate::fixtures::ALIAS_PRIVATE {
                ProjectReferenceBinding {
                    alias: gated_alias.clone(),
                    ..binding.clone()
                }
            } else {
                binding.clone()
            }
        })
        .collect::<Vec<_>>();
    let saved = host
        .runtime
        .replace_project_references(
            handles.project,
            ReplaceProjectReferencesRequest {
                expected_revision: original.revision,
                bindings: gated_bindings,
            },
        )
        .await
        .map_err(|cause| {
            error(
                "t27_replace_failed",
                format!("CAS 保存 private-fixture 新 alias 失败：{cause}"),
            )
        })?;
    evidence.fact("t27.gated_configuration", json!(&saved));
    valid = checked(
        evidence,
        "t27.revision_advanced",
        saved.revision > original.revision,
        format!("引用修订从 {} 前进到 {}", original.revision, saved.revision),
        format!(
            "引用修订没有前进：{} -> {}",
            original.revision, saved.revision
        ),
    );
    host.runtime
        .apply_project_references(
            handles.project,
            ApplyProjectReferencesRequest {
                expected_revision: saved.revision,
                policy: ReferenceApplyPolicy::WhenIdle,
            },
        )
        .await
        .map_err(|cause| {
            error(
                "t27_apply_request_failed",
                format!("请求应用修订 {} 失败：{cause}", saved.revision),
            )
        })?;
    let status = wait_for_applied(
        host,
        handles.project,
        saved.revision,
        host.config.operation_timeout,
    )
    .await?;
    evidence.fact("t27.applied_status", json!(&status));
    let maintainer_applied = status
        .agents
        .iter()
        .find(|entry| entry.agent_id == handles.maintainer)
        .is_some_and(|entry| {
            entry.applied_revision == saved.revision
                && entry.container_id.as_deref() != Some(before_container.as_str())
                && entry.effective_image_id.as_deref() != Some(base_image_id.as_str())
        });
    valid &= checked(
        evidence,
        "t27.maintainer_applied_new_generation",
        maintainer_applied && status.counts.failed == 0,
        format!(
            "Maintainer 已应用修订 {}，status={} failed=0",
            saved.revision, status.state
        ),
        format!(
            "Maintainer 未应用安全换代：state={} counts={:?} agents={:?}",
            status.state, status.counts, status.agents
        ),
    );
    if !valid {
        return Err(error(
            "t27_reference_apply_invalid",
            "引用替换没有形成已应用且无失败的新执行环境",
        ));
    }

    // 5) 对比新旧 Docker 事实：新容器、旧 alias 消失、卷身份不变、effective image 换成快照。
    let after_container = host
        .container_id(handles.maintainer)
        .await?
        .ok_or_else(|| error("t27_new_container_missing", "应用后 Maintainer 没有容器"))?;
    let after = environment::inspect_binding(host, handles.maintainer, &after_container).await?;
    record_environment(evidence, "t27.after_environment", &after);

    let old_target = crate::fixtures::container_path(crate::fixtures::ALIAS_PRIVATE);
    let new_target = crate::fixtures::container_path(&gated_alias);
    let targets = crate::host::reference_targets_of(&after.facts);
    let workspace_after = environment::volume_identity(&after.facts, "/workspace");
    let anon_after = environment::volume_identity(&after.facts, ANON_VOLUME_TARGET);
    let identities_preserved = workspace_before
        .as_ref()
        .zip(workspace_after.as_ref())
        .is_some_and(|(before, after)| before == after)
        && anon_before
            .as_ref()
            .zip(anon_after.as_ref())
            .is_some_and(|(before, after)| before == after);
    let after_verified = matches!(
        after.inspected,
        mai_docker::ExistingEnvironment::Matching(_)
    );
    valid = checked(
        evidence,
        "t27.container_replaced",
        after_container != before_container && after.binding.container_id == after_container,
        format!("引用变化触发真实换代：{before_container} -> {after_container}"),
        format!(
            "新容器身份不成立：before={before_container} after={after_container} binding={}",
            after.binding.container_id
        ),
    );
    valid &= checked(
        evidence,
        "t27.reference_alias_swapped",
        targets.contains(&new_target) && !targets.contains(&old_target),
        format!("新容器挂载 {new_target}，旧 alias {old_target} 已消失"),
        format!("新容器参考挂载不符合新修订：{targets:?}"),
    );
    valid &= checked(
        evidence,
        "t27.volume_identities_preserved",
        identities_preserved
            && workspace_after
                .clone()
                .is_some_and(|identity| identity.matches_name(&expected_workspace))
            && anon_after
                .clone()
                .is_some_and(|identity| identity.is_generated_anonymous()),
        format!(
            "workspace 与镜像声明匿名卷的实际 name/source 均保持不变：workspace={workspace_after:?} anon={anon_after:?}"
        ),
        format!(
            "换前后卷身份不一致：workspace {workspace_before:?}->{workspace_after:?}，anon {anon_before:?}->{anon_after:?}"
        ),
    );
    valid &= checked(
        evidence,
        "t27.writable_layer_snapshot_used",
        after.facts.image_id != base_image_id
            && after.facts.image_id != before.facts.image_id
            && after.binding.effective_image_id == after.facts.image_id,
        format!(
            "新容器 effective image 从基础 {base_image_id} 换为可写层快照 {}",
            after.facts.image_id
        ),
        format!(
            "新容器没有使用保留可写层快照：base={base_image_id} before={} after={} binding={}",
            before.facts.image_id, after.facts.image_id, after.binding.effective_image_id
        ),
    );
    valid &= checked(
        evidence,
        "t27.after_environment_matches_plan",
        after_verified
            && after.binding.container_id == after_container
            && after.facts.generation == Some(after.binding.generation),
        format!(
            "DockerClient.inspect_environment 核验新容器 {} 符合换后计划",
            after.facts.id
        ),
        format!(
            "新容器环境或绑定身份核验失败：container={after_container} binding={} diff={:?}",
            after.binding.container_id, after.inspected
        ),
    );
    if !valid {
        return Err(error(
            "t27_new_environment_invalid",
            "换后容器身份、挂载、卷身份或 effective image 不符合保留要求",
        ));
    }

    // 6) 继续只走已注册 exec 工具读取真实数据与 dpkg 数据库。
    valid &= verification::retained_data(host, handles, &tag, evidence).await?;
    if !valid {
        return Err(error(
            "t27_retention_verification_failed",
            "真实 exec 读取未能证明三类数据或本地 deb 安装保留",
        ));
    }

    Ok(())
}
