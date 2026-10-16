//! 快照镜像的归属核对与保护判定。
//!
//! 这里全是纯函数：输入是 `docker image inspect` 的事实、调用方给出的保护集合、以及
//! daemon 侧在用镜像的 ID 集合，输出是“可以回收”或“保留，并给出确切原因”。

use std::collections::BTreeSet;

use uuid::Uuid;

use crate::image::SnapshotImageFacts;
use crate::naming::{
    AGENT_LABEL_KEY, MANAGED_LABEL, SNAPSHOT_CONTAINER_LABEL_KEY, SNAPSHOT_OPERATION_LABEL_KEY,
    environment_snapshot_image_name,
};

use super::SNAPSHOT_ORIGIN_LABEL_KEY;
use super::report::{SnapshotOrigin, SnapshotOwnershipGap, SnapshotRetentionReason};
use super::retention::{SnapshotRetention, normalize_image_id};

/// 归属核对通过、允许回收的快照镜像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReclaimableSnapshot {
    pub agent_id: String,
    pub operation_id: Uuid,
    pub origin: SnapshotOrigin,
    pub repo_tag: String,
}

/// 单个候选镜像的处置结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotDisposition {
    Reclaimable(ReclaimableSnapshot),
    Retained(SnapshotRetentionReason),
}

/// 判定一个候选镜像是否可以回收。
///
/// 先证明归属，再看保护：归属不成立的镜像即使有保护集合命中也不构成删除授权，反之
/// 归属成立的镜像只要被任何一条保护覆盖就必须保留。`images_in_use` 是全部 daemon
/// 容器（含未受管与已停止容器）引用的镜像 ID，已归一化。
pub(crate) fn classify_snapshot_image(
    facts: &SnapshotImageFacts,
    retention: &SnapshotRetention,
    images_in_use: &BTreeSet<String>,
) -> SnapshotDisposition {
    let snapshot = match verify_ownership(facts) {
        Ok(snapshot) => snapshot,
        Err(gap) => {
            return SnapshotDisposition::Retained(SnapshotRetentionReason::OwnershipUnproven {
                gap,
            });
        }
    };
    if retention.protects_image(&facts.id) {
        return SnapshotDisposition::Retained(SnapshotRetentionReason::ProtectedImageId);
    }
    if retention.protects_operation(snapshot.operation_id) {
        return SnapshotDisposition::Retained(SnapshotRetentionReason::ProtectedOperationId {
            operation_id: snapshot.operation_id,
        });
    }
    if images_in_use.contains(&normalize_image_id(&facts.id)) {
        return SnapshotDisposition::Retained(SnapshotRetentionReason::ImageInUse);
    }
    if snapshot.origin == SnapshotOrigin::Clone
        && retention.protects_clone_agent(&snapshot.agent_id)
    {
        return SnapshotDisposition::Retained(SnapshotRetentionReason::ProtectedCloneAgent {
            agent_id: snapshot.agent_id,
        });
    }
    SnapshotDisposition::Reclaimable(snapshot)
}

