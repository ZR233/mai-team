//! 驱动输入：固定验收根、独立 daemon、证据目录、fixture provider 与超时。
//!
//! 所有路径在解析阶段就落到具体值；`guard` 只做边界判定，不在场景代码里再拼路径。
//! 验收根、数据目录与证据目录集中在本次隔离实例内（`root/data` 是前一领域驱动 seed 的
//! 产品库），任何其它根（生产数据目录、`proddata` 路径）都在 `guard` 拒绝。

use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Result, error};

/// 固定验收根；独立库、独立夹具、独立证据都在这里。
pub const ACCEPTANCE_ROOT: &str =
    "/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources";

/// 独立验收 Docker daemon 的 unix socket；不是生产 daemon 的 socket。
pub const ACCEPTANCE_DOCKER_SOCKET: &str = "/run/mai-reference-acceptance-docker/docker.sock";

/// 固定 `DOCKER_HOST` 取值；任何其它取值都拒绝。
pub const ACCEPTANCE_DOCKER_HOST: &str = "unix:///run/mai-reference-acceptance-docker/docker.sock";

/// 夹具源码在验收根内的相对位置：`sources/private-fixture/repo`。
pub const FIXTURE_SOURCE_RELATIVE: &str = "sources/private-fixture/repo";

/// 管理员已发布的 Linux 固定版本快照（只读输入，驱动绝不修改）。
pub const LINUX_SOURCE: &str =
    "/mnt/docker_nvme/mai-references/linux/7b20d05fd38de8e0be168956f6fd241d750c7d69/repo";

/// 缺省 Agent 镜像引用；root 负责准备，驱动不 pull。
pub const DEFAULT_AGENT_IMAGE: &str = "mai-team-agent:latest";

/// 缺省 MCP sidecar 镜像引用。
pub const DEFAULT_SIDECAR_IMAGE: &str = "ghcr.io/zr233/mai-team-sidecar:latest";

/// fixture provider 的 provider id：与夹具 Agent 冻结的 `provider_id` 一致。
pub const PROVIDER_ID: &str = "acceptance";

/// fixture provider 的模型 slug：与夹具 Agent 冻结的 `model` 一致。
pub const MODEL_SLUG: &str = "acceptance-model";

/// provider 展示名与 base url 缺省值（HTTP Responses，SSE）。
pub const PROVIDER_NAME: &str = "Acceptance Fixture";
pub const PROVIDER_BASE_URL: &str = "http://127.0.0.1:18081/v1";

/// 隔离用 dummy token：只用于本地 fixture，禁止真实网络模型凭证。
pub const PROVIDER_TOKEN: &str = "acceptance-fixture-dummy-token";

/// 单次 turn 的等待上限与单次参考换代收敛上限。
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 900;

/// 使用说明。
pub const USAGE: &str = "\
隔离参考源的真实 Runtime 验收驱动（真实 AgentRuntime + 真实 PL 工具 + 真实容器）

  DOCKER_HOST=unix:///run/mai-reference-acceptance-docker/docker.sock \\
    cargo run -p mai-runtime --example shared_references_runtime -- [选项] <命令>

命令：
  config    只更新 provider/角色模型（HTTP Responses fixture）并核对生效路由
  tools     用真实 read_file/list_files/apply_patch/exec 工具验证读放行与写拒绝、
            非 A 项目读取失败、宿主原件摘要前后一致，并记录工具清单事实
  gate      活动 turn 期间保存下一修订（WhenIdle），再覆盖 StopAndApply 取消行为，
            验证旧 turn 租约固定、排队 turn 必须等到应用完成，最后收敛且还原夹具配置
  lifecycle 用真实 spawn_agent 从可信 projectA parent 产生 child 再 grandchild，
            证明新 children 继承 project A 且 refs 可读；关闭后重建 Runtime 复用同一物理容器
  channels-limits  单独复核真实命令输出与落盘硬上限
  channels  验证上传、stdin、后台命令与输出上限使用固定租约
  projects  创建自带引用的项目，验证数据保留、自动审查与删除清理
  injection 验证参考资料中的 AGENTS.md/skills 不自动加载为指令或工具

