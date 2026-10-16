//! 参考配置收敛与模型事实核对：`gate` 场景的两条路径共用同一套判定。
//!
//! 收敛判定只看产品投影（`project_reference_status`）：目标修订一致 + 项目状态
//! `applied` + 关键 Agent 的 `applied_revision` 达标 + 没有失败条目；超时与失败都显式
//! 报错，不把“还在 waiting”当成通过。

use std::sync::Arc;
use std::time::Duration;

use mai_protocol::{
    ApplyProjectReferencesRequest, ProjectReferenceApplicationState, ProjectReferenceBinding,
    ProjectReferenceConfiguration, ProjectReferenceStatus, ReferenceApplicationState,
    ReferenceApplyPolicy, ReplaceProjectReferencesRequest,
};
use serde::Serialize;
use serde_json::json;
use tokio::time::sleep;

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::RuntimeHost;
use crate::probe;
use crate::runtime_config::DriverConfig;
use mai_store::MaiStore;

/// 收敛轮询间隔。
const CONVERGENCE_POLL: Duration = Duration::from_secs(1);
/// 把某个 alias 改名（alias 决定容器目标路径，因此是真实挂载变化）。
pub(super) fn renamed(
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

/// 还原夹具原始绑定；内容完全一致时修订号不前进（走同一套领域 CAS + 快照）。
pub(super) async fn restore_configuration(
    host: &RuntimeHost,
    project: ProjectKind,
    original: &ProjectReferenceConfiguration,
    policy: ReferenceApplyPolicy,
) -> Result<ProjectReferenceConfiguration> {
    let current = fixtures::project_configuration(&host.store, project).await?;
    if current.bindings == original.bindings {
        return Ok(current);
    }
    let saved = host
        .runtime
        .replace_project_references(
            project.id(),
            ReplaceProjectReferencesRequest {
                expected_revision: current.revision,
                bindings: original.bindings.clone(),
            },
        )
        .await
        .map_err(|cause| {
            error(
                "restore_references_failed",
                format!("还原夹具配置失败：{cause}"),
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
                "restore_apply_rejected",
                format!("还原配置的应用请求被拒绝：{cause}"),
            )
        })?;
    Ok(saved)
}

/// 等待项目收敛到目标修订且全部有执行环境的 Agent 都已应用。
pub(super) async fn wait_for_applied(
    host: &RuntimeHost,
    project: ProjectKind,
    revision: u64,
    limit: Duration,
) -> Result<ProjectReferenceStatus> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let status = host.reference_status(project).await?;
        let settled = status.configuration.revision == revision
            && status.state == ProjectReferenceApplicationState::Applied;
        if settled {
            return Ok(status);
        }
        if status.state == ProjectReferenceApplicationState::Failed {
            return Err(error(
                "reference_apply_failed",
                format!(
                    "目标修订 {revision} 应用失败：{}",
                    serde_json::to_string(&status).unwrap_or_default()
                ),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error(
                "reference_apply_timeout",
                format!(
                    "等待目标修订 {revision} 收敛超时 {limit:?}：{}",
                    serde_json::to_string(&status).unwrap_or_default()
                ),
            ));
        }
        sleep(CONVERGENCE_POLL).await;
    }
}

pub(super) fn add_convergence_evidence(
    evidence: &mut Evidence,
    prefix: &str,
    status: &ProjectReferenceStatus,
    revision: u64,
    agent: mai_protocol::AgentId,
) {
    evidence.fact(
        format!("{prefix}.status"),
        json!({
            "revision": status.configuration.revision,
            "state": status.state,
            "counts": status.counts,
            "agents": status.agents.iter().map(|entry| json!({
                "agent_id": entry.agent_id,
                "applied_revision": entry.applied_revision,
                "state": entry.state,
                "failure": &entry.failure,
            })).collect::<Vec<_>>(),
        }),
    );
    evidence.check(
        &format!("{prefix}.no_failures"),
        status.counts.failed == 0,
        format!(
            "目标修订 {revision} 没有失败的 Agent（counts={:?}）",
            status.counts
        ),
        format!("目标修订 {revision} 存在失败 Agent：{:?}", status.counts),
    );
    let applied = agent_state(status, agent);
    evidence.check(
        &format!("{prefix}.maintainer_applied"),
        applied
            .as_ref()
            .is_some_and(|state| state.applied_revision == revision),
        format!("{} 已应用目标修订 {revision}", fixtures::AGENT_MAINTAINER),
        format!(
            "{} 未应用目标修订 {revision}：{:?}",
            fixtures::AGENT_MAINTAINER,
            applied
        ),
    );
}

