//! 回收结果的证据结构：删除了什么、保留了什么、以及保留的确切原因。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 快照镜像的来源。
///
/// `Clone` 表示它是克隆出生时从父容器提交的可写层，因此受“子 Agent 还没发布 binding”
/// 这一保护约束；`Generation` 表示它是换代时保留的旧环境可写层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotOrigin {
    /// `commit_clone_parent` 提交的父容器可写层。
    Clone,
    /// `commit_stopped_environment` 提交的换代旧环境可写层。
    Generation,
}

impl SnapshotOrigin {
    /// 写入标签时使用的取值。
    pub(crate) fn label_value(self) -> &'static str {
        match self {
            Self::Clone => "clone",
            Self::Generation => "generation",
        }
    }

    /// 从标签取值解析；未知取值返回 `None`，由调用方按无法证明归属处理。
    pub(crate) fn from_label(value: &str) -> Option<Self> {
        match value {
            "clone" => Some(Self::Clone),
            "generation" => Some(Self::Generation),
            _ => None,
        }
    }
}

/// 归属核对失败的确切缺口。
///
/// 每一个取值都对应“少一条证明”：标签缺失、Agent/operation 不是 UUID、RepoTag 不唯一
/// 或与标签推出的名字不一致。出现任何一种都只能保留镜像，不能放宽归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotOwnershipGap {
    /// 缺少 `mai.team.managed=true` 标签，或取值不是严格的 `true`。
    ManagedLabel,
    /// 缺少 Agent 标签。
    AgentLabel,
    /// Agent 标签不是合法 UUID（项目里 `AgentId` 就是 UUID）。
    AgentLabelNotUuid,
    /// 缺少 operation 标签，或它不是合法 UUID。
    OperationLabel,
    /// 缺少来源容器标签。
    ContainerLabel,
    /// 来源标签取值未知。
    OriginLabel,
    /// 镜像没有任何 RepoTag。
    MissingRepoTag,
    /// 镜像有多个 RepoTag（例如备份标签），无法证明唯一归属。
    MultipleRepoTags,
    /// 唯一 RepoTag 与标签推导出的确定性名字不一致。
    RepoTagMismatch,
}

/// 一个被保留的候选镜像以及保留原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedSnapshotImage {
    /// daemon 返回的镜像 ID。
    pub image_id: String,
    /// 核对时镜像上的 RepoTag（镜像在核对前消失时为空）。
    pub repo_tags: Vec<String>,
    pub reason: SnapshotRetentionReason,
}

/// 保留原因；序列化后可以直接作为回收证据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotRetentionReason {
    /// 缺少数据目录归属标签，或属于另一个实例；不得删除。
    ForeignOwner,
    /// 保护集合按镜像 ID 命中（例如刚提交但收据还没落盘）。
    ProtectedImageId,
    /// 保护集合按 operation 命中（未终态或 Failed 的收据）。
    ProtectedOperationId { operation_id: Uuid },
    /// clone 出生且该 Agent 还没有发布 binding。
    ProtectedCloneAgent { agent_id: String },
    /// 至少一个 daemon 容器（含未受管、已停止容器）仍然引用该镜像。
    ImageInUse,
    /// 标签或唯一 RepoTag 不足以证明归属。
    OwnershipUnproven { gap: SnapshotOwnershipGap },
    /// daemon 拒绝删除：镜像正在被使用、存在子镜像依赖或引用冲突。
    RemovalBlocked { message: String },
    /// 核对或删除之间镜像已被并发移除，本轮没有对它执行任何删除动作。
    ImageVanished,
}

/// 本轮回收掉的镜像；归属已经过标签、唯一 RepoTag 与来源三重核对。
///
/// 镜像在删除前已被并发移除时同样记为已回收（目标状态一致，且它此前已通过归属核对）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedSnapshotImage {
    /// daemon 返回的镜像 ID。
    pub image_id: String,
    /// 删除时镜像上的唯一 RepoTag。
    pub repo_tag: String,
    pub agent_id: String,
    pub operation_id: Uuid,
    pub origin: SnapshotOrigin,
}

/// 一次回收流程的最终状态。
///
/// 每个候选镜像只会出现在 `removed` 或 `retained` 之一；两个列表都按镜像 ID 排序，
/// 因此同一状态的重复执行会给出可比较的证据。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCleanupReport {
    pub removed: Vec<RemovedSnapshotImage>,
    pub retained: Vec<RetainedSnapshotImage>,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::{SnapshotOrigin, SnapshotOwnershipGap, SnapshotRetentionReason};

    #[test]
    fn origin_labels_round_trip() {
        for origin in [SnapshotOrigin::Clone, SnapshotOrigin::Generation] {
            assert_eq!(
                SnapshotOrigin::from_label(origin.label_value()),
                Some(origin)
            );
        }
        assert_eq!(SnapshotOrigin::from_label("cloned"), None);
    }

    #[test]
    fn reasons_serialize_with_a_machine_readable_kind() {
        assert_eq!(
            serde_json::to_value(SnapshotRetentionReason::ProtectedImageId)
                .expect("序列化保留原因"),
            serde_json::json!({ "kind": "protected_image_id" })
        );
        assert_eq!(
            serde_json::to_value(SnapshotRetentionReason::OwnershipUnproven {
                gap: SnapshotOwnershipGap::RepoTagMismatch,
            })
            .expect("序列化保留原因"),
            serde_json::json!({ "kind": "ownership_unproven", "gap": "repo_tag_mismatch" })
        );
    }
}
