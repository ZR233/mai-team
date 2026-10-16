//! 通道证据读取：provider 回填、exec 快照/full payload 解析与容器进程事实。
//!
//! 这里的读取边界刻意区分三种视图：
//! - provider `function_call_output` 是模型实际看到的文本；后台任务的 running receipt 是纯文本；
//! - exec preview JSON 是模型可见的 `{state, stdout, stderr, archived}` 投影；
//! - `get_tool_task.result.content` 拼页后是完整 opaque payload，捕获路径只能来自该真实记录。

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::error::{Result, error};
use crate::host::RuntimeHost;
use crate::probe;
use crate::runtime_config::DriverConfig;

/// provider 请求里出现过的工具输出，按 call_id 取**最后一次**（历史回放里最后一次最完整）。
pub fn tool_outputs(requests: &[Value]) -> std::collections::BTreeMap<String, String> {
    let mut outputs = std::collections::BTreeMap::new();
    for request in requests {
        let Some(items) = request.get("input").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
                continue;
            }
            let (Some(call_id), Some(output)) = (
                item.get("call_id").and_then(Value::as_str),
                item.get("output").and_then(Value::as_str),
            ) else {
                continue;
            };
            outputs.insert(call_id.to_string(), output.to_string());
        }
    }
    outputs
}

/// 取出本轮 provider 请求里某个 call_id 的工具输出。
pub fn tool_output_for(requests: &[Value], call_id: &str) -> Option<String> {
    tool_outputs(requests).remove(call_id)
}

/// 只解析 JSON 形状的 exec/任务控制视图；错误文本会保持为 `None`，调用方必须精确判定。
pub fn exec_preview(output: &str) -> Option<Value> {
    // exec 的模型视图可能附加单独一行 Resource reference；首个 JSON 值仍是原始视图。
    let mut values = serde_json::Deserializer::from_str(output).into_iter::<Value>();
    let value = values.next()?.ok()?;
    let trailer = output[values.byte_offset()..].trim();
    (trailer.is_empty()
        || trailer
            .strip_prefix("Resource reference: ")
            .is_some_and(|reference| serde_json::from_str::<Value>(reference).is_ok()))
    .then_some(value)
}

/// 解析 PL 通用后台任务回执，并要求任务身份与真实 call_id 一一对应。
///
/// 回执必须精确为：
/// `Task task:<call_id> is running. Its result will be delivered when execution finishes.`
/// 不从任意错误文本里提取 `task` 字符串，也不复刻 exec 内部的 `safe_identity`。
pub fn running_task_receipt(output: &str, expected_call_id: &str) -> Option<String> {
    let expected_task_id = format!("task:{expected_call_id}");
    let expected_receipt = format!(
        "Task {expected_task_id} is running. Its result will be delivered when execution finishes."
    );
    (output == expected_receipt).then_some(expected_task_id)
}

