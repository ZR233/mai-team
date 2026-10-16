//! 命令实现：seed / apply / status / recover / clear / delete。
//!
//! 统一流程：隔离边界检查 → 能力探测 → 打开独立库 → 登记/复用源 → 采集事实 → 判定 →
//! 追加证据。`status` 全程只读；其余命令只写本验收根下的数据库与 Docker 资源，
//! 绝不删除宿主源码，也绝不复位数据库。
//!
//! 子模块按职责拆分，命令语义、证据字段与退出码保持不变：
//! - `sources`：部署策略、夹具源码、源登记与验证等待；
//! - `fixtures`：项目/Agent 夹具持久化与资源状态投影；
//! - `targets`：别名绑定与目标配置保存；
//! - `operations`：收据驱动的换代、重新入队、并发目标与 ensure 复用探测；
//! - `environments`：`seed` 与删除路径的容器/卷收尾；
//! - `lifecycle`：apply/status/recover/clear；
//! - `delete`：删除引用/项目/Agent 与注销源。

mod delete;
mod environments;
mod fixtures;
mod lifecycle;
mod operations;
mod sources;
mod targets;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mai_docker::{DockerClient, ManagedContainer};
use mai_protocol::{
    NewReferenceSourceRequest, ProjectReferenceBinding, ProjectReferenceConfiguration,
    ReferenceApplyPhase, ReferenceApplyPolicy, ReferenceSource, ReplaceProjectReferencesRequest,
};
use mai_references::{
    ReferenceApplicationObserver, ReferenceApplications, ReferenceCatalog, SourceTrustRequirement,
    load_reference_deployment_policy,
};
use mai_store::{MaiStore, PersistedReferenceOperation};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::checks::{self, CheckInput};
use crate::config::{Command, ProbeConfig, Scenario, TrustChoice};
use crate::environment::{self, AgentFacts, Probe, Sources};
use crate::error::{CommandFailure, ProbeError, Result, err};
use crate::evidence;
use crate::fixtures::{FixtureSet, ProjectKind};
use crate::guard;
use crate::host::ProbeHost;
use crate::inject::PhaseCrashInjector;
use crate::report::{Check, Report};

use delete::delete;
use environments::seed;
use lifecycle::{apply, clear, recover, status};
use sources::write_policy_file;

/// 命令入口：任何失败都以非零退出码结束，并带上命令名。
pub async fn run(config: ProbeConfig) -> std::result::Result<Report, CommandFailure> {
    let command = config.command.as_str();
    let mut report = Report::new(&config).map_err(|error| CommandFailure {
        command: command.to_string(),
        error,
    })?;
    guard::ensure_isolated_root(&config).map_err(|error| CommandFailure {
        command: command.to_string(),
        error,
    })?;
    let target = guard::ensure_isolated_docker_host(&config).map_err(|error| CommandFailure {
        command: command.to_string(),
        error,
    })?;
    report.set_docker_host(&target.host);
    report
        .fact(
            "docker_target",
            json!({ "host": target.host, "socket": target.socket, "binary": target.binary }),
        )
        .map_err(|error| CommandFailure {
            command: command.to_string(),
            error,
        })?;

    let outcome = match config.command {
        Command::Seed => seed(&config, &mut report).await,
        Command::Apply => apply(&config, &mut report).await,
        Command::Status => status(&config, &mut report).await,
        Command::Recover => recover(&config, &mut report).await,
        Command::Clear => clear(&config, &mut report).await,
        Command::Delete => delete(&config, &mut report).await,
    };
    outcome.map_err(|error| CommandFailure {
        command: command.to_string(),
        error,
    })?;
    Ok(report)
}

/// 采集到的事实集合。
struct FactsBundle {
    agents: Vec<AgentFacts>,
    sources: Vec<environment::SourceFacts>,
    containers: Vec<ManagedContainer>,
    host: crate::host::HostEvidence,
}

