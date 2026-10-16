//! 回收流程与提交标签的端到端测试。
//!
//! fake docker CLI 完全由状态文件驱动（候选镜像、容器、每个镜像的 inspect JSON、删除
//! 拒绝规则），因此测试不修改任何进程级环境，也不需要真实 daemon。

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use crate::naming::environment_snapshot_image_name;
use crate::{DockerClient, DockerError, EnvironmentSnapshotRequest};

use super::fixtures::{agent_id, snapshot_labels, snapshot_tag};
use super::{
    RemovedSnapshotImage, RetainedSnapshotImage, SnapshotCleanupReport, SnapshotOrigin,
    SnapshotOwnershipGap, SnapshotRetention, SnapshotRetentionReason,
};

/// fake docker CLI：所有状态都来自 `$S` 目录下的文件。
///
/// `image ls` 会按参数里的标签过滤真的筛一遍候选（只有带受管与 operation 标签的镜像才
/// 出现），`image rm` 从不接受 `-f`，删除是否被打回由状态文件决定。
const FAKE_DOCKER: &str = r#"#!/bin/sh
S='@STATE@'
echo "$*" >> "$S/calls"

case "$1" in
  image)
    case "$2" in
      ls)
        case "$*" in
          *"label=mai.team.managed=true"*) ;;
          *) echo "fake docker: 枚举必须带受管标签过滤" >&2; exit 9 ;;
        esac
        case "$*" in
          *"label=mai.team.snapshot.operation"*) ;;
          *) echo "fake docker: 枚举必须带 operation 标签过滤" >&2; exit 9 ;;
        esac
        if [ -f "$S/candidates" ]; then
          while read -r id; do
            [ -n "$id" ] || continue
            json="$S/image-json/$id"
            if [ -f "$json" ] && grep -q '"mai.team.managed":"true"' "$json" && grep -q '"mai.team.snapshot.operation"' "$json"; then
              echo "$id"
            fi
          done < "$S/candidates"
        fi
        exit 0
        ;;
      rm)
        shift 2
        if [ "$1" = "-f" ] || [ "$1" = "--force" ]; then
          echo "fake docker: 回收不允许强制删除" >&2
          exit 9
        fi
        id="$1"
        dependency="$S/rm-dependency/$id"
        if [ -f "$dependency" ]; then
          child="$(cat "$dependency")"
          if [ ! -f "$S/removed/$child" ]; then
            echo "Error response from daemon: conflict: unable to delete $id (cannot be forced) - image has dependent child images" >&2
            exit 1
          fi
        fi
        fault="$S/rm-fault/$id"
        if [ -f "$fault" ]; then
          cat "$fault" >&2
          exit 1
        fi
        mkdir -p "$S/removed"
        : > "$S/removed/$id"
        echo "Deleted: $id"
        exit 0
        ;;
      inspect)
        format=""
        reference=""
        shift 2
        while [ $# -gt 0 ]; do
          case "$1" in
            --format) format="$2"; shift 2 ;;
            *) reference="$1"; shift ;;
          esac
        done
        if [ -n "$format" ]; then
          if [ -f "$S/reference-absent" ]; then
            echo "Error: No such image: $reference" >&2
            exit 1
          fi
          if [ -f "$S/reference-id" ]; then
            cat "$S/reference-id"
            exit 0
          fi
          echo "Error: No such image: $reference" >&2
          exit 1
        fi
        json="$S/image-json/$reference"
        if [ -f "$json" ]; then
          cat "$json"
          exit 0
        fi
        echo "Error: No such image: $reference" >&2
        exit 1
        ;;
      *)
        echo "fake docker: 未预期的 image 子命令: $2" >&2
        exit 9
        ;;
    esac
    ;;
  ps)
    cat "$S/containers" 2>/dev/null
    exit 0
    ;;
  inspect)
    format=""
    ids=""
    shift
    while [ $# -gt 0 ]; do
      case "$1" in
        --format) format="$2"; shift 2 ;;
        *) ids="$ids $1"; shift ;;
      esac
    done
    if [ "$format" = "{{.State.Running}}" ]; then
      for id in $ids; do
        if [ -f "$S/running/$id" ]; then
          cat "$S/running/$id"
        else
          echo "false"
        fi
      done
      exit 0
    fi
    if [ "$format" = "{{.Image}}" ]; then
      missing=""
      for id in $ids; do
        if [ -f "$S/container-vanished/$id" ]; then
          missing="$id"
        fi
      done
      if [ -n "$missing" ]; then
        echo "Error: No such container: $missing" >&2
        exit 1
      fi
      for id in $ids; do
        if [ -f "$S/container-image/$id" ]; then
          cat "$S/container-image/$id"
        else
          echo "Error: No such container: $id" >&2
          exit 1
        fi
      done
      exit 0
    fi
    echo "fake docker: 未预期的 inspect format: $format" >&2
    exit 9
    ;;
  commit)
    exit 0
    ;;
  *)
    echo "fake docker: 未预期的子命令: $1" >&2
    exit 9
    ;;