/// 证明归属：受管标签、Agent、operation、来源容器、来源与唯一 RepoTag 必须互相印证。
///
/// 任何一条不成立都返回确切的缺口，调用方按“保留”处理；这里不做任何猜测，也不接受
/// 拼写相近的名字。
fn verify_ownership(
    facts: &SnapshotImageFacts,
) -> std::result::Result<ReclaimableSnapshot, SnapshotOwnershipGap> {
    let (managed_key, managed_value) = managed_label_expression();
    if facts.label(managed_key) != Some(managed_value) {
        return Err(SnapshotOwnershipGap::ManagedLabel);
    }
    let agent_id = label_value(facts, AGENT_LABEL_KEY).ok_or(SnapshotOwnershipGap::AgentLabel)?;
    // Agent 标识在项目里就是 UUID（`mai-protocol` 里 `AgentId = Uuid`）；不是 UUID 的
    // 标签说明这个名字不是本项目的 Agent 写下的，不能作为删除授权。
    agent_id
        .parse::<Uuid>()
        .map_err(|_| SnapshotOwnershipGap::AgentLabelNotUuid)?;
    let operation_id = label_value(facts, SNAPSHOT_OPERATION_LABEL_KEY)
        .ok_or(SnapshotOwnershipGap::OperationLabel)?
        .parse::<Uuid>()
        .map_err(|_| SnapshotOwnershipGap::OperationLabel)?;
    label_value(facts, SNAPSHOT_CONTAINER_LABEL_KEY).ok_or(SnapshotOwnershipGap::ContainerLabel)?;
    let origin = match facts.label(SNAPSHOT_ORIGIN_LABEL_KEY) {
        // 本轮隔离镜像没有来源标签，按换代处理；名字与其它标签仍必须完全一致。
        None => SnapshotOrigin::Generation,
        Some(value) => {
            SnapshotOrigin::from_label(value).ok_or(SnapshotOwnershipGap::OriginLabel)?
        }
    };

    let repo_tag = match facts.repo_tags.as_slice() {
        [repo_tag] => repo_tag,
        [] => return Err(SnapshotOwnershipGap::MissingRepoTag),
        _ => return Err(SnapshotOwnershipGap::MultipleRepoTags),
    };
    let expected_name = environment_snapshot_image_name(&agent_id, operation_id);
    if !is_exact_snapshot_reference(repo_tag, &expected_name) {
        return Err(SnapshotOwnershipGap::RepoTagMismatch);
    }

    Ok(ReclaimableSnapshot {
        agent_id,
        operation_id,
        origin,
        repo_tag: repo_tag.clone(),
    })
}

/// RepoTag 必须严格等于确定性名字，允许 Docker 省略 `:latest` 的等价写法。
fn is_exact_snapshot_reference(repo_tag: &str, expected_name: &str) -> bool {
    match repo_tag.strip_suffix(":latest") {
        Some(name) => name == expected_name,
        None => repo_tag == expected_name,
    }
}

