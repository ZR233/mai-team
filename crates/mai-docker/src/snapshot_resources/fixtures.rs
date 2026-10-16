//! 测试夹具：按 Docker 实际写入的字面量构造快照镜像的标签与 RepoTag。
//!
//! 标签键与取值在这里刻意写成字面量，而不是复用生产常量：夹具一旦复用常量，常量写错
//! 时会与生产代码一起错，测试就失去了核对线上格式的能力。

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::image::SnapshotImageFacts;
use crate::naming::environment_snapshot_image_name;

use super::report::SnapshotOrigin;

/// 受管快照镜像的确定性 RepoTag：`environment_snapshot_image_name(agent, operation):latest`。
pub(super) fn snapshot_tag(agent_id: &str, operation_id: Uuid) -> String {
    format!(
        "{}:latest",
        environment_snapshot_image_name(agent_id, operation_id)
    )
}

/// 夹具里的 Agent 标识：项目里 `AgentId` 就是 UUID，标签与镜像名都必须用它。
pub(super) fn agent_id(seed: u128) -> String {
    Uuid::from_u128(seed).to_string()
}

/// 受管快照镜像的标签集合；`origin` 为 `None` 时不写来源标签（本轮隔离镜像）。
pub(super) fn snapshot_labels(
    agent_id: &str,
    operation_id: Uuid,
    container_id: &str,
    origin: Option<SnapshotOrigin>,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (
            "mai.team.snapshot.owner".to_string(),
            Uuid::nil().to_string(),
        ),
        ("mai.team.managed".to_string(), "true".to_string()),
        ("mai.team.agent".to_string(), agent_id.to_string()),
        (
            "mai.team.snapshot.operation".to_string(),
            operation_id.to_string(),
        ),
        (
            "mai.team.snapshot.container".to_string(),
            container_id.to_string(),
        ),
    ]);
    if let Some(origin) = origin {
        let value = match origin {
            SnapshotOrigin::Clone => "clone",
            SnapshotOrigin::Generation => "generation",
        };
        labels.insert("mai.team.snapshot.origin".to_string(), value.to_string());
    }
    labels
}

/// 归属完整的候选镜像事实。
pub(super) fn snapshot_facts(
    image_id: &str,
    agent_id: &str,
    operation_id: Uuid,
    container_id: &str,
    origin: Option<SnapshotOrigin>,
) -> SnapshotImageFacts {
    SnapshotImageFacts {
        id: image_id.to_string(),
        repo_tags: vec![snapshot_tag(agent_id, operation_id)],
        labels: snapshot_labels(agent_id, operation_id, container_id, origin),
    }
}
