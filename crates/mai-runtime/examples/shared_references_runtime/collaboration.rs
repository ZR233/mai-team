//! 协作夹具支撑：冻结父 Agent 的 Profile、查找真实 spawn 出来的 children、解析回执与继承断言。
//!
//! 这里不产生任何“假 child”：`spawn_agent` 的能力与父子关系仍由产品 host 判定，本模块只
//! 负责前置的 Profile 冻结（夹具原本 `profile_id = None`）、事实采集与断言。

use std::sync::Arc;

use mai_protocol::{AgentId, AgentResourceState, AgentSummary};
use mai_store::MaiStore;
use serde_json::{Value, json};

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::RuntimeHost;

/// 被授权 Profile：`system-agents/executor`（spawn_agents/close_agents = true）。
pub const PARENT_PROFILE: &str = "executor";

/// child/grandchild 必须真实继承 project A 与冻结 Profile。
pub(super) async fn assert_inheritance(
    evidence: &mut Evidence,
    host: &RuntimeHost,
    summary: &AgentSummary,
    parent: AgentId,
    label: &str,
) -> Result<()> {
    evidence.fact(
        format!("{label}_summary"),
        json!({
            "agent_id": summary.id,
            "name": &summary.name,
            "parent_id": summary.parent_id,
            "project_id": summary.project_id,
            "profile_id": &summary.profile_id,
            "resource_state": summary.resource.state,
        }),
    );
    evidence.check(
        &format!("lifecycle.{label}_parent"),
        summary.parent_id == Some(parent),
        format!("{label} 的 parent 是 {parent}"),
        format!(
            "{label} 的 parent 是 {:?}，期望 {parent}",
            summary.parent_id
        ),
    );
    evidence.check(
        &format!("lifecycle.{label}_project_inherited"),
        summary.project_id == Some(ProjectKind::A.id()),
        format!("{label} 真实继承了 project A（{}）", ProjectKind::A.slug()),
        format!("{label} 的 project_id 是 {:?}", summary.project_id),
    );
    evidence.check(
        &format!("lifecycle.{label}_profile_frozen"),
        summary.profile_id.as_deref() == Some(PARENT_PROFILE),
        format!("{label} 冻结了 Profile {PARENT_PROFILE}"),
        format!("{label} 的 Profile 是 {:?}", summary.profile_id),
    );
    let targets = host.reference_targets(summary.id).await?;
    evidence.check(
        &format!("lifecycle.{label}_reference_mounts"),
        targets.contains(&fixtures::container_path(fixtures::ALIAS_PRIVATE)),
        format!("{label} 的真实容器挂载包含 A 私有夹具：{targets:?}"),
        format!("{label} 的真实容器挂载缺少 A 私有夹具：{targets:?}"),
    );
    if summary.resource.state != AgentResourceState::Ready {
        evidence.limitation(format!(
            "{label} 的资源状态是 {:?}；真实 turn 需要 Ready，读取证据因此可能缺失",
            summary.resource.state
        ));
    }
    Ok(())
}

/// 给父 Agent 冻结被授权 Profile；已经冻结时不做任何写入。
pub(super) async fn ensure_parent_profile(
    store: &Arc<MaiStore>,
    agent_id: AgentId,
) -> Result<bool> {
    let snapshot = store
        .load_runtime_snapshot(0)
        .await
        .map_err(|cause| error("store_snapshot_failed", format!("读取产品库失败：{cause}")))?;
    let persisted = snapshot
        .agents
        .iter()
        .find(|agent| agent.summary.id == agent_id)
        .ok_or_else(|| {
            error(
                "agent_missing",
                format!("产品库里没有 Agent {agent_id}（前一领域驱动的夹具缺失）"),
            )
        })?;
    if persisted.summary.profile_id.as_deref() == Some(PARENT_PROFILE) {
        return Ok(false);
    }
    let mut summary = persisted.summary.clone();
    summary.profile_id = Some(PARENT_PROFILE.to_string());
    summary.updated_at = chrono::Utc::now();
    store
        .save_agent(&summary, persisted.system_prompt.as_deref())
        .await
        .map_err(|cause| {
            error(
                "profile_freeze_failed",
                format!("写入父 Agent 的冻结 Profile 失败：{cause}"),
            )
        })?;
    Ok(true)
}

/// 产品库里由夹具父 Agent 真实 spawn 出来的 child（排除夹具自带的 `a-child`）。
pub(super) async fn spawned_children(store: &Arc<MaiStore>) -> Result<Vec<AgentId>> {
    let snapshot = store
        .load_runtime_snapshot(0)
        .await
        .map_err(|cause| error("store_snapshot_failed", format!("读取产品库失败：{cause}")))?;
    let maintainer = fixtures::agent_id(fixtures::AGENT_MAINTAINER);
    let fixture_child = fixtures::agent_id(fixtures::AGENT_CHILD);
    let mut children = snapshot
        .agents
        .iter()
        .filter(|agent| {
            agent.summary.parent_id == Some(maintainer) && agent.summary.id != fixture_child
        })
        .map(|agent| agent.summary.id)
        .collect::<Vec<_>>();
    children.sort();
    Ok(children)
}

/// 从 `spawn_agent` 回执里解析 child 身份（回执是产品 host 生成的 JSON）。
pub(super) fn extract_agent_id(receipt: &str) -> Option<AgentId> {
    let marker = "\"agentId\":\"";
    let start = receipt.find(marker)? + marker.len();
    let rest = &receipt[start..];
    let end = rest.find('"')?;
    AgentId::parse_str(&rest[..end]).ok()
}

/// 回执/输出首行摘要；回执是 JSON 时优先取 error.message。
pub fn first_line(text: &str) -> String {
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let detail = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| text.lines().next().unwrap_or_default().trim().to_string());
    if detail.len() > 300 {
        format!("{}…", detail.chars().take(300).collect::<String>())
    } else {
        detail
    }
}
