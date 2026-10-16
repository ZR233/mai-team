//! 证据日志：每轮运行的 JSON 写一行追加到 `<验收根>/evidence/runs.jsonl`。
//!
//! 驱动从不删除或重写历史证据：跨进程重启的身份比较（容器 ID、镜像 ID、卷名、
//! 源内容摘要）和“哪些验收点曾经被真实观察到”的覆盖边界都靠这份只增日志。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::config::ProbeConfig;
use crate::error::{ProbeError, Result};

/// 逐行读取证据日志；损坏行只计数并跳过，不影响已成功的运行记录。
pub fn read_log(config: &ProbeConfig) -> Result<Vec<Value>> {
    read_log_at(&config.evidence_log)
}

/// 逐行读取指定证据日志。
pub fn read_log_at(path: &Path) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ProbeError::new(
                "evidence_unreadable",
                format!("无法读取证据日志 {}：{error}", path.display()),
            ));
        }
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            entries.push(value);
        }
    }
    Ok(entries)
}

/// 追加一行证据到指定目录/文件。
pub fn append_at(directory: &Path, path: &Path, value: &Value) -> Result<()> {
    crate::guard::ensure_directory(directory)?;
    let line = serde_json::to_string(value)?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            ProbeError::new(
                "evidence_append_failed",
                format!("无法写入证据日志 {}：{error}", path.display()),
            )
        })?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// 最近一轮已经记录事实的条目（用于跨轮身份比较）。
pub fn last_with_facts(entries: &[Value]) -> Option<&Value> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.get("facts").is_some_and(|facts| !facts.is_null()))
}

/// 把一轮报告里的 `facts.agents[]` 按 agent key 建索引。
pub fn agents_by_key(entry: &Value) -> BTreeMap<String, Value> {
    let mut indexed = BTreeMap::new();
    let Some(agents) = entry
        .get("facts")
        .and_then(|facts| facts.get("agents"))
        .and_then(Value::as_array)
    else {
        return indexed;
    };
    for agent in agents {
        if let Some(key) = agent.get("agent_key").and_then(Value::as_str) {
            indexed.insert(key.to_string(), agent.clone());
        }
    }
    indexed
}

/// 已经真实执行过的场景名（`apply` 的 scenario）。
pub fn scenarios_seen(entries: &[Value]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("scenario")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect()
}

/// 已经真实执行过的命令名。
pub fn commands_seen(entries: &[Value]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| entry.get("command").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

/// 某个验收点在各轮里的状态计数（用于覆盖边界）。
#[derive(Debug, Default, Clone)]
pub struct CheckHistory {
    pub states: BTreeMap<String, u64>,
    pub ever_passed: bool,
    pub last_reason: Option<String>,
}

/// 逐轮汇总验收点状态；顺序按日志出现顺序，后者覆盖 `last_reason`。
pub fn check_history(
    entries: &[Value],
    current: &[crate::report::Check],
) -> BTreeMap<String, CheckHistory> {
    let mut history: BTreeMap<String, CheckHistory> = BTreeMap::new();
    let record = |history: &mut BTreeMap<String, CheckHistory>,
                  id: &str,
                  state: &str,
                  reason: Option<&str>| {
        let entry = history.entry(id.to_string()).or_default();
        *entry.states.entry(state.to_string()).or_insert(0) += 1;
        if state == "pass" {
            entry.ever_passed = true;
        }
        if let Some(reason) = reason {
            entry.last_reason = Some(reason.to_string());
        }
    };
    for entry in entries {
        let Some(checks) = entry.get("checks").and_then(Value::as_array) else {
            continue;
        };
        for check in checks {
            let id = check.get("id").and_then(Value::as_str).unwrap_or_default();
            let state = check
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let reason = check.get("reason").and_then(Value::as_str);
            record(&mut history, id, state, reason);
        }
    }
    for check in current {
        record(
            &mut history,
            &check.id,
            check.state.as_str(),
            Some(&check.reason),
        );
    }
    history
}