pub(super) fn agent_state(
    status: &ProjectReferenceStatus,
    agent: mai_protocol::AgentId,
) -> Option<AgentState> {
    status
        .agents
        .iter()
        .find(|entry| entry.agent_id == agent)
        .map(|entry| AgentState {
            applied_revision: entry.applied_revision,
            state: entry.state,
        })
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct AgentState {
    pub(super) applied_revision: u64,
    state: ReferenceApplicationState,
}

/// 最新宿主清单必须来自 applied 快照；历史输入、工具结果和旧清单仍保留在会话中。
pub(super) fn assert_model_facts(
    config: &DriverConfig,
    evidence: &mut Evidence,
    probe_id: &str,
    expected_paths: &[&str],
    absent_paths: &[&str],
) {
    let requests = probe::provider_requests(&config.provider_evidence_dir);
    let Some(request) = requests
        .iter()
        .find(|request| request.to_string().contains(probe_id))
    else {
        evidence.fail(
            "gate.model_facts.request",
            format!(
                "没有找到 probe {probe_id} 的真实 provider 请求（{}）",
                config.provider_evidence_dir.display()
            ),
        );
        return;
    };
    let manifests = request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .filter_map(|item| item.get("content").and_then(serde_json::Value::as_array))
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| text.starts_with("[当前宿主事实：mai.references]"))
        .collect::<Vec<_>>();
    let Some(text) = manifests.last() else {
        evidence.fail(
            "gate.model_facts.manifest",
            "真实请求缺少当前参考源宿主清单",
        );
        return;
    };
    let paths = text
        .split("/references/")
        .skip(1)
        .map(|suffix| {
            let alias = suffix
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
                .collect::<String>();
            format!("/references/{alias}")
        })
        .collect::<Vec<_>>();
    evidence.fact(format!("model_facts.{probe_id}"), serde_json::json!({"latest_manifest": text,"paths":paths,"historical_manifest_count":manifests.len().saturating_sub(1)}));
    for path in expected_paths {
        evidence.check(
            &format!("gate.model_facts.present.{path}"),
            paths.iter().any(|candidate| candidate == path),
            format!("probe {probe_id} 的 provider 请求包含 applied 路径 {path}"),
            format!("probe {probe_id} 的 provider 请求缺少 applied 路径 {path}"),
        );
    }
    for path in absent_paths {
        evidence.check(
            &format!("gate.model_facts.absent.{path}"),
            !paths.iter().any(|candidate| candidate == path),
            format!("probe {probe_id} 的 provider 请求不含过期路径 {path}"),
            format!("probe {probe_id} 的 provider 请求仍含过期路径 {path}"),
        );
    }
}

/// 夹具必须回到原始 alias：其它场景与后续运行继续复用同一批夹具。
pub async fn assert_fixture_restored(
    evidence: &mut Evidence,
    store: &Arc<MaiStore>,
    project: ProjectKind,
    gated_alias: &str,
) -> Result<()> {
    let configuration = fixtures::project_configuration(store, project).await?;
    evidence.check(
        "gate.fixture_restored",
        fixtures::binding(&configuration, fixtures::ALIAS_PRIVATE).is_some()
            && fixtures::binding(&configuration, gated_alias).is_none(),
        "夹具配置已还原为原始 alias，其它场景可继续复用",
        format!(
            "夹具配置没有还原（revision {}，alias={:?}）",
            configuration.revision,
            configuration
                .bindings
                .iter()
                .map(|binding| binding.alias.clone())
                .collect::<Vec<_>>()
        ),
    );
    Ok(())
}