esac
"#;

/// 自带状态文件的 fake docker CLI。
struct FakeDocker {
    _directory: TempDir,
    root: PathBuf,
    binary: String,
}

impl FakeDocker {
    fn new() -> Self {
        let directory = TempDir::new().expect("临时目录");
        let root = directory.path().to_path_buf();
        let binary = root.join("docker");
        let staging = root.join("docker.new");
        fs::write(
            &staging,
            FAKE_DOCKER.replace("@STATE@", &root.to_string_lossy()),
        )
        .expect("写 fake docker");
        let mut permissions = fs::metadata(&staging)
            .expect("fake docker 元数据")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&staging, permissions).expect("fake docker 可执行");
        fs::rename(&staging, &binary).expect("发布 fake docker");
        Self {
            _directory: directory,
            root,
            binary: binary.to_string_lossy().into_owned(),
        }
    }

    fn client(&self) -> DockerClient {
        DockerClient::new_with_binary("unused:latest", self.binary.clone())
            .with_snapshot_owner(Uuid::nil())
    }

    /// 记录下来的全部调用。
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    /// 记录下来的删除调用；用来证明哪些镜像被尝试删除、以及从不带 `-f`。
    fn removal_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|call| call.starts_with("image rm"))
            .collect()
    }

    /// 把一个镜像放进候选列表（`docker image ls` 的输出），并写入它的 inspect 事实。
    ///
    /// `repo_tags` 为 `None` 时模拟悬空镜像（Docker 用 `null` 表示没有 RepoTag）。
    fn publish_image(
        &self,
        image_id: &str,
        repo_tags: Option<&[String]>,
        labels: &BTreeMap<String, String>,
    ) {
        self.append_line("candidates", image_id);
        let repo_tags = match repo_tags {
            Some(repo_tags) => serde_json::to_value(repo_tags).expect("RepoTag 列表"),
            None => serde_json::Value::Null,
        };
        let facts = serde_json::json!([{
            "Id": image_id,
            "RepoTags": repo_tags,
            "Config": { "Labels": serde_json::to_value(labels).expect("标签映射") },
        }]);
        self.write_file(&format!("image-json/{image_id}"), &facts.to_string());
    }

    /// 发布一个归属完整的受管快照镜像。
    fn publish_snapshot(
        &self,
        image_id: &str,
        agent_id: &str,
        operation_id: Uuid,
        container_id: &str,
        origin: Option<SnapshotOrigin>,
    ) {
        let labels = snapshot_labels(agent_id, operation_id, container_id, origin);
        let repo_tags = [snapshot_tag(agent_id, operation_id)];
        self.publish_image(image_id, Some(&repo_tags), &labels);
    }

    /// 让一个容器引用某个镜像。
    ///
    /// 脚本不做受管过滤，所以这里写下的容器可以代表“未受管/已停止”的占用方。
    fn attach_container(&self, container_id: &str, image_id: &str) {
        self.append_line("containers", container_id);
        self.write_file(&format!("container-image/{container_id}"), image_id);
    }

    /// 容器在核对前消失：批量 inspect 报错，逐个 inspect 时这个容器按不存在处理。
    fn vanish_container(&self, container_id: &str) {
        self.write_file(&format!("container-vanished/{container_id}"), "");
    }

    /// daemon 以“存在子镜像依赖”拒绝删除 `parent`，直到 `child` 已被删除。
    fn depend_on_child(&self, parent: &str, child: &str) {
        self.write_file(&format!("rm-dependency/{parent}"), child);
    }

    /// daemon 以给定消息拒绝删除该镜像。
    fn reject_removal(&self, image_id: &str, message: &str) {
        self.write_file(&format!("rm-fault/{image_id}"), message);
    }

    /// 让 `docker image inspect --format {{.Id}} <引用>` 返回固定镜像 ID。
    fn report_image_id(&self, image_id: &str) {
        self.write_file("reference-id", image_id);
    }

    fn write_file(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("建状态目录");
        }
        fs::write(&path, contents).expect("写状态文件");
    }

    fn append_line(&self, relative: &str, line: &str) {
        let path = self.root.join(relative);
        let mut contents = fs::read_to_string(&path).unwrap_or_default();
        contents.push_str(line);
        contents.push('\n');
        fs::write(&path, contents).expect("追加状态文件");
    }
}

