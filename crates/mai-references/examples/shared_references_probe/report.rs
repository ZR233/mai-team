//! 运行报告：验收点判定 + 事实 + 覆盖边界，最后追加写入证据日志并打印。
//!
//! 退出码语义（不把未验证写成通过）：
//! - `2`：至少一个验收点明确失败（事实违反了契约）。
//! - `0`：本轮没有失败判定；`blocked`/`not_run` 逐条列出，并在 `verdict` 里计数，
//!   绝不把这两种状态写成“通过”。
//! - `1`：证据写入或报告组装失败（无法留下可复核的结论）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::config::ProbeConfig;
use crate::error::Result;
use crate::evidence;

/// 单个验收点的判定状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    /// 事实满足契约。
    Pass,
    /// 事实违反契约（明确失败）。
    Fail,
    /// 当前状态或前置步骤使判定无法进行（例如换代进行中、尚未绑定参考源）。
    Blocked,
    /// 本轮没有执行该验收点所需的步骤（例如还没跑对应场景）。
    NotRun,
}

impl CheckState {
    /// 证据里的状态名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Blocked => "blocked",
            Self::NotRun => "not_run",
        }
    }
}

/// 一个验收点记录。
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// 验收点 ID（例如 `T14.same_source_same_mount`）。
    pub id: String,
    pub state: CheckState,
    /// 中文说明：为什么是这个状态。
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}

impl Check {
    /// 通过。
    pub fn pass(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: CheckState::Pass,
            reason: reason.into(),
            evidence: None,
        }
    }

    /// 失败。
    pub fn fail(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: CheckState::Fail,
            reason: reason.into(),
            evidence: None,
        }
    }

    /// 阻塞（无法判定）。
    pub fn blocked(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: CheckState::Blocked,
            reason: reason.into(),
            evidence: None,
        }
    }

    /// 本轮未执行该步骤。
    pub fn not_run(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: CheckState::NotRun,
            reason: reason.into(),
            evidence: None,
        }
    }

    /// 附带结构化证据。
    pub fn with_evidence(mut self, evidence: impl Serialize) -> Self {
        self.evidence = serde_json::to_value(evidence).ok();
        self
    }
}

/// 一轮运行的报告累加器。
pub struct Report {
    run_id: Uuid,
    command: &'static str,
    scenario: Option<&'static str>,
    root: PathBuf,
    evidence_dir: PathBuf,
    evidence_log: PathBuf,
    started_at: DateTime<Utc>,
    docker_host: Option<String>,
    config: Value,
    boundaries: Map<String, Value>,
    checks: Vec<Check>,
    facts: Map<String, Value>,
}

impl Report {
    /// 为一轮运行建立报告；配置摘要与边界说明一并写入，便于 root 复核。
    pub fn new(config: &ProbeConfig) -> Result<Self> {
        let scenario = match config.command {
            crate::config::Command::Apply => Some(config.scenario_or_bind().as_str()),
            _ => config.scenario.map(|scenario| scenario.as_str()),
        };
        let mut report = Self {
            run_id: Uuid::new_v4(),
            command: config.command.as_str(),
            scenario,
            root: config.root.clone(),
            evidence_dir: config.evidence_dir.clone(),
            evidence_log: config.evidence_log.clone(),
            started_at: Utc::now(),
            docker_host: None,
            config: config_summary(config)?,
            boundaries: Map::new(),
            checks: Vec::new(),
            facts: Map::new(),
        };
        report.boundary(
            "activate",
            "不模拟模型：ReferenceEnvironmentHost::activate 只记录绑定，不启动 PL Thread 或模型 turn；\
             真实 Runtime 的 Thread 恢复、工具读取与 UI 由 root 另测",
        );
        report.boundary(
            "file_tools",
            "T24/T25（read_file/list_files/apply_patch/上传等工具层）属于 PL 工具层，本驱动不覆盖，只覆盖 Docker 只读事实",
        );
        report.boundary(
            "mcp_tools",
            "sidecar 只验证真实容器与挂载只读；MCP 协议握手与必需服务清单由真实 Runtime 覆盖",
        );
        report.boundary(
            "review_workspace",
            "自动 PR 审查（T20）与 review view 语义未在本驱动覆盖；本驱动只保留真实只读 view 卷身份",
        );
        report.boundary(
            "crash_injection",
            "崩溃注入只在指定 phase 落盘收据后写一次已触发标记并 _exit(137)；恢复由 root 反复运行 recover 完成，驱动不做重试循环",
        );
        report.boundary(
            "docker_faults",
            "T32 的磁盘满/Docker offline/必需 MCP 失败可用 --docker-binary 指向 root 提供的 wrapper 注入；驱动只如实报告失败与保留的旧容器",
        );
        Ok(report)
    }

    /// 记录 Docker 连接目标。
    pub fn set_docker_host(&mut self, host: &str) {
        self.docker_host = Some(host.to_string());
    }

