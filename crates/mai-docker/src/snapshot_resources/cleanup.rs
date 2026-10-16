//! 回收流程本体：先证明归属，再按有限轮删除；daemon 的“在用/依赖/引用冲突”按保留记录。

use tokio::process::Command;

use crate::client::{DockerClient, stderr_or_stdout};
use crate::error::{DockerError, Result};

use super::is_missing_image_error;
use super::ownership::{ReclaimableSnapshot, SnapshotDisposition, classify_snapshot_image};
use super::report::{
    RemovedSnapshotImage, RetainedSnapshotImage, SnapshotCleanupReport, SnapshotRetentionReason,
};
use super::retention::SnapshotRetention;

impl DockerClient {
    /// 回收能够证明归属的受管快照镜像，并返回本轮最终状态。
    ///
    /// 调用方（运行时）在换代成功或旧失败清理完成之后、删除元数据之后，以及启动恢复时
    /// 的孤儿容器清理之后调用本方法。它不依赖数据库存在：Agent 记录被删除后仍能凭标签
    /// 找回合法归属，因此不会丢失回收责任；反过来，本层也不认识数据库，所有在途保护都
    /// 必须由调用方通过 [`SnapshotRetention`] 显式给出。
    ///
    /// 行为边界：
    /// - 只枚举同时带受管标签与 operation 标签的镜像；
    /// - 标签、唯一 RepoTag 与来源必须互相印证，任何一条不能证明就保留；
    /// - 保护集合（镜像 ID、operation、未发布 binding 的 clone Agent）命中即保留；
    /// - 仍被任何 daemon 容器（含未受管、已停止容器）引用的镜像保留；
    /// - 只用 `docker image rm <镜像 ID>`，从不带 `-f`、从不 prune；
    /// - daemon 因“在用/子镜像依赖/引用冲突”拒绝时记为保留，其它失败直接返回错误。
    ///
    /// 删除子镜像之后父镜像才可能回收，因此按“候选数量 + 1”的有限轮重试，并且只在上一
    /// 轮确有删除时继续，绝不无限循环。
    pub async fn reclaim_snapshot_images(
        &self,
        retention: &SnapshotRetention,
    ) -> Result<SnapshotCleanupReport> {
        let Some(owner) = self.snapshot_owner else {
            return Ok(SnapshotCleanupReport::default());
        };
        let owner = owner.to_string();
        let candidates = self.managed_snapshot_image_candidates().await?;
        let images_in_use = self.container_image_ids_in_use().await?;

        let mut removed = Vec::new();
        let mut retained = Vec::new();
        let mut reclaimable = Vec::new();
        for image_id in &candidates {
            match self.inspect_snapshot_image_facts(image_id).await? {
                // 核对前就消失的镜像无法证明归属，按保留记录：本轮没有对它执行删除。
                None => retained.push(RetainedSnapshotImage {
                    image_id: image_id.clone(),
                    repo_tags: Vec::new(),
                    reason: SnapshotRetentionReason::ImageVanished,
                }),
                Some(facts) if facts.labels.get("mai.team.snapshot.owner") != Some(&owner) => {
                    retained.push(RetainedSnapshotImage {
                        image_id: facts.id,
                        repo_tags: facts.repo_tags,
                        reason: SnapshotRetentionReason::ForeignOwner,
                    });
                }
                Some(facts) => match classify_snapshot_image(&facts, retention, &images_in_use) {
                    SnapshotDisposition::Reclaimable(snapshot) => {
                        reclaimable.push(PendingSnapshotImage {
                            image_id: facts.id,
                            snapshot,
                            blocked: None,
                        });
                    }
                    SnapshotDisposition::Retained(reason) => {
                        retained.push(RetainedSnapshotImage {
                            image_id: facts.id,
                            repo_tags: facts.repo_tags,
                            reason,
                        });
                    }
                },
            }
        }

        let max_rounds = candidates.len() + 1;
        let mut pending = reclaimable;
        for _ in 0..max_rounds {
            if pending.is_empty() {
                break;
            }
            let mut blocked = Vec::new();
            let mut removed_in_round = false;
            for candidate in pending {
                match self.remove_snapshot_image(&candidate.image_id).await? {
                    SnapshotRemoval::Removed => {
                        removed_in_round = true;
                        removed.push(candidate.into_removed());
                    }
                    SnapshotRemoval::Blocked(message) => blocked.push(candidate.blocked(message)),
                }
            }
            pending = blocked;
            if !removed_in_round {
                break;
            }
        }
        for candidate in pending {
            retained.push(candidate.into_retained());
        }

        removed.sort_by(|left, right| left.image_id.cmp(&right.image_id));
        retained.sort_by(|left, right| left.image_id.cmp(&right.image_id));
        tracing::info!(
            removed = removed.len(),
            retained = retained.len(),
            "受管快照镜像回收完成"
        );
        Ok(SnapshotCleanupReport { removed, retained })
    }