/// 枚举范围必须只有受管快照镜像：基础镜像与 legacy 临时 clone 镜像没有这两个标签。
fn assert_enumerates_managed_snapshots(docker: &FakeDocker) {
    let enumeration = docker
        .calls()
        .into_iter()
        .find(|call| call.starts_with("image ls"))
        .expect("镜像枚举调用");
    assert!(enumeration.contains("--no-trunc"), "{enumeration}");
    assert!(enumeration.contains("--quiet"), "{enumeration}");
    assert!(
        enumeration.contains("label=mai.team.managed=true"),
        "{enumeration}"
    );
    assert!(
        enumeration.contains("label=mai.team.snapshot.operation"),
        "{enumeration}"
    );
}

#[tokio::test]
async fn reclaims_owned_snapshot_images_and_keeps_unproven_ones() {
    let docker = FakeDocker::new();
    let agent = agent_id(101);
    let operation = Uuid::from_u128(1);

    // 归属完整：标签推出的确定性名字与唯一 RepoTag 一致，且没有来源标签（本轮隔离镜像）。
    docker.publish_snapshot("sha256:good", &agent, operation, "container-1", None);
    // 标签说这个 Agent 与 operation，RepoTag 却来自另一个 operation：名字与标签不一致。
    docker.publish_image(
        "sha256:gap-a-name-mismatch",
        Some(&[snapshot_tag(&agent, Uuid::from_u128(2))]),
        &snapshot_labels(&agent, operation, "container-2", None),
    );
    // 同一个镜像上有备份标签，无法证明唯一的确定性归属。
    docker.publish_image(
        "sha256:gap-b-multiple-tags",
        Some(&[
            snapshot_tag(&agent, operation),
            "mai-team-snapshot-backup:latest".to_string(),
        ]),
        &snapshot_labels(&agent, operation, "container-3", None),
    );
    // 缺少来源容器标签：无法证明它保留的是哪一个容器。
    let mut missing_container = snapshot_labels(&agent, operation, "container-4", None);
    missing_container.remove("mai.team.snapshot.container");
    docker.publish_image(
        "sha256:gap-c-container-label",
        Some(&[snapshot_tag(&agent, operation)]),
        &missing_container,
    );
    // 悬空镜像：没有任何 RepoTag。
    docker.publish_image(
        "sha256:gap-d-no-tags",
        None,
        &snapshot_labels(&agent, operation, "container-5", None),
    );
    // 受管标签不是严格的 `true`（大小写不同也不行）。
    let mut not_managed = snapshot_labels(&agent, operation, "container-6", None);
    not_managed.insert("mai.team.managed".to_string(), "TRUE".to_string());
    docker.publish_image(
        "sha256:gap-e-not-managed",
        Some(&[snapshot_tag(&agent, operation)]),
        &not_managed,
    );
    // legacy 临时 clone 镜像没有受管标签，枚举阶段就不在回收范围内。
    docker.publish_image(
        "sha256:legacy-temp-clone",
        Some(&["mai-team-snapshot-agent-1-1700000000000000000:latest".to_string()]),
        &BTreeMap::new(),
    );

    let report = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("回收受管快照镜像");

    assert_eq!(
        report,
        SnapshotCleanupReport {
            removed: vec![RemovedSnapshotImage {
                image_id: "sha256:good".to_string(),
                repo_tag: snapshot_tag(&agent, operation),
                agent_id: agent.clone(),
                operation_id: operation,
                // 本轮隔离镜像没有来源标签，按换代处理。
                origin: SnapshotOrigin::Generation,
            }],
            retained: vec![
                RetainedSnapshotImage {
                    image_id: "sha256:gap-a-name-mismatch".to_string(),
                    repo_tags: vec![snapshot_tag(&agent, Uuid::from_u128(2))],
                    reason: SnapshotRetentionReason::OwnershipUnproven {
                        gap: SnapshotOwnershipGap::RepoTagMismatch,
                    },
                },
                RetainedSnapshotImage {
                    image_id: "sha256:gap-b-multiple-tags".to_string(),
                    repo_tags: vec![
                        snapshot_tag(&agent, operation),
                        "mai-team-snapshot-backup:latest".to_string(),
                    ],
                    reason: SnapshotRetentionReason::OwnershipUnproven {
                        gap: SnapshotOwnershipGap::MultipleRepoTags,
                    },
                },
                RetainedSnapshotImage {
                    image_id: "sha256:gap-c-container-label".to_string(),
                    repo_tags: vec![snapshot_tag(&agent, operation)],
                    reason: SnapshotRetentionReason::OwnershipUnproven {
                        gap: SnapshotOwnershipGap::ContainerLabel,
                    },
                },
                RetainedSnapshotImage {
                    image_id: "sha256:gap-d-no-tags".to_string(),
                    repo_tags: Vec::new(),
                    reason: SnapshotRetentionReason::OwnershipUnproven {
                        gap: SnapshotOwnershipGap::MissingRepoTag,
                    },
                },
            ],
        }
    );
    // 只对归属完整的镜像执行了删除，且删除命令按镜像 ID、不带 `-f`。
    assert_eq!(
        docker.removal_calls(),
        vec!["image rm sha256:good".to_string()]
    );
    assert_enumerates_managed_snapshots(&docker);
}

