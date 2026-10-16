//! 证据：步骤判定、事实与 JSON 落盘（`root/evidence/runtime-<命令>-<时间戳>.json`）。
//!
//! 判定只有四种：`pass`（真实证据成立）、`fail`（证据表明行为错误）、`blocked`
//! （前置条件缺失，无法判定；非零退出，绝不当作通过）、`boundary`（已知接口缺口或
//! 未覆盖范围，不参与成败判定，单独列出）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{Result, error};
use crate::guard;
use crate::runtime_config::DriverConfig;

/// 单条判定的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Blocked,
    Boundary,
}

/// 一条判定。
#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub id: String,
    pub verdict: Verdict,
    pub detail: String,
}

/// 本次运行的证据。
#[derive(Debug, Serialize)]
pub struct Evidence {
    driver: &'static str,
    command: String,
    started_at: String,
    finished_at: Option<String>,
    paths: BTreeMap<String, String>,
    steps: Vec<Step>,
    /// 已知接口缺口与未覆盖范围（不参与成败判定）。
    limitations: Vec<String>,
    facts: BTreeMap<String, Value>,
    failures: usize,
    blocked: usize,
}

impl Evidence {
    /// 建立证据目录并记录固定路径事实。
    pub fn new(config: &DriverConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.evidence_dir).map_err(|cause| {
            error(
                "evidence_dir_unwritable",
                format!(
                    "无法创建证据目录 {}：{cause}",
                    config.evidence_dir.display()
                ),
            )
        })?;
        let mut paths = BTreeMap::new();
        for (role, path) in guard::path_facts(config) {
            paths.insert(role.to_string(), path.display().to_string());
        }
        let mut evidence = Self {
            driver: "shared-references-runtime",
            command: config.command.as_str().to_string(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            paths,
            steps: Vec::new(),
            limitations: Vec::new(),
            facts: BTreeMap::new(),
            failures: 0,
            blocked: 0,
        };
        for note in guard::boundary_notes(config) {
            evidence.limitation(note);
        }
        evidence.fact(
            "docker_host",
            json!(std::env::var("DOCKER_HOST").unwrap_or_default()),
        );
        Ok(evidence)
    }

    /// 真实证据成立。
    pub fn pass(&mut self, id: impl Into<String>, detail: impl Into<String>) {
        self.push(id, Verdict::Pass, detail);
    }

    /// 证据表明行为错误。
    pub fn fail(&mut self, id: impl Into<String>, detail: impl Into<String>) {
        self.failures += 1;
        self.push(id, Verdict::Fail, detail);
    }

    /// 前置条件缺失，无法判定；非零退出。
    pub fn blocked(&mut self, id: impl Into<String>, detail: impl Into<String>) {
        self.blocked += 1;
        self.push(id, Verdict::Blocked, detail);
    }

    /// 记录已知接口缺口或未覆盖范围（不参与判定）。
    pub fn boundary(&mut self, id: impl Into<String>, detail: impl Into<String>) {
        self.push(id, Verdict::Boundary, detail);
    }

    /// 记录一条局限说明。
    pub fn limitation(&mut self, detail: impl Into<String>) {
        self.limitations.push(detail.into());
    }

    /// 记录一条事实（任意 JSON）。
    pub fn fact(&mut self, key: impl Into<String>, value: Value) {
        self.facts.insert(key.into(), value);
    }

    /// 判定某个布尔断言。
    pub fn check(
        &mut self,
        id: &str,
        condition: bool,
        pass_detail: impl Into<String>,
        fail_detail: impl Into<String>,
    ) {
        if condition {
            self.pass(id, pass_detail);
        } else {
            self.fail(id, fail_detail);
        }
    }

    fn push(&mut self, id: impl Into<String>, verdict: Verdict, detail: impl Into<String>) {
        self.steps.push(Step {
            id: id.into(),
            verdict,
            detail: detail.into(),
        });
    }

    /// 本次运行是否全部通过（`blocked` 视为未通过）。
    pub fn succeeded(&self) -> bool {
        self.failures == 0 && self.blocked == 0
    }

    fn summary(&self) -> Value {
        json!({
            "driver": self.driver,
            "command": &self.command,
            "started_at": &self.started_at,
            "finished_at": &self.finished_at,
            "paths": &self.paths,
            "ok": self.succeeded(),
            "pass": self.steps.iter().filter(|step| step.verdict == Verdict::Pass).count(),
            "fail": self.failures,
            "blocked": self.blocked,
            "boundary": self.steps.iter().filter(|step| step.verdict == Verdict::Boundary).count(),
            "limitations": &self.limitations,
            "steps": &self.steps,
            "facts": &self.facts,
        })
    }

    /// 写完证据、打印摘要并返回退出码（有 fail/blocked 时为 1）。
    pub fn finish_and_write(mut self) -> ExitCode {
        self.finished_at = Some(Utc::now().to_rfc3339());
        let summary = self.summary();
        let path = self.output_path();
        let mut write_failure = None;
        if let Err(cause) = std::fs::write(
            &path,
            serde_json::to_vec_pretty(&summary)
                .unwrap_or_else(|_| summary.to_string().into_bytes()),
        ) {
            write_failure = Some(cause.to_string());
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).unwrap_or_else(|_| summary.to_string())
        );
        if let Some(failure) = write_failure {
            eprintln!("证据写入失败 {}：{failure}", path.display());
            return ExitCode::from(1);
        }
        eprintln!("证据已写入 {}", path.display());
        if self.succeeded() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }

    fn output_path(&self) -> PathBuf {
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let evidence_dir = self
            .paths
            .get("evidence_dir")
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        PathBuf::from(evidence_dir).join(format!("runtime-{}-{stamp}.json", self.command))
    }
}
