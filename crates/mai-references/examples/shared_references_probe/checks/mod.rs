//! 验收点判定：把数据库事实、Docker 事实、回调证据与上一轮证据放在一起核对。
//!
//! 判定原则：
//! - 只使用真实观察；前置不满足时给 `blocked`，本轮没跑到给 `not_run`，绝不写“通过”；
//! - 写保护用真实容器尝试（只读打开原文件 / touch / append / rm / mv / chmod / ln），
//!   每次尝试都配宿主内容摘要前后一致；除 chmod 外都不写被保护目标本身；
//! - 换代保留性用“三类标记 + 实际卷名 + 生效镜像 ID”与上一轮证据逐项比较。
//!
//! 子模块按职责拆分，判定口径、验收点 ID、报表字段与退出码语义都保持不变：
//! - `sources`：源登记状态、源根身份与内容摘要；
//! - `state`：绑定与实际容器一致性、owner 唯一性、目标收敛；
//! - `mounts`：挂载集合与可见性（T14/T16/T17/T21）；
//! - `readonly`：只读写保护与提权探测（T22/T23）；
//! - `sidecar`：MCP sidecar 契约（T26）；
//! - `preservation`：换代保留性（T27）；
//! - `recovery`：崩溃恢复与失败态（T31/T32）；
//! - `attempts`：容器内尝试原语（供 readonly/sidecar 复用）。

mod attempts;
mod mounts;
mod preservation;
mod readonly;
mod recovery;
mod sidecar;
mod sources;
mod state;

use std::collections::{BTreeMap, BTreeSet};

use mai_protocol::{AppliedReference, ReferenceApplyPhase};
use serde_json::{Value, json};

use crate::environment::{
    self, AgentFacts, MARKER_ANON, MARKER_USR_LOCAL, MARKER_WORKSPACE, Probe, SourceFacts,
    marker_content, reference_targets, shell_quote,
};
use crate::error::Result;
use crate::host::HostEvidence;
use crate::report::{Check, Report};

use mounts::{
    clear_removes_mounts, metadata_only_keeps_container, other_agents_cannot_read_private,
    same_source_same_mount,
};
use preservation::preservation;
use readonly::{privilege_probes, readonly_write_attempts};
use recovery::{crash_recovery, failure_keeps_old_container};
use sidecar::sidecar_contract;
use sources::{host_digest_unchanged, source_content_digest, source_root_identity, sources_ready};
use state::{binding_matches_container, converged_to_latest_target, single_owner};

/// 判定输入。
pub struct CheckInput<'a> {
    pub facts: &'a [AgentFacts],
    pub sources: &'a [SourceFacts],
    pub host: &'a HostEvidence,
    /// 独立 daemon 上的受管容器清单（用于 owner 唯一性与候选保留判定）。
    pub containers: &'a [mai_docker::ManagedContainer],
    /// 上一轮（含事实的）证据；没有历史时为 `None`。
    pub previous: Option<&'a Value>,
    pub scenarios: &'a BTreeSet<String>,
    pub commands: &'a BTreeSet<String>,
}

/// 一次验收点判定的累加器。
#[derive(Debug)]
pub struct Outcome {
    id: String,
    passes: Vec<String>,
    failures: Vec<String>,
    blocked: Vec<String>,
}

impl Outcome {
    /// 新建判定。
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            passes: Vec::new(),
            failures: Vec::new(),
            blocked: Vec::new(),
        }
    }

    /// 记录一个通过证据。
    pub fn pass(&mut self, detail: impl Into<String>) {
        self.passes.push(detail.into());
    }

    /// 记录一个失败证据。
    pub fn fail(&mut self, detail: impl Into<String>) {
        self.failures.push(detail.into());
    }

    /// 记录一个无法判定的原因。
    pub fn block(&mut self, detail: impl Into<String>) {
        self.blocked.push(detail.into());
    }

    /// 汇总成验收点。
    pub fn finish(self, ok_reason: &str) -> Check {
        let passes = self.passes;
        let failures = self.failures;
        let blocked = self.blocked;
        let id = self.id;
        let failure_reason = failures.join("；");
        let blocked_reason = blocked.join("；");
        let has_failure = !failures.is_empty();
        let has_blocked = !blocked.is_empty();
        let has_pass = !passes.is_empty();
        let evidence = json!({
            "pass_count": passes.len(),
            "fail_count": failures.len(),
            "blocked_count": blocked.len(),
            "pass": passes,
            "fail": failures,
            "blocked": blocked,
        });
        if has_failure {
            return Check::fail(id, failure_reason).with_evidence(evidence);
        }
        if has_blocked {
            return Check::blocked(id, blocked_reason).with_evidence(evidence);
        }
        if !has_pass {
            return Check::not_run(id, "本轮没有可判定的事实");
        }
        Check::pass(id, ok_reason).with_evidence(evidence)
    }
}

/// 执行全部状态类验收点。
pub async fn evaluate(probe: &Probe, input: &CheckInput<'_>, report: &mut Report) -> Result<()> {
    report.check(sources_ready(input));
    report.check(source_root_identity(input));
    report.check(source_content_digest(probe, input).await?);
    report.check(host_digest_unchanged(input));
    report.check(binding_matches_container(input));
    report.check(single_owner(input));
    report.check(converged_to_latest_target(input));
    report.check(same_source_same_mount(probe, input));
    report.check(clear_removes_mounts(input));
    report.check(metadata_only_keeps_container(input));
    report.check(other_agents_cannot_read_private(probe, input).await?);
    report.check(readonly_write_attempts(probe, input).await?);
    report.check(privilege_probes(probe, input).await?);
    report.check(sidecar_contract(probe, input).await?);
    report.check(preservation(probe, input).await?);
    report.check(crash_recovery(probe, input));
    report.check(failure_keeps_old_container(probe, input).await?);
    Ok(())
}