#[tokio::test]
async fn reclaims_clone_images_that_are_no_longer_protected() {
    let docker = FakeDocker::new();
    let agent = agent_id(102);
    let operation = Uuid::from_u128(11);
    docker.publish_snapshot(
        "sha256:clone",
        &agent,
        operation,
        "parent-container",
        Some(SnapshotOrigin::Clone),
    );

    let report = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("回收 clone 出生镜像");

    assert_eq!(
        report,
        SnapshotCleanupReport {
            removed: vec![RemovedSnapshotImage {
                image_id: "sha256:clone".to_string(),
                repo_tag: snapshot_tag(&agent, operation),
                agent_id: agent,
                operation_id: operation,
                origin: SnapshotOrigin::Clone,
            }],
            retained: Vec::new(),
        }
    );
}

#[tokio::test]
async fn keeps_images_still_referenced_by_stopped_or_unmanaged_containers() {
    let docker = FakeDocker::new();
    let agent = agent_id(103);
    let operation = Uuid::from_u128(21);
    docker.publish_snapshot("sha256:in-use", &agent, operation, "container-1", None);
    // 这个容器不在受管标签之内、也没有运行；它引用镜像的事实同样必须保护镜像。
    docker.attach_container("unmanaged-stopped-container", "sha256:in-use");

    let report = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("回收受管快照镜像");

    assert_eq!(
        report,
        SnapshotCleanupReport {
            removed: Vec::new(),
            retained: vec![RetainedSnapshotImage {
                image_id: "sha256:in-use".to_string(),
                repo_tags: vec![snapshot_tag(&agent, operation)],
                reason: SnapshotRetentionReason::ImageInUse,
            }],
        }
    );
    // 容器枚举必须覆盖全部容器（不加受管标签过滤），否则停止的未受管容器会被漏掉。
    assert!(
        docker
            .calls()
            .iter()
            .any(|call| call == "ps -aq --no-trunc")
    );
    assert_eq!(docker.removal_calls(), Vec::<String>::new());
}