选项：
  --root <path>                 验收根；只接受固定验收根，其它取值一律拒绝
  --gate-mode <mode>            idle（只测 WhenIdle）| stop（只测 StopAndApply）| full（缺省）
  --phase <phase>               lifecycle: spawn | resume | full（缺省）
  --digest <scope>              fixture（缺省，只摘要夹具源）| all（同时摘要 Linux 快照）
  --turn-timeout-secs <n>      单次 turn 等待上限（缺省 600）
  --operation-timeout-secs <n> 单次参考换代收敛上限（缺省 900）
  --agent-image <ref>           Agent 容器镜像引用（缺省 mai-team-agent:latest）
  --sidecar-image <ref>         MCP sidecar 镜像引用
  --provider-base-url <url>     fixture provider 地址（缺省 http://127.0.0.1:18081/v1）
  --help                        打印本说明

环境变量回退：MAI_ACCEPTANCE_ROOT、MAI_ACCEPTANCE_AGENT_IMAGE、
MAI_ACCEPTANCE_SIDECAR_IMAGE、MAI_ACCEPTANCE_PROVIDER_BASE_URL、
MAI_ACCEPTANCE_PROVIDER_EVIDENCE_DIR、MAI_ACCEPTANCE_SYSTEM_AGENTS、MAI_ACCEPTANCE_SYSTEM_SKILLS。";

/// 驱动子命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Config,
    Tools,
    Channels,
    ChannelsLimits,
    Gate,
    Lifecycle,
    Projects,
    Injection,
}

impl Command {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "config" => Ok(Self::Config),
            "tools" => Ok(Self::Tools),
            "channels" => Ok(Self::Channels),
            "channels-limits" => Ok(Self::ChannelsLimits),
            "gate" => Ok(Self::Gate),
            "lifecycle" => Ok(Self::Lifecycle),
            "projects" => Ok(Self::Projects),
            "injection" => Ok(Self::Injection),
            other => Err(error(
                "unknown_command",
                format!("未知命令 `{other}`；只支持 config/tools/gate/lifecycle"),
            )),
        }
    }

    /// 证据与日志里的稳定命令名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Tools => "tools",
            Self::Channels => "channels",
            Self::ChannelsLimits => "channels-limits",
            Self::Gate => "gate",
            Self::Lifecycle => "lifecycle",
            Self::Projects => "projects",
            Self::Injection => "injection",
        }
    }
}

/// `gate` 命令覆盖的应用策略组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    Idle,
    Stop,
    Full,
}

impl GateMode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "idle" => Ok(Self::Idle),
            "stop" => Ok(Self::Stop),
            "full" => Ok(Self::Full),
            other => Err(error(
                "unknown_gate_mode",
                format!("未知 --gate-mode `{other}`；只支持 idle/stop/full"),
            )),
        }
    }

    /// 是否覆盖 WhenIdle 路径。
    pub fn covers_when_idle(self) -> bool {
        matches!(self, Self::Idle | Self::Full)
    }

    /// 是否覆盖 StopAndApply 路径。
    pub fn covers_stop_and_apply(self) -> bool {
        matches!(self, Self::Stop | Self::Full)
    }
}

/// `lifecycle` 命令的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    Spawn,
    Resume,
    Full,
}

impl LifecyclePhase {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "spawn" => Ok(Self::Spawn),
            "resume" => Ok(Self::Resume),
            "full" => Ok(Self::Full),
            other => Err(error(
                "unknown_phase",
                format!("未知 --phase `{other}`；只支持 spawn/resume/full"),
            )),
        }
    }
}

/// 宿主内容摘要范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestScope {
    /// 只摘要夹具源（Linux 整树摘要很慢，且由真实验证器覆盖）。
    Fixture,
    /// 同时摘要 Linux 快照。
    All,
}

impl DigestScope {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "fixture" => Ok(Self::Fixture),
            "all" => Ok(Self::All),
            other => Err(error(
                "unknown_digest_scope",
                format!("未知 --digest `{other}`；只支持 fixture/all"),
            )),
        }
    }

    /// 是否包含 Linux 快照。
    pub fn includes_linux(self) -> bool {
        matches!(self, Self::All)
    }
}