    /// 记录一条边界说明。
    pub fn boundary(&mut self, key: &str, message: &str) {
        self.boundaries
            .insert(key.to_string(), Value::String(message.to_string()));
    }

    /// 记录一个事实块。
    pub fn fact(&mut self, key: &str, value: impl Serialize) -> Result<()> {
        self.facts
            .insert(key.to_string(), serde_json::to_value(value)?);
        Ok(())
    }

    /// 追加一个验收点判定。
    pub fn check(&mut self, check: Check) {
        self.checks.push(check);
    }

    /// 退出码：失败 2，其余 0（blocked/not_run 逐条列出，不冒充通过）。
    pub fn exit_code(&self) -> u8 {
        if self
            .checks
            .iter()
            .any(|check| check.state == CheckState::Fail)
        {
            2
        } else {
            0
        }
    }

    /// 本轮判定的汇总：通过/失败/阻塞/未执行 + 是否全部通过。
    pub fn verdict(&self) -> Value {
        let count = |state: CheckState| {
            self.checks
                .iter()
                .filter(|check| check.state == state)
                .count()
        };
        let pending = self
            .checks
            .iter()
            .filter(|check| check.state == CheckState::Blocked)
            .map(|check| format!("{}：{}", check.id, check.reason))
            .collect::<Vec<_>>();
        let not_run = self
            .checks
            .iter()
            .filter(|check| check.state == CheckState::NotRun)
            .map(|check| check.id.clone())
            .collect::<Vec<_>>();
        serde_json::json!({
            "passed": count(CheckState::Pass),
            "failed": count(CheckState::Fail),
            "blocked": count(CheckState::Blocked),
            "not_run": count(CheckState::NotRun),
            "all_checks_passed": self.checks.iter().all(|check| check.state == CheckState::Pass),
            "pending_reasons": pending,
            "not_run_checks": not_run,
        })
    }

    /// 组装最终 JSON：事实、边界、验收点、以及跨轮覆盖边界。
    pub fn to_value(&self) -> Result<Value> {
        let history = match evidence::read_log_at(&self.evidence_log) {
            Ok(entries) => evidence::check_history(&entries, &self.checks),
            Err(_) => BTreeMap::new(),
        };
        let coverage = history
            .into_iter()
            .map(|(id, entry)| {
                let states = entry
                    .states
                    .into_iter()
                    .map(|(state, count)| (state, Value::from(count)))
                    .collect::<Map<String, Value>>();
                (
                    id,
                    serde_json::json!({
                        "ever_passed": entry.ever_passed,
                        "states": Value::Object(states),
                        "last_reason": entry.last_reason,
                    }),
                )
            })
            .collect::<Map<String, Value>>();

        Ok(serde_json::json!({
            "probe": "mai-references-acceptance",
            "report_version": 1,
            "run_id": self.run_id,
            "command": self.command,
            "scenario": self.scenario,
            "started_at": self.started_at,
            "finished_at": Utc::now(),
            "root": self.root,
            "docker_host": self.docker_host,
            "exit_code": self.exit_code(),
            "verdict": self.verdict(),
            "config": self.config,
            "boundaries": Value::Object(self.boundaries.clone()),
            "checks": self.checks,
            "facts": Value::Object(self.facts.clone()),
            "coverage": Value::Object(coverage),
        }))
    }

    /// 追加到证据日志并打印；返回进程退出码。
    pub fn finish(self) -> u8 {
        let exit_code = self.exit_code();
        let value = match self.to_value() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("组装证据失败：{error}");
                return 1;
            }
        };
        if let Err(error) = evidence::append_at(&self.evidence_dir, &self.evidence_log, &value) {
            eprintln!("证据写入失败：{error}");
            // 证据没落盘时不能按“通过”退出。
            return 1;
        }
        match serde_json::to_string_pretty(&value) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("打印证据失败：{error}");
                return 1;
            }
        }
        exit_code
    }
}

fn config_summary(config: &ProbeConfig) -> Result<Value> {
    Ok(serde_json::json!({
        "command": config.command.as_str(),
        "scenario": config.scenario.map(|scenario| scenario.as_str()),
        "root": config.root,
        "db_path": config.db_path,
        "evidence_log": config.evidence_log,
        "policy_path": config.policy_path,
        "fixture_repo": config.fixture_repo,
        "linux_source": config.linux_source,
        "linux_alias": config.linux_alias,
        "private_alias": config.private_alias,
        "allowed_roots": config.allowed_roots(),
        "agent_image": config.agent_image,
        "sidecar_image": config.sidecar_image,
        "docker_binary": config.docker_binary,
        "trust": config.trust,
        "provision": config.provision,
        "seed_bindings": config.seed_bindings,
        "inject_phase": config.inject_phase.map(|phase| phase.to_string()),
        "host_digest": config.host_digest,
        "timeout_secs": config.operation_timeout.as_secs(),
        "revalidate_sources": config.revalidate_sources,
        "extra_allowed_roots": config.extra_allowed_roots,
        "check_commands": config.check_commands,
    }))
}