#[tokio::test]
async fn keeps_images_matching_the_protection_sets() {
    let docker = FakeDocker::new();
    let first_agent = agent_id(104);
    let second_agent = agent_id(105);
    let clone_agent = agent_id(106);
    let first = Uuid::from_u128(31);
    let second = Uuid::from_u128(32);
    let third = Uuid::from_u128(33);
    docker.publish_snapshot(
        "sha256:protected-image",
        &first_agent,
        first,
        "container-1",
        None,
    );
    docker.publish_snapshot(
        "sha256:protected-operation",
        &second_agent,
        second,
        "container-2",
        None,
    );
    docker.publish_snapshot(
        "sha256:protected-clone",
        &clone_agent,
        third,
        "container-3",
        Some(SnapshotOrigin::Clone),
    );

    let retention = SnapshotRetention::new()
        // 收据与 binding 保存的镜像 ID 可能带前缀、大小写不同，归一化后仍须命中。
        .with_image_ids(["SHA256:PROTECTED-IMAGE"])
        .with_operation_ids([second])
        .with_clone_agent_ids([clone_agent.clone()]);

    let report = docker
        .client()
        .reclaim_snapshot_images(&retention)
        .await
        .expect("回收受管快照镜像");

    assert_eq!(
        report,
        SnapshotCleanupReport {
            removed: Vec::new(),
            retained: vec![
                RetainedSnapshotImage {
                    image_id: "sha256:protected-clone".to_string(),
                    repo_tags: vec![snapshot_tag(&clone_agent, third)],
                    reason: SnapshotRetentionReason::ProtectedCloneAgent {
                        agent_id: clone_agent.clone(),
                    },
                },
                RetainedSnapshotImage {
                    image_id: "sha256:protected-image".to_string(),
                    repo_tags: vec![snapshot_tag(&first_agent, first)],
                    reason: SnapshotRetentionReason::ProtectedImageId,
                },
                RetainedSnapshotImage {
                    image_id: "sha256:protected-operation".to_string(),
                    repo_tags: vec![snapshot_tag(&second_agent, second)],
                    reason: SnapshotRetentionReason::ProtectedOperationId {
                        operation_id: second,
                    },
                },
            ],
        }
    );
    assert_eq!(docker.removal_calls(), Vec::<String>::new());
}

#[tokio::test]
async fn retries_dependent_images_only_after_the_child_is_removed() {
    let docker = FakeDocker::new();
    let parent_agent = agent_id(107);
    let child_agent = agent_id(108);
    let operation = Uuid::from_u128(41);
    // 候选顺序刻意让父镜像排在前面：第一轮会被子镜像依赖挡住。
    docker.publish_snapshot(
        "sha256:parent-snapshot",
        &parent_agent,
        operation,
        "parent-container",
        Some(SnapshotOrigin::Generation),
    );
    docker.publish_snapshot(
        "sha256:child-snapshot",
        &child_agent,
        operation,
        "child-container",
        Some(SnapshotOrigin::Clone),
    );
    docker.depend_on_child("sha256:parent-snapshot", "sha256:child-snapshot");

    let report = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("回收受管快照镜像");

    assert_eq!(report.retained, Vec::<RetainedSnapshotImage>::new());
    assert_eq!(
        report
            .removed
            .iter()
            .map(|entry| entry.image_id.as_str())
            .collect::<Vec<_>>(),
        vec!["sha256:child-snapshot", "sha256:parent-snapshot"]
    );
    // 父镜像被重试过一次才成功；重试有界，并且全程不带 `-f`。
    assert_eq!(
        docker.removal_calls(),
        vec![
            "image rm sha256:parent-snapshot".to_string(),
            "image rm sha256:child-snapshot".to_string(),
            "image rm sha256:parent-snapshot".to_string(),
        ]
    );
}