    /// 按镜像 ID 删除，从不带 `-f`：daemon 对在用镜像与子镜像的拒绝按保留处理。
    ///
    /// 镜像在删除前已被并发移除时目标状态已经达成，同样记为已回收。
    async fn remove_snapshot_image(&self, image_id: &str) -> Result<SnapshotRemoval> {
        let output = Command::new(&self.binary)
            .args(["image", "rm"])
            .arg(image_id)
            .output()
            .await?;
        if output.status.success() {
            return Ok(SnapshotRemoval::Removed);
        }
        let message = stderr_or_stdout(&output);
        if is_missing_image_error(&message) {
            return Ok(SnapshotRemoval::Removed);
        }
        if is_removal_conflict(&message) {
            return Ok(SnapshotRemoval::Blocked(message));
        }
        Err(DockerError::CommandFailed(message))
    }
}

/// 归属已证明、正在等待删除的候选镜像。
struct PendingSnapshotImage {
    image_id: String,
    snapshot: ReclaimableSnapshot,
    /// daemon 上一次拒绝删除的原因；只有被拒绝过的候选才会走到保留列表。
    blocked: Option<String>,
}

impl PendingSnapshotImage {
    fn blocked(mut self, message: String) -> Self {
        self.blocked = Some(message);
        self
    }

    fn into_removed(self) -> RemovedSnapshotImage {
        RemovedSnapshotImage {
            image_id: self.image_id,
            repo_tag: self.snapshot.repo_tag,
            agent_id: self.snapshot.agent_id,
            operation_id: self.snapshot.operation_id,
            origin: self.snapshot.origin,
        }
    }

    fn into_retained(self) -> RetainedSnapshotImage {
        RetainedSnapshotImage {
            image_id: self.image_id,
            repo_tags: vec![self.snapshot.repo_tag],
            reason: SnapshotRetentionReason::RemovalBlocked {
                message: self
                    .blocked
                    .unwrap_or_else(|| "daemon 未给出拒绝原因".to_string()),
            },
        }
    }
}

/// 单次删除尝试的结果。
enum SnapshotRemoval {
    /// 镜像已不在 daemon 中（删除成功，或删除前已被并发移除）。
    Removed,
    /// daemon 拒绝删除（在用、子镜像依赖、引用冲突）；原始消息作为保留原因。
    Blocked(String),
}

/// daemon 拒绝删除的非故障原因：镜像在用、存在子镜像依赖、或被多个引用指向。
///
/// 这些情况在回收里等价于“保留”，不是需要上抛的 Docker 故障；未命中的消息一律按真实
/// 故障处理，避免把删除失败伪造成保留。
fn is_removal_conflict(message: &str) -> bool {
    const MARKERS: [&str; 7] = [
        "conflict: unable to delete",
        "conflict: unable to remove repository reference",
        "dependent child images",
        "image is being used by",
        "is using its referenced image",
        "referenced in multiple repositories",
        "must be forced",
    ];
    let message = message.to_ascii_lowercase();
    MARKERS.iter().any(|marker| message.contains(marker))
}