/// 解析后的驱动配置。
#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub command: Command,
    pub help: bool,
    pub root: PathBuf,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub evidence_dir: PathBuf,
    pub provider_evidence_dir: PathBuf,
    pub policy_path: PathBuf,
    pub fixture_root: PathBuf,
    pub fixture_repo: PathBuf,
    pub linux_source: PathBuf,
    pub projects_root: PathBuf,
    pub sessions_root: PathBuf,
    pub cache_root: PathBuf,
    pub artifacts_root: PathBuf,
    pub system_agents_root: PathBuf,
    pub system_skills_root: PathBuf,
    pub agent_image: String,
    pub sidecar_image: String,
    pub provider_base_url: String,
    pub gate_mode: GateMode,
    pub lifecycle_phase: LifecyclePhase,
    pub digest_scope: DigestScope,
    pub turn_timeout: Duration,
    pub operation_timeout: Duration,
}

impl DriverConfig {
    /// 解析 CLI 与环境变量。
    pub fn from_args(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let arguments = arguments.collect::<Vec<_>>();
        let mut cli = CliOptions::default();
        let mut index = 0usize;
        while index < arguments.len() {
            let argument = arguments[index].clone();
            index += 1;
            let mut value = |flag: &str| -> Result<String> {
                let value = arguments
                    .get(index)
                    .cloned()
                    .ok_or_else(|| error("missing_argument", format!("{flag} 需要一个取值")))?;
                index += 1;
                Ok(value)
            };
            match argument.as_str() {
                "--help" | "-h" => cli.help = true,
                "--root" => cli.root = Some(value("--root")?),
                "--gate-mode" => cli.gate_mode = Some(value("--gate-mode")?),
                "--phase" => cli.phase = Some(value("--phase")?),
                "--digest" => cli.digest = Some(value("--digest")?),
                "--turn-timeout-secs" => cli.turn_timeout = Some(value("--turn-timeout-secs")?),
                "--operation-timeout-secs" => {
                    cli.operation_timeout = Some(value("--operation-timeout-secs")?)
                }
                "--agent-image" => cli.agent_image = Some(value("--agent-image")?),
                "--sidecar-image" => cli.sidecar_image = Some(value("--sidecar-image")?),
                "--provider-base-url" => {
                    cli.provider_base_url = Some(value("--provider-base-url")?)
                }
                other if other.starts_with("--") => {
                    return Err(error(
                        "unknown_argument",
                        format!("未知参数 `{other}`；见 --help"),
                    ));
                }
                other => {
                    if cli.command.is_some() {
                        return Err(error(
                            "duplicate_command",
                            format!("一次只能指定一个子命令，重复出现 `{other}`"),
                        ));
                    }
                    cli.command = Some(other.to_string());
                }
            }
        }

        if cli.help {
            return Ok(Self::placeholder(cli));
        }

        let root = PathBuf::from(
            cli.root
                .clone()
                .or_else(|| std::env::var("MAI_ACCEPTANCE_ROOT").ok())
                .unwrap_or_else(|| ACCEPTANCE_ROOT.to_string()),
        );
        let data_dir = root.join("data");
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let server_dir = manifest_dir.join("..").join("mai-server");
        let command = Command::parse(
            cli.command
                .as_deref()
                .ok_or_else(|| error("missing_command", "缺少子命令；见 --help"))?,
        )?;

        Ok(Self {
            command,
            help: false,
            db_path: data_dir.join("mai-team.sqlite3"),
            data_dir,
            evidence_dir: root.join("evidence"),
            provider_evidence_dir: std::env::var("MAI_ACCEPTANCE_PROVIDER_EVIDENCE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| root.join("provider-evidence")),
            policy_path: root.join("config").join("reference-sources.toml"),
            fixture_root: root.join("sources").join("private-fixture"),
            fixture_repo: root.join(FIXTURE_SOURCE_RELATIVE),
            linux_source: PathBuf::from(LINUX_SOURCE),
            projects_root: root.join("projects"),
            sessions_root: root.join("data").join("sessions"),
            cache_root: root.join("cache"),
            artifacts_root: root.join("artifacts"),
            system_agents_root: std::env::var("MAI_ACCEPTANCE_SYSTEM_AGENTS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| server_dir.join("system-agents")),
            system_skills_root: std::env::var("MAI_ACCEPTANCE_SYSTEM_SKILLS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| server_dir.join("system-skills")),
            agent_image: cli
                .agent_image
                .or_else(|| std::env::var("MAI_ACCEPTANCE_AGENT_IMAGE").ok())
                .unwrap_or_else(|| DEFAULT_AGENT_IMAGE.to_string()),
            sidecar_image: cli
                .sidecar_image
                .or_else(|| std::env::var("MAI_ACCEPTANCE_SIDECAR_IMAGE").ok())
                .unwrap_or_else(|| DEFAULT_SIDECAR_IMAGE.to_string()),
            provider_base_url: cli
                .provider_base_url
                .or_else(|| std::env::var("MAI_ACCEPTANCE_PROVIDER_BASE_URL").ok())
                .unwrap_or_else(|| PROVIDER_BASE_URL.to_string()),
            gate_mode: GateMode::parse(cli.gate_mode.as_deref().unwrap_or("full"))?,
            lifecycle_phase: LifecyclePhase::parse(cli.phase.as_deref().unwrap_or("full"))?,
            digest_scope: DigestScope::parse(cli.digest.as_deref().unwrap_or("fixture"))?,
            turn_timeout: seconds(
                cli.turn_timeout,
                DEFAULT_TURN_TIMEOUT_SECS,
                "--turn-timeout-secs",
            )?,
            operation_timeout: seconds(
                cli.operation_timeout,
                DEFAULT_OPERATION_TIMEOUT_SECS,
                "--operation-timeout-secs",
            )?,
            root,
        })
    }

    fn placeholder(cli: CliOptions) -> Self {
        let root = PathBuf::from(ACCEPTANCE_ROOT);
        Self {
            command: Command::Config,
            help: true,
            data_dir: root.join("data"),
            db_path: root.join("data").join("mai-team.sqlite3"),
            evidence_dir: root.join("evidence"),
            provider_evidence_dir: root.join("provider-evidence"),
            policy_path: root.join("config").join("reference-sources.toml"),
            fixture_root: root.join("sources").join("private-fixture"),
            fixture_repo: root.join(FIXTURE_SOURCE_RELATIVE),
            linux_source: PathBuf::from(LINUX_SOURCE),
            projects_root: root.join("projects"),
            sessions_root: root.join("data").join("sessions"),
            cache_root: root.join("cache"),
            artifacts_root: root.join("artifacts"),
            system_agents_root: PathBuf::from("system-agents"),
            system_skills_root: PathBuf::from("system-skills"),
            agent_image: cli
                .agent_image
                .unwrap_or_else(|| DEFAULT_AGENT_IMAGE.to_string()),
            sidecar_image: cli
                .sidecar_image
                .unwrap_or_else(|| DEFAULT_SIDECAR_IMAGE.to_string()),
            provider_base_url: cli
                .provider_base_url
                .unwrap_or_else(|| PROVIDER_BASE_URL.to_string()),
            gate_mode: GateMode::Full,
            lifecycle_phase: LifecyclePhase::Full,
            digest_scope: DigestScope::Fixture,
            turn_timeout: Duration::from_secs(DEFAULT_TURN_TIMEOUT_SECS),
            operation_timeout: Duration::from_secs(DEFAULT_OPERATION_TIMEOUT_SECS),
            root,
        }
    }
}

#[derive(Debug, Default)]
struct CliOptions {
    command: Option<String>,
    root: Option<String>,
    gate_mode: Option<String>,
    phase: Option<String>,
    digest: Option<String>,
    turn_timeout: Option<String>,
    operation_timeout: Option<String>,
    agent_image: Option<String>,
    sidecar_image: Option<String>,
    provider_base_url: Option<String>,
    help: bool,
}

fn seconds(value: Option<String>, fallback: u64, flag: &str) -> Result<Duration> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(fallback));
    };
    let parsed = value.parse::<u64>().map_err(|parse| {
        error(
            "invalid_timeout",
            format!("{flag} 不是合法秒数（{value}）：{parse}"),
        )
    })?;
    if parsed == 0 {
        return Err(error("invalid_timeout", format!("{flag} 必须大于 0")));
    }
    Ok(Duration::from_secs(parsed))
}