#[tokio::test]
async fn keeps_images_the_daemon_refuses_to_delete() {
    let docker = FakeDocker::new();
    let agent = agent_id(109);
    let operation = Uuid::from_u128(51);
    docker.publish_snapshot("sha256:still-used", &agent, operation, "container-1", None);
    // daemon 的“在用/多引用”退档不是 Docker 故障，按保留记录而不是上抛错误。
    const REFUSAL: &str = "Error response from daemon: conflict: unable to delete sha256:still-used (must be forced) - image is being used by stopped container abc";
    docker.reject_removal("sha256:still-used", REFUSAL);

    let report = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("冲突按保留处理");

    assert_eq!(
        report,
        SnapshotCleanupReport {
            removed: Vec::new(),
            retained: vec![RetainedSnapshotImage {
                image_id: "sha256:still-used".to_string(),
                repo_tags: vec![snapshot_tag(&agent, operation)],
                reason: SnapshotRetentionReason::RemovalBlocked {
                    message: REFUSAL.to_string(),
                },
            }],
        }
    );
}

#[tokio::test]
async fn surfaces_real_docker_failures_instead_of_reporting_success() {
    let docker = FakeDocker::new();
    let agent = agent_id(110);
    let operation = Uuid::from_u128(61);
    docker.publish_snapshot("sha256:broken", &agent, operation, "container-1", None);
    docker.reject_removal(
        "sha256:broken",
        "Error response from daemon: driver overlay2 failed: invalid argument",
    );

    let error = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect_err("真实故障必须上抛");

    assert!(matches!(
        error,
        DockerError::CommandFailed(message) if message.contains("driver overlay2 failed")
    ));
}

#[tokio::test]
async fn protects_images_of_containers_that_disappear_during_inspection() {
    let docker = FakeDocker::new();
    let agent = agent_id(111);
    let operation = Uuid::from_u128(71);
    docker.publish_snapshot("sha256:kept", &agent, operation, "container-1", None);
    docker.attach_container("gone-container", "sha256:unrelated");
    docker.attach_container("keeper-container", "sha256:kept");
    docker.vanish_container("gone-container");

    let report = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("逐个核对仍然能证明占用");

    assert_eq!(
        report
            .retained
            .iter()
            .map(|entry| entry.reason.clone())
            .collect::<Vec<_>>(),
        vec![SnapshotRetentionReason::ImageInUse]
    );
    assert_eq!(docker.removal_calls(), Vec::<String>::new());
}

#[tokio::test]
async fn serializes_the_final_state_as_recovery_evidence() {
    let docker = FakeDocker::new();
    let reclaimed_agent = agent_id(112);
    let kept_agent = agent_id(113);
    let operation = Uuid::from_u128(81);
    docker.publish_snapshot(
        "sha256:reclaimed",
        &reclaimed_agent,
        operation,
        "container-1",
        None,
    );
    docker.publish_snapshot("sha256:kept", &kept_agent, operation, "container-2", None);

    let retention = SnapshotRetention::new().with_image_ids(["sha256:kept"]);
    let report = docker
        .client()
        .reclaim_snapshot_images(&retention)
        .await
        .expect("回收受管快照镜像");

    assert_eq!(
        serde_json::to_value(&report).expect("序列化回收报告"),
        serde_json::json!({
            "removed": [{
                "image_id": "sha256:reclaimed",
                "repo_tag": snapshot_tag(&reclaimed_agent, operation),
                "agent_id": reclaimed_agent,
                "operation_id": operation.to_string(),
                "origin": "generation",
            }],
            "retained": [{
                "image_id": "sha256:kept",
                "repo_tags": [snapshot_tag(&kept_agent, operation)],
                "reason": { "kind": "protected_image_id" },
            }],
        })
    );
}

