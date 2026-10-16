//! 回收所需的 Docker 只读枚举：候选镜像、镜像事实、daemon 侧在用镜像。
//!
//! 这一层只读，不删除任何东西；删除的边界判定在 `cleanup`。

use std::collections::BTreeSet;

use tokio::process::Command;

use crate::client::{DockerClient, stderr_or_stdout};
use crate::container::is_missing_container_error;
use crate::error::{DockerError, Result};
use crate::image::{SnapshotImageFacts, snapshot_image_facts_from_inspect};
use crate::naming::{MANAGED_LABEL, SNAPSHOT_OPERATION_LABEL_KEY};

use super::is_missing_image_error;
use super::retention::normalize_image_id;

impl DockerClient {
    /// 枚举受管快照镜像候选：`managed=true` 与 operation 两个标签必须同时存在。
    ///
    /// 基础镜像与 legacy 临时 clone 镜像没有这两个标签，因此不在回收范围内；daemon
    /// 重复输出的镜像 ID 按首次出现去重，保证每个镜像只被处置一次。
    pub(crate) async fn managed_snapshot_image_candidates(&self) -> Result<Vec<String>> {
        let managed_filter = format!("label={MANAGED_LABEL}");
        let operation_filter = format!("label={SNAPSHOT_OPERATION_LABEL_KEY}");
        let output = Command::new(&self.binary)
            .args(["image", "ls", "--no-trunc", "--quiet", "--filter"])
            .arg(managed_filter.as_str())
            .arg("--filter")
            .arg(operation_filter.as_str())
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        let mut seen = BTreeSet::new();
        Ok(parse_docker_lines(&String::from_utf8(output.stdout)?)
            .into_iter()
            .filter(|image_id| seen.insert(image_id.clone()))
            .collect())
    }

    /// 读取单个候选镜像的标签与 RepoTag 事实；镜像在核对前消失时返回 `None`。
    pub(crate) async fn inspect_snapshot_image_facts(
        &self,
        image_id: &str,
    ) -> Result<Option<SnapshotImageFacts>> {
        let output = Command::new(&self.binary)
            .args(["image", "inspect"])
            .arg(image_id)
            .output()
            .await?;
        if !output.status.success() {
            let message = stderr_or_stdout(&output);
            if is_missing_image_error(&message) {
                return Ok(None);
            }
            return Err(DockerError::CommandFailed(message));
        }
        snapshot_image_facts_from_inspect(&String::from_utf8(output.stdout)?)
    }

    /// 所有 daemon 容器（含未受管与已停止容器）引用的镜像 ID，已归一化。
    ///
    /// 这里刻意不加受管标签过滤：未受管或已停止的容器同样可能引用候选镜像，漏掉它们
    /// 会让回收在 daemon 侧才失败；读不到在用情况时整次回收都必须失败，而不是继续删。
    pub(crate) async fn container_image_ids_in_use(&self) -> Result<BTreeSet<String>> {
        let output = Command::new(&self.binary)
            .args(["ps", "-aq", "--no-trunc"])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        let containers = parse_docker_lines(&String::from_utf8(output.stdout)?);
        if containers.is_empty() {
            return Ok(BTreeSet::new());
        }

        // 批量读取；有容器在核对前消失时回退到逐个读取，仍然读不到的容器按已消失处理。
        let inspect = Command::new(&self.binary)
            .args(["inspect", "--format", "{{.Image}}"])
            .args(&containers)
            .output()
            .await?;
        if inspect.status.success() {
            return Ok(in_use_image_ids(&String::from_utf8(inspect.stdout)?));
        }
        let message = stderr_or_stdout(&inspect);
        if !is_missing_container_error(&message) {
            return Err(DockerError::CommandFailed(message));
        }

        let mut in_use = BTreeSet::new();
        for container in &containers {
            let inspect = Command::new(&self.binary)
                .args(["inspect", "--format", "{{.Image}}"])
                .arg(container)
                .output()
                .await?;
            if !inspect.status.success() {
                let message = stderr_or_stdout(&inspect);
                if is_missing_container_error(&message) {
                    continue;
                }
                return Err(DockerError::CommandFailed(message));
            }
            in_use.extend(in_use_image_ids(&String::from_utf8(inspect.stdout)?));
        }
        Ok(in_use)
    }
}

/// 把 docker 的多行输出切干净：每行去空白，丢弃空行。
fn parse_docker_lines(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// 把 `docker inspect --format {{.Image}}` 的输出归一化成镜像 ID 集合。
fn in_use_image_ids(output: &str) -> BTreeSet<String> {
    parse_docker_lines(output)
        .iter()
        .map(|image_id| normalize_image_id(image_id))
        .collect()
}