/// 快照里的进程生命周期类别（`running` / `final` / …）。
pub fn state_kind(preview: &Value) -> Option<String> {
    preview
        .get("state")
        .and_then(|state| state.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 终态结果类别（`succeeded` / `failed` / `timedOut` / `cancelled`）。
pub fn final_result_kind(preview: &Value) -> Option<String> {
    preview
        .get("state")
        .and_then(|state| state.get("data"))
        .and_then(|data| data.get("result"))
        .and_then(|result| result.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 终态失败子类别（`exited` / `output` / `wait`）。
pub fn final_failure_kind(preview: &Value) -> Option<String> {
    preview
        .get("state")?
        .get("data")?
        .get("result")?
        .get("data")?
        .get("failure")?
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 成功终态的进程退出码；成功 JSON 内部字段是 `exit_code`，不是 `exitCode`。
pub fn success_exit_code(preview: &Value) -> Option<i64> {
    let result = preview.get("state")?.get("data")?.get("result")?;
    if result.get("kind").and_then(Value::as_str) != Some("succeeded") {
        return None;
    }
    result.get("data")?.get("exit_code").and_then(Value::as_i64)
}

/// 失败终态携带的退出码（`exited` 子类别）。
pub fn failure_exit_code(preview: &Value) -> Option<i64> {
    let failure = preview
        .get("state")?
        .get("data")?
        .get("result")?
        .get("data")?
        .get("failure")?;
    if failure.get("kind").and_then(Value::as_str) != Some("exited") {
        return None;
    }
    failure
        .get("data")?
        .get("exit_code")
        .and_then(Value::as_i64)
}

/// 指定失败子类别中的消息；用于 output capture-limit 的精确契约文本。
pub fn failure_message(preview: &Value, expected_kind: &str) -> Option<String> {
    let failure = preview
        .get("state")?
        .get("data")?
        .get("result")?
        .get("data")?
        .get("failure")?;
    if failure.get("kind").and_then(Value::as_str) != Some(expected_kind) {
        return None;
    }
    failure
        .get("data")?
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 模型 preview 里的 stdout 字符串（不是 full payload 的 `TruncatedOutput` 对象）。
pub fn stdout_of(preview: &Value) -> String {
    preview
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// full opaque payload 中的有界输出流。
#[derive(Debug, Clone, Serialize)]
pub struct OutputStream {
    pub content: String,
    pub was_truncated: bool,
    pub original_length: u64,
}

/// 读取 full payload 的 stdout/stderr；preview 字符串视图会因对象形状不符而返回 `None`。
pub fn output_stream(payload: &Value, stream: &str) -> Option<OutputStream> {
    let value = payload.get(stream)?;
    Some(OutputStream {
        content: value.get("content")?.as_str()?.to_string(),
        was_truncated: value
            .get("wasTruncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        original_length: value.get("originalLength").and_then(Value::as_u64)?,
    })
}

/// 本轮 provider 请求集合。
pub fn requests(config: &DriverConfig) -> Vec<Value> {
    probe::provider_requests(&config.provider_evidence_dir)
}

/// 证据详情的第一行，超长时做字符级截断，避免失败文本淹没报告。
pub fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 240 {
        format!("{}…", line.chars().take(240).collect::<String>())
    } else {
        line.to_string()
    }
}

/// 真实记录路径的定位事实；绝不从 call_id 或 SHA 重算路径。
#[derive(Debug, Clone, Serialize)]
pub struct RecordedPathFact {
    pub value: String,
    pub resolved: Option<String>,
    pub exists: bool,
    pub bytes: Option<u64>,
    pub candidates: Vec<String>,
}

/// full payload 中 exec 输出/捕获路径与归档资源的集合。
#[derive(Debug, Clone, Serialize)]
pub struct ExecCapturePaths {
    pub capture_file: Option<RecordedPathFact>,
    pub output_file: Option<RecordedPathFact>,
    pub output_artifacts: Value,
    pub resource: Value,
}

/// 从 full payload 读取 exact 路径，并在相对路径是 `target/pure/...` 时动态尝试已知根。
pub fn exec_capture_paths(config: &DriverConfig, payload: &Value) -> ExecCapturePaths {
    ExecCapturePaths {
        capture_file: optional_path_fact(config, payload.get("captureFile")),
        output_file: optional_path_fact(config, payload.get("outputFile")),
        output_artifacts: payload
            .get("outputArtifacts")
            .cloned()
            .unwrap_or(Value::Null),
        resource: payload.get("resource").cloned().unwrap_or(Value::Null),
    }
}

fn optional_path_fact(config: &DriverConfig, value: Option<&Value>) -> Option<RecordedPathFact> {
    let value = value.and_then(Value::as_str)?;
    Some(recorded_path_fact(config, value))
}

fn recorded_path_fact(config: &DriverConfig, value: &str) -> RecordedPathFact {
    let recorded = Path::new(value);
    let candidates = if recorded.is_absolute() {
        vec![recorded.to_path_buf()]
    } else {
        candidate_roots(config)
            .into_iter()
            .map(|root| root.join(recorded))
            .collect()
    };
    let resolved = candidates.iter().find(|candidate| candidate.is_file());
    RecordedPathFact {
        value: value.to_string(),
        resolved: resolved
            .as_ref()
            .map(|path| path.to_string_lossy())
            .map(String::from),
        exists: resolved.is_some(),
        bytes: resolved
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len()),
        candidates: candidates
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
    }
}

fn candidate_roots(config: &DriverConfig) -> Vec<PathBuf> {
    let mut roots = vec![config.root.clone(), config.fixture_repo.clone()];
    if let Ok(current) = std::env::current_dir() {
        roots.push(current);
    }
    let mut expanded = Vec::new();
    for root in roots {
        for ancestor in root.ancestors() {
            if !expanded.contains(&ancestor.to_path_buf()) {
                expanded.push(ancestor.to_path_buf());
            }
        }
    }
    expanded
}

/// 一条真实容器进程记录。
#[derive(Debug, Clone, Serialize)]
pub struct ContainerProcess {
    pub pid: u32,
    pub process_group: Option<u32>,
    pub stat: String,
    pub command_line: String,
}

/// 一次容器内进程扫描的结果。
#[derive(Debug, Clone)]
pub struct ProcessCheck {
    /// 扫描命令是否可用（`false` 表示无法判定，调用方必须记 blocked）。
    pub supported: bool,
    /// cmdline 精确包含完整标记的目标进程。
    pub matching: Vec<ContainerProcess>,
    /// 原始输出，供人核对。
    pub raw: String,
}

/// 在容器内按标记扫描 `/proc/*/cmdline`。
///
/// 标记在扫描脚本里拆成两段并在 shell 运行时拼接，且不用 `grep`，避免扫描 shell 或其子进程
/// 因 argv 自包含完整标记而形成伪残留。`stat` 中的 PGID 按 comm 之后真实字段解析。
pub async fn container_processes(
    host: &RuntimeHost,
    container_id: &str,
    marker: &str,
) -> Result<ProcessCheck> {
    let split = marker.len() / 2;
    let (left, right) = marker.split_at(split);
    let command = format!(
        "h='{left}'; t='{right}'; for p in /proc/[0-9]*; do \
            [ \"$p\" = \"/proc/$$\" ] && continue; \
            [ -r \"$p/cmdline\" ] || continue; \
            line=$(tr '\\0' ' ' < \"$p/cmdline\") || continue; \
            case \"$line\" in \
                *\"$h$t\"*) \
                    stat=$(cat \"$p/stat\") || continue; \
                    printf '%s\\t%s\\t%s\\n' \"${{p#/proc/}}\" \"$stat\" \"$line\" ;; \
            esac; \
        done"
    );
    let output = host
        .docker
        .exec_shell(container_id, &command, Some("/"), Some(20))
        .await
        .map_err(|cause| {
            error(
                "container_process_scan_failed",
                format!("容器 {container_id} 内的进程扫描失败：{cause}"),
            )
        })?;
    let matching = output
        .stdout
        .lines()
        .filter_map(parse_container_process)
        .filter(|process| process.command_line.contains(marker))
        .collect::<Vec<_>>();
    Ok(ProcessCheck {
        supported: output.status == 0,
        matching,
        raw: format!(
            "status={} stdout={} stderr={}",
            output.status,
            output.stdout.trim(),
            output.stderr.trim()
        ),
    })
}

fn parse_container_process(line: &str) -> Option<ContainerProcess> {
    let mut fields = line.splitn(3, '\t');
    let pid = fields.next()?.trim().parse::<u32>().ok()?;
    let stat = fields.next()?.trim().to_string();
    let command_line = fields.next()?.trim().to_string();
    Some(ContainerProcess {
        pid,
        process_group: process_group(&stat),
        stat,
        command_line,
    })
}

fn process_group(stat: &str) -> Option<u32> {
    let after_comm = stat.rsplit_once(')')?.1.trim_start();
    let mut fields = after_comm.split_whitespace();
    let _state = fields.next()?;
    let _parent = fields.next()?;
    fields.next()?.parse::<u32>().ok()
}

/// 容器是否仍在运行（`inspect` 事实）。
pub async fn container_running(host: &RuntimeHost, container_id: &str) -> Result<bool> {
    let running = host
        .docker
        .list_managed_containers()
        .await
        .map_err(|cause| error("docker_list_failed", format!("列出容器失败：{cause}")))?
        .into_iter()
        .any(|container| container.id == container_id && container.state == "running");
    Ok(running)
}