#[tokio::test]
async fn clone_commit_overwrites_every_ownership_label() {
    let docker = FakeDocker::new();
    let agent = agent_id(114);
    let generation = Uuid::from_u128(91);
    docker.report_image_id("sha256:clone-image");

    let image_id = docker
        .client()
        .commit_clone_parent(agent.clone(), generation, "parent-container-id")
        .await
        .expect("提交父容器的可写层");

    assert_eq!(image_id, "sha256:clone-image".to_string());
    let commit = docker
        .calls()
        .into_iter()
        .find(|call| call.starts_with("commit "))
        .expect("提交调用");
    // 继承自父容器的归属必须被逐条覆盖；clone 出生用 generation 作为 operation。
    assert!(commit.contains("LABEL mai.team.managed=true"), "{commit}");
    assert!(
        commit.contains(&format!("LABEL mai.team.agent={agent}")),
        "{commit}"
    );
    assert!(
        commit.contains(&format!("LABEL mai.team.snapshot.operation={generation}")),
        "{commit}"
    );
    assert!(
        commit.contains("LABEL mai.team.snapshot.container=parent-container-id"),
        "{commit}"
    );
    assert!(
        commit.contains("LABEL mai.team.snapshot.origin=clone"),
        "{commit}"
    );
    assert!(
        commit.contains(&environment_snapshot_image_name(&agent, generation)),
        "{commit}"
    );
}

#[tokio::test]
async fn generation_commit_overwrites_every_ownership_label() {
    let docker = FakeDocker::new();
    let agent = agent_id(115);
    let operation = Uuid::from_u128(92);
    docker.report_image_id("sha256:generation-image");
    let request = EnvironmentSnapshotRequest::new(operation, agent.clone(), "stopped-container-id")
        .expect("快照请求");

    let receipt = docker
        .client()
        .commit_stopped_environment(&request)
        .await
        .expect("提交已停止容器的可写层");

    assert_eq!(receipt.operation_id, operation);
    assert_eq!(receipt.image_id, "sha256:generation-image".to_string());
    assert_eq!(
        receipt.image_name,
        environment_snapshot_image_name(&agent, operation)
    );
    let commit = docker
        .calls()
        .into_iter()
        .find(|call| call.starts_with("commit "))
        .expect("提交调用");
    assert!(commit.contains("LABEL mai.team.managed=true"), "{commit}");
    assert!(
        commit.contains(&format!("LABEL mai.team.agent={agent}")),
        "{commit}"
    );
    assert!(
        commit.contains(&format!("LABEL mai.team.snapshot.operation={operation}")),
        "{commit}"
    );
    assert!(
        commit.contains("LABEL mai.team.snapshot.container=stopped-container-id"),
        "{commit}"
    );
    assert!(
        commit.contains("LABEL mai.team.snapshot.origin=generation"),
        "{commit}"
    );
}

#[tokio::test]
async fn foreign_or_missing_instance_owner_never_authorizes_removal() {
    let docker = FakeDocker::new();
    let agent = agent_id(209);
    let operation = Uuid::from_u128(1001);
    let tags = vec![snapshot_tag(&agent, operation)];
    for (id, owner) in [
        ("sha256:foreign", Some(Uuid::from_u128(55))),
        ("sha256:unowned", None),
    ] {
        let mut labels = snapshot_labels(&agent, operation, "old-container", None);
        match owner {
            Some(owner) => {
                labels.insert("mai.team.snapshot.owner".into(), owner.to_string());
            }
            None => {
                labels.remove("mai.team.snapshot.owner");
            }
        }
        docker.publish_image(id, Some(&tags), &labels);
    }
    let report = docker
        .client()
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("保留外部镜像");
    assert_eq!(
        report,
        SnapshotCleanupReport {
            removed: vec![],
            retained: ["sha256:foreign", "sha256:unowned"]
                .into_iter()
                .map(|id| RetainedSnapshotImage {
                    image_id: id.into(),
                    repo_tags: tags.clone(),
                    reason: SnapshotRetentionReason::ForeignOwner,
                })
                .collect(),
        }
    );
    assert_eq!(docker.removal_calls(), Vec::<String>::new());
}

#[tokio::test]
async fn unscoped_client_cannot_collect_images() {
    let docker = FakeDocker::new();
    let client = DockerClient::new_with_binary("unused", docker.binary.clone());
    let report = client
        .reclaim_snapshot_images(&SnapshotRetention::new())
        .await
        .expect("未配置归属不回收");
    assert_eq!(report, SnapshotCleanupReport::default());
    assert_eq!(docker.calls(), Vec::<String>::new());
}