/// 构造探针句柄：能力探测、独立库、领域组件与真实环境回调。
async fn build_probe(config: &ProbeConfig, report: &mut Report) -> Result<Probe> {
    guard::ensure_directory(&config.root)?;
    guard::ensure_directory(&config.data_dir)?;
    guard::ensure_directory(&config.evidence_dir)?;
    if let Some(parent) = config.policy_path.parent() {
        guard::ensure_directory(parent)?;
    }

    let docker =
        DockerClient::new_with_binary(config.agent_image.clone(), config.docker_binary.clone());
    // 远程/非 Linux/旧 Engine/旧内核一律 fail closed。
    let capabilities = docker.probe_capabilities().await?;
    report.fact(
        "docker_capabilities",
        json!({
            "os": capabilities.os,
            "server_version": capabilities.server_version,
            "kernel_version": capabilities.kernel_version,
            "api_version": capabilities.api_version,
            "recursive_read_only": capabilities.recursive_read_only,
        }),
    )?;

    write_policy_file(config)?;
    let policy = load_reference_deployment_policy(&config.policy_path)?;
    // 部署文件不包含信任模式开关；离线夹具模式必须显式在这里生效。
    let policy = policy.with_source_trust_requirement(match config.trust {
        TrustChoice::TrustedOwnerAndPermissions => {
            SourceTrustRequirement::TrustedOwnerAndPermissions
        }
        TrustChoice::Unrestricted => SourceTrustRequirement::Unrestricted,
    });
    if config.trust == TrustChoice::Unrestricted {
        report.boundary(
            "source_trust",
            "本次使用 SourceTrustRequirement::Unrestricted（只用于离线夹具）：跳过源归属与权限检查，路径/类型/链接检查仍然执行",
        );
    }

    let store = Arc::new(MaiStore::open_in_data_dir(&config.data_dir).await?);
    let docker = docker.with_snapshot_owner(mai_references::reference_snapshot_owner(&store)?);
    let catalog = ReferenceCatalog::new(Arc::clone(&store), Some(policy.clone()));
    let injector = config.inject_phase.map(|phase| {
        let marker = config.evidence_dir.join(format!("injected-{phase}.json"));
        Arc::new(PhaseCrashInjector::new(phase, None, marker))
    });
    if let Some(injector) = &injector {
        report.boundary(
            "inject_phase",
            &format!(
                "已装配崩溃注入：{} 收据落盘后写标记并 _exit(137)；标记 {} 存在时本轮不再注入",
                config
                    .inject_phase
                    .map(|phase| phase.to_string())
                    .unwrap_or_default(),
                injector.marker().display()
            ),
        );
    }
    let observer: Option<Arc<dyn ReferenceApplicationObserver>> =
        injector.map(|injector| -> Arc<dyn ReferenceApplicationObserver> { injector });
    let applications = ReferenceApplications::new(
        Arc::clone(&store),
        docker.clone(),
        Arc::clone(&catalog),
        observer,
    );
    let host = ProbeHost::new(
        docker.clone(),
        config.sidecar_image.clone(),
        config.root.join("fault-control.json"),
    );
    Ok(Probe {
        config: config.clone(),
        docker,
        store,
        catalog,
        applications,
        host,
        policy,
        fixtures: FixtureSet::new(),
        sources: Sources::empty(),
    })
}

/// 采集事实 + 判定：所有命令共用的收尾。
async fn finish(probe: &Probe, report: &mut Report) -> Result<()> {
    let bundle = collect_facts(probe, report).await?;
    evaluate_checks(probe, report, &bundle).await
}

/// 采集 Agent/源/容器/回调事实，并写入报告。
async fn collect_facts(probe: &Probe, report: &mut Report) -> Result<FactsBundle> {
    let containers = probe.docker.list_managed_containers().await?;
    let mut agents = Vec::new();
    for agent in &probe.fixtures.agents {
        agents.push(environment::collect_agent_facts(probe, &probe.fixtures, agent).await?);
    }
    let mut sources = Vec::new();
    for (role, id, path) in [
        (
            "linux",
            probe.sources.linux,
            probe.config.linux_source.clone(),
        ),
        (
            "private-fixture",
            probe.sources.private_fixture,
            probe.config.fixture_repo.clone(),
        ),
    ] {
        match id {
            Some(id) => sources.push(environment::collect_source_facts(probe, role, id).await?),
            None => sources.push(environment::absent_source_facts(role, &path)),
        }
    }
    let host = probe.host.evidence().await;
    report.fact("agents", &agents)?;
    report.fact("sources", &sources)?;
    report.fact("managed_containers", &containers)?;
    report.fact("host", &host)?;
    report.fact(
        "source_paths",
        json!({
            "linux": probe.config.linux_source,
            "private_fixture": probe.config.fixture_repo,
            "linux_alias": probe.config.linux_alias,
            "private_alias": probe.config.private_alias,
        }),
    )?;
    Ok(FactsBundle {
        agents,
        sources,
        containers,
        host,
    })
}

/// 用当前事实与本轮之前的历史证据判定全部验收点。
async fn evaluate_checks(probe: &Probe, report: &mut Report, bundle: &FactsBundle) -> Result<()> {
    let entries = evidence::read_log(&probe.config)?;
    let previous = evidence::last_with_facts(&entries);
    let scenarios = evidence::scenarios_seen(&entries);
    let commands = evidence::commands_seen(&entries);
    let input = CheckInput {
        facts: &bundle.agents,
        sources: &bundle.sources,
        host: &bundle.host,
        containers: &bundle.containers,
        previous,
        scenarios: &scenarios,
        commands: &commands,
    };
    checks::evaluate(probe, &input, report).await
}
