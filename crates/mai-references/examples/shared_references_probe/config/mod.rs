//! 驱动输入：JSON 配置文件、CLI 子命令与少量环境变量回退。
//!
//! 优先级从高到低：CLI 参数 → 环境变量 → JSON 配置文件 → 固定缺省值。所有路径在
//! 解析阶段就落到具体值，`guard` 只做边界判定，不在业务代码里再拼路径。
//!
//! 子模块按职责拆分，解析优先级与配置字段保持不变：
//! - `options`：子命令/场景/信任模式/覆盖范围/摘要范围等枚举取值；
//! - `resolve`：CLI、环境变量与 JSON 配置文件的解析与合并（CLI → env → 文件 → 缺省）。

mod options;
mod resolve;

pub use options::{Command, DigestScope, ProvisionChoice, Scenario, TrustChoice};

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use mai_protocol::ReferenceApplyPhase;
use serde::{Deserialize, Serialize};

use crate::error::{ProbeError, Result, err};

/// 固定验收根：独立库、独立夹具、独立证据都在这里，绝不复用生产数据目录。
pub const ACCEPTANCE_ROOT: &str =
    "/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources";

/// 独立验收 Docker daemon 的 unix socket；它不是生产 daemon 的 socket。
pub const ACCEPTANCE_DOCKER_SOCKET: &str = "/run/mai-reference-acceptance-docker/docker.sock";

/// 管理员已发布的 Linux 固定版本快照（只读输入，驱动绝不修改）。
pub const PUBLISHED_LINUX_SOURCE: &str =
    "/mnt/docker_nvme/mai-references/linux/7b20d05fd38de8e0be168956f6fd241d750c7d69/repo";

/// 夹具源码在验收根内的相对位置：`sources/private-fixture/repo`。
pub const FIXTURE_SOURCE_RELATIVE: &str = "sources/private-fixture/repo";

/// 缺省 Agent 镜像引用；root 必须先构建/拉取，驱动不自动 pull。
pub const DEFAULT_AGENT_IMAGE: &str = "mai-team-agent:latest";

/// 缺省 MCP sidecar 镜像引用。
pub const DEFAULT_SIDECAR_IMAGE: &str = "ghcr.io/zr233/mai-team-sidecar:latest";

/// 单个 Agent 换代的等待上限；超过即失败，不无限等待。
pub const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 900;

/// 使用说明；`--help` 与参数错误都会打印它。
pub const USAGE: &str = "\
隔离参考源验收驱动（只驱动领域层与 Docker；不模拟模型）

  DOCKER_HOST=unix:///run/mai-reference-acceptance-docker/docker.sock \\
    cargo run -p mai-references --example shared_references_probe -- [选项] <命令>

命令：
  seed     建立/复用夹具项目与 Agent，创建初始真实环境（卷、可写层标记），登记参考源
  apply    保存目标配置并驱动应用状态机到收敛（--scenario 决定本轮目标）
  status   只读收集数据库 + Docker 事实并判定各验收点（不修改任何状态）
  recover  先 restore_admission，再按最新收据 run_operation 恢复，最后 status
  clear    把项目绑定清空并驱动到收敛（空清除必须真实撤销挂载）
  delete   收敛后删除 Agent/项目记录并注销源；绝不删除宿主源码

选项：
  --config <path>        JSON 配置文件（serde 严格解析，未知字段报错）
  --root <path>          验收根；只接受固定验收根，其它取值（含 proddata）一律拒绝
  --scenario <name>      apply 的目标：bind | metadata_only | clear | reuse | concurrent
  --inject-phase <phase> 在指定 phase 落盘收据后硬退出一次：
                         draining | snapshotting | creating | verifying | rebinding |
                         publishing | cleanup_pending
  --agent-image <ref>    Agent 容器镜像引用（缺省 mai-team-agent:latest）
  --sidecar-image <ref>  MCP sidecar 镜像引用
  --docker-binary <path> docker CLI 路径；root 可用 wrapper 注入外部故障
  --trust <mode>         trusted_owner_and_permissions（缺省）| unrestricted（仅离线夹具）
  --provision <mode>     canonical（缺省，4 个 Agent）| all（全部 8 个 Agent）
  --seed-bindings <list> seed 阶段写入的项目绑定别名，逗号分隔；缺省空（先空环境后换代）
  --host-digest <scope>  fixture（缺省）| all | none：宿主内容摘要覆盖范围
  --timeout-secs <n>     单个换代等待上限秒数
  --revalidate-sources   在 status 里对源做深度重验证（会重新扫描整棵 Linux 树）
  --help                 打印本说明

环境变量回退：MAI_ACCEPTANCE_ROOT、MAI_ACCEPTANCE_AGENT_IMAGE、
MAI_ACCEPTANCE_SIDECAR_IMAGE、MAI_ACCEPTANCE_DOCKER_BIN、MAI_ACCEPTANCE_FIXTURE_ROOT、
MAI_ACCEPTANCE_LINUX_SOURCE。";

/// 解析后的驱动配置。
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub command: Command,
    pub scenario: Option<Scenario>,
    pub help: bool,
    pub root: PathBuf,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub fixture_root: PathBuf,
    pub fixture_repo: PathBuf,
    pub evidence_dir: PathBuf,
    pub evidence_log: PathBuf,
    pub policy_path: PathBuf,
    pub linux_source: PathBuf,
    pub linux_alias: String,
    pub private_alias: String,
    pub agent_image: String,
    pub sidecar_image: String,
    pub docker_binary: String,
    pub trust: TrustChoice,
    pub provision: ProvisionChoice,
    pub seed_bindings: Vec<String>,
    pub inject_phase: Option<ReferenceApplyPhase>,
    pub host_digest: DigestScope,
    pub operation_timeout: Duration,
    pub revalidate_sources: bool,
    pub extra_allowed_roots: Vec<PathBuf>,
    pub check_commands: Vec<String>,
}

impl ProbeConfig {
    /// 场景：CLI/文件都没给时，`apply` 使用 `bind`。
    pub fn scenario_or_bind(&self) -> Scenario {
        self.scenario.unwrap_or(Scenario::Bind)
    }

    /// 全部允许根：夹具版本目录、Linux 版本目录，以及配置里显式追加的允许根。
    pub fn allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.fixture_root.clone()];
        if let Some(parent) = self.linux_source.parent() {
            roots.push(PathBuf::from(parent));
        }
        roots.extend(self.extra_allowed_roots.iter().cloned());
        roots
    }
}