/// 读取不能用作归属证明的空标签；空值与缺失等价。
fn label_value(facts: &SnapshotImageFacts, key: &str) -> Option<String> {
    facts
        .label(key)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// 把 `naming::MANAGED_LABEL`（`key=value` 形式）拆成键与值，避免同一字面量写两遍。
fn managed_label_expression() -> (&'static str, &'static str) {
    MANAGED_LABEL
        .split_once('=')
        .expect("MANAGED_LABEL 必须是 key=value 形式")
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use crate::image::SnapshotImageFacts;
    use crate::naming::environment_snapshot_image_name;

    use super::super::fixtures::{agent_id, snapshot_facts, snapshot_labels, snapshot_tag};
    use super::super::report::{SnapshotOrigin, SnapshotOwnershipGap, SnapshotRetentionReason};
    use super::super::retention::SnapshotRetention;
    use super::{ReclaimableSnapshot, SnapshotDisposition, classify_snapshot_image};

    /// 用给定的标签与 RepoTag 构造候选镜像事实。
    fn facts(
        image_id: &str,
        repo_tags: Vec<String>,
        labels: BTreeMap<String, String>,
    ) -> SnapshotImageFacts {
        SnapshotImageFacts {
            id: image_id.to_string(),
            repo_tags,
            labels,
        }
    }

    #[test]
    fn reclaims_images_with_a_single_deterministic_repo_tag() {
        let agent = agent_id(201);
        let operation = Uuid::from_u128(1);
        let expected_tag = snapshot_tag(&agent, operation);
        let disposition = classify_snapshot_image(
            &facts(
                "sha256:one",
                vec![expected_tag.clone()],
                snapshot_labels(&agent, operation, "container-1", None),
            ),
            &SnapshotRetention::new(),
            &BTreeSet::new(),
        );

        assert_eq!(
            disposition,
            SnapshotDisposition::Reclaimable(ReclaimableSnapshot {
                agent_id: agent,
                operation_id: operation,
                // 没有来源标签的镜像按换代处理，而不是当成无法证明归属。
                origin: SnapshotOrigin::Generation,
                repo_tag: expected_tag,
            })
        );
    }

    #[test]
    fn accepts_repo_tags_without_the_latest_suffix() {
        let agent = agent_id(202);
        let operation = Uuid::from_u128(2);
        let name_only = environment_snapshot_image_name(&agent, operation);
        let disposition = classify_snapshot_image(
            &facts(
                "sha256:one",
                vec![name_only.clone()],
                snapshot_labels(&agent, operation, "container-1", None),
            ),
            &SnapshotRetention::new(),
            &BTreeSet::new(),
        );

        assert_eq!(
            disposition,
            SnapshotDisposition::Reclaimable(ReclaimableSnapshot {
                agent_id: agent,
                operation_id: operation,
                origin: SnapshotOrigin::Generation,
                repo_tag: name_only,
            })
        );
    }

    #[test]
    fn reads_the_clone_origin_from_the_label() {
        let agent = agent_id(203);
        let operation = Uuid::from_u128(3);
        let expected_tag = snapshot_tag(&agent, operation);
        let disposition = classify_snapshot_image(
            &snapshot_facts(
                "sha256:clone",
                &agent,
                operation,
                "parent-container",
                Some(SnapshotOrigin::Clone),
            ),
            &SnapshotRetention::new(),
            &BTreeSet::new(),
        );

        assert_eq!(
            disposition,
            SnapshotDisposition::Reclaimable(ReclaimableSnapshot {
                agent_id: agent,
                operation_id: operation,
                origin: SnapshotOrigin::Clone,
                repo_tag: expected_tag,
            })
        );
    }

    #[test]
    fn reports_the_exact_ownership_gap() {
        let agent = agent_id(204);
        let operation = Uuid::from_u128(4);
        let base = || snapshot_labels(&agent, operation, "container-1", None);
        let with = |labels: BTreeMap<String, String>, key: &str, value: &str| {
            let mut labels = labels;
            labels.insert(key.to_string(), value.to_string());
            labels
        };
        let without = |labels: BTreeMap<String, String>, key: &str| {
            let mut labels = labels;
            labels.remove(key);
            labels
        };

        let cases = vec![
            (BTreeMap::new(), SnapshotOwnershipGap::ManagedLabel),
            (
                with(base(), "mai.team.managed", "TRUE"),
                SnapshotOwnershipGap::ManagedLabel,
            ),
            (
                without(base(), "mai.team.agent"),
                SnapshotOwnershipGap::AgentLabel,
            ),
            (
                with(base(), "mai.team.agent", "not-an-agent-uuid"),
                SnapshotOwnershipGap::AgentLabelNotUuid,
            ),
            (
                with(base(), "mai.team.snapshot.operation", "not-a-uuid"),
                SnapshotOwnershipGap::OperationLabel,
            ),
            (
                without(base(), "mai.team.snapshot.container"),
                SnapshotOwnershipGap::ContainerLabel,
            ),
            (
                with(base(), "mai.team.snapshot.origin", "cloned"),
                SnapshotOwnershipGap::OriginLabel,
            ),
        ];
        for (labels, gap) in cases {
            let disposition = classify_snapshot_image(
                &facts("sha256:one", vec![snapshot_tag(&agent, operation)], labels),
                &SnapshotRetention::new(),
                &BTreeSet::new(),
            );
            assert_eq!(
                disposition,
                SnapshotDisposition::Retained(SnapshotRetentionReason::OwnershipUnproven { gap }),
                "缺口 {gap:?} 必须被报出来"
            );
        }
    }

    #[test]
    fn keeps_images_with_ambiguous_repo_tags() {
        let agent = agent_id(205);
        let operation = Uuid::from_u128(5);
        let labels = snapshot_labels(&agent, operation, "container-1", None);
        let extra_tag = "mai-team-snapshot-backup:latest".to_string();

        assert_eq!(
            classify_snapshot_image(
                &facts("sha256:one", Vec::new(), labels.clone()),
                &SnapshotRetention::new(),
                &BTreeSet::new(),
            ),
            SnapshotDisposition::Retained(SnapshotRetentionReason::OwnershipUnproven {
                gap: SnapshotOwnershipGap::MissingRepoTag,
            })
        );
        assert_eq!(
            classify_snapshot_image(
                &facts(
                    "sha256:one",
                    vec![snapshot_tag(&agent, operation), extra_tag],
                    labels.clone(),
                ),
                &SnapshotRetention::new(),
                &BTreeSet::new(),
            ),
            SnapshotDisposition::Retained(SnapshotRetentionReason::OwnershipUnproven {
                gap: SnapshotOwnershipGap::MultipleRepoTags,
            })
        );
        assert_eq!(
            classify_snapshot_image(
                &facts(
                    "sha256:one",
                    vec![snapshot_tag(&agent, Uuid::from_u128(6))],
                    labels,
                ),
                &SnapshotRetention::new(),
                &BTreeSet::new(),
            ),
            SnapshotDisposition::Retained(SnapshotRetentionReason::OwnershipUnproven {
                gap: SnapshotOwnershipGap::RepoTagMismatch,
            })
        );
    }

    #[test]
    fn keeps_protected_images_and_images_in_use() {
        let agent = agent_id(206);
        let operation = Uuid::from_u128(7);
        let clone_operation = Uuid::from_u128(8);
        let generation = facts(
            "sha256:one",
            vec![snapshot_tag(&agent, operation)],
            snapshot_labels(&agent, operation, "container-1", None),
        );
        let clone = snapshot_facts(
            "sha256:two",
            &agent,
            clone_operation,
            "container-2",
            Some(SnapshotOrigin::Clone),
        );

        assert_eq!(
            classify_snapshot_image(
                &generation,
                &SnapshotRetention::new().with_image_ids(["SHA256:ONE"]),
                &BTreeSet::new(),
            ),
            SnapshotDisposition::Retained(SnapshotRetentionReason::ProtectedImageId)
        );
        assert_eq!(
            classify_snapshot_image(
                &generation,
                &SnapshotRetention::new().with_operation_ids([operation]),
                &BTreeSet::new(),
            ),
            SnapshotDisposition::Retained(SnapshotRetentionReason::ProtectedOperationId {
                operation_id: operation,
            })
        );
        assert_eq!(
            classify_snapshot_image(
                &generation,
                &SnapshotRetention::new(),
                &BTreeSet::from(["one".to_string()]),
            ),
            SnapshotDisposition::Retained(SnapshotRetentionReason::ImageInUse)
        );
        assert_eq!(
            classify_snapshot_image(
                &clone,
                &SnapshotRetention::new().with_clone_agent_ids([agent.clone()]),
                &BTreeSet::new(),
            ),
            SnapshotDisposition::Retained(SnapshotRetentionReason::ProtectedCloneAgent {
                agent_id: agent.clone(),
            })
        );
        // 未发布 binding 的保护只针对 clone 出生镜像，换代镜像仍然可以按 generation 回收。
        assert_eq!(
            classify_snapshot_image(
                &generation,
                &SnapshotRetention::new().with_clone_agent_ids([agent.clone()]),
                &BTreeSet::new(),
            ),
            SnapshotDisposition::Reclaimable(ReclaimableSnapshot {
                agent_id: agent.clone(),
                operation_id: operation,
                origin: SnapshotOrigin::Generation,
                repo_tag: snapshot_tag(&agent, operation),
            })
        );
    }
}
