//! exec 通道用例：无匹配退出码、超时、模型可见截断、捕获硬上限。
//!
//! Turn terminal 不是 exec terminal。凡拿到 PL running receipt 的命令，先用真实 `wait`
//! 等任务终态，再用 `get_tool_task` 分页拼接 full opaque payload；失败文本、preview JSON
//! 与 full payload 三种视图不得混用。捕获路径只读取 receipt 后的 exact payload 记录。

use mai_protocol::AgentId;
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe::{self, Probe, ProbeCall};
use crate::runtime_config::DriverConfig;

use super::outputs;

#[path = "exec_limits/no_match.rs"]
mod no_match;

use super::task::{self, TaskTerminalFacts};
use no_match::assert_rg_no_match;

/// mai 命令后端的模型可见 stdout+stderr 总上限。
const MODEL_OUTPUT_BYTES_CAP: usize = 32 * 1024;

/// mai 命令后端的单命令捕获硬上限。
const CAPTURE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// 大输出用例的目标字节量（超过捕获硬上限，触发上限语义）。
const OVERSIZE_BYTES: u64 = 70 * 1024 * 1024;

/// `ExecFailure::OutputLimitExceeded` 的契约文本。
const OUTPUT_LIMIT_MESSAGE: &str =
    "command output exceeded the 67108864-byte per-command capture limit";

/// 执行 exec 上限用例。
pub async fn case(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    tag: &str,
    agent: AgentId,
    private_target: &str,
) -> Result<()> {
    assert_rg_no_match(config, host, evidence, tag, agent, private_target).await?;
    assert_timeout(config, host, evidence, tag, agent).await?;
    assert_truncation(config, host, evidence, tag, agent).await?;
    assert_capture_limit(config, host, evidence, tag, agent).await?;
    Ok(())
}

async fn assert_timeout(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    tag: &str,
    agent: AgentId,
) -> Result<()> {
    let marker = format!("channel-timeout-marker-{tag}");
    let probe = Probe::new(
        format!("channels-timeout-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": format!("echo {marker}; sleep 30"),
                "timeoutSeconds": 2,
            }),
        )],
    );
    let outcome = probe::run(host, agent, &probe, config.turn_timeout).await?;
    record_outcome(evidence, "exec.timeout", &outcome);
    let Some(call) = outcome.call(0) else {
        evidence.fail(
            "channels.exec.timeout_reported",
            "timeout probe 没有任何工具调用结论",
        );
        return Ok(());
    };
    let raw_output = outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id);
    let Some(task_id) = raw_output
        .as_deref()
        .and_then(|output| outputs::running_task_receipt(output, &call.expected_call_id))
    else {
        evidence.blocked(
            "channels.exec.timeout_reported",
            format!(
                "timeout exec 没有返回精确 running receipt，无法用真实 wait/get_tool_task 判定：{:?}",
                raw_output
            ),
        );
        return Ok(());
    };
    let facts = task::wait_and_query(config, host, agent, &task_id, "timeout", tag).await?;
    evidence.fact("exec.timeout.task", json!(&facts));
    let payload = facts.full_exec_payload();
    let state = payload.and_then(outputs::state_kind);
    let result = payload.and_then(outputs::final_result_kind);
    let failure = payload.and_then(outputs::final_failure_kind);
    let exit_code = payload.and_then(outputs::failure_exit_code);
    evidence.check(
        "channels.exec.timeout_reported",
        facts.status() == Some("failed")
            && state.as_deref() == Some("final")
            && result.as_deref() == Some("timedOut")
            && failure.is_none()
            && exit_code.is_none(),
        "真实 wait 到达 exec 终态，full payload 报告 timedOut 且没有退出码",
        format!(
            "timeout 不符合契约：status={:?} state={state:?} result={result:?} failure={failure:?} exit={exit_code:?} query_error={:?}",
            facts.status(),
            facts.query.parse_error
        ),
    );
    assert_no_process(
        host,
        evidence,
        agent,
        &marker,
        "channels.exec.timeout_no_leftover",
    )
    .await
}

async fn assert_truncation(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    tag: &str,
    agent: AgentId,
) -> Result<()> {
    let head = format!("CHANNEL-HEAD-{tag}");
    let tail = format!("CHANNEL-TAIL-{tag}");
    let precise = 200_000usize;
    let probe = Probe::new(
        format!("channels-truncate-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": format!(
                    "sleep 2; printf '%s\\n' '{head}'; head -c {precise} /dev/zero | tr '\\0' 'b'; printf '\\n%s\\n' '{tail}'"
                ),
                "timeoutSeconds": 60,
            }),
        )],
    );
    let outcome = probe::run(host, agent, &probe, config.turn_timeout).await?;
    record_outcome(evidence, "exec.truncate", &outcome);
    let Some(facts) =
        background_task(config, host, evidence, agent, &outcome, "truncate", tag).await?
    else {
        return Ok(());
    };
    let payload = facts.full_exec_payload();
    let Some(payload) = payload else {
        evidence.fail(
            "channels.exec.model_view_truncated",
            format!(
                "截断任务没有可解析 full payload：status={:?} query_error={:?}",
                facts.status(),
                facts.query.parse_error
            ),
        );
        return Ok(());
    };
    let Some(stdout) = outputs::output_stream(payload, "stdout") else {
        evidence.fail(
            "channels.exec.model_view_truncated",
            "full payload 缺少 stdout TruncatedOutput 对象",
        );
        return Ok(());
    };
    let stderr = outputs::output_stream(payload, "stderr");
    let stderr_bytes = stderr
        .as_ref()
        .map(|stream| stream.content.len())
        .unwrap_or(0);
    let combined_bytes = stdout.content.len() + stderr_bytes;
    let paths = outputs::exec_capture_paths(config, payload);
    evidence.fact(
        "exec.truncate.payload",
        json!({
            "success_exit_code": outputs::success_exit_code(payload),
            "stdout": &stdout,
            "stderr": &stderr,
            "combined_bytes": combined_bytes,
            "head_visible": stdout.content.contains(&head),
            "tail_visible": stdout.content.contains(&tail),
            "capture_paths": &paths,
        }),
    );
    evidence.check(
        "channels.exec.model_view_truncated",
        facts.status() == Some("succeeded")
            && outputs::success_exit_code(payload) == Some(0)
            && combined_bytes <= MODEL_OUTPUT_BYTES_CAP
            && stdout.content.contains(&head)
            && stdout.content.contains(&tail)
            && stdout.was_truncated
            && stdout.original_length > MODEL_OUTPUT_BYTES_CAP as u64,
        format!(
            "模型可见视图保留 head/tail 且合计 {} 字节；full payload 记录原始 stdout {} 字节",
            combined_bytes, stdout.original_length
        ),
        format!(
            "截断契约不符：status={:?} exit={:?} combined={combined_bytes} head={} tail={} truncated={} original={}",
            facts.status(),
            outputs::success_exit_code(payload),
            stdout.content.contains(&head),
            stdout.content.contains(&tail),
            stdout.was_truncated,
            stdout.original_length
        ),
    );
    let recorded_bytes = paths
        .capture_file
        .as_ref()
        .filter(|path| path.exists)
        .and_then(|path| path.bytes)
        .or_else(|| {
            paths
                .output_file
                .as_ref()
                .filter(|path| path.exists)
                .and_then(|path| path.bytes)
        });
    let path_available = recorded_bytes.is_some();
    if path_available {
        evidence.check(
            "channels.exec.capture_exceeds_model_cap",
            recorded_bytes.is_some_and(|bytes| bytes > MODEL_OUTPUT_BYTES_CAP as u64 * 4),
            format!(
                "真实记录的捕获文件超过模型可见上限：{} 字节",
                recorded_bytes.unwrap_or_default()
            ),
            format!(
                "真实记录的捕获文件不足以证明大输出：{} 字节",
                recorded_bytes.unwrap_or_default()
            ),
        );
    } else {
        evidence.blocked(
            "channels.exec.capture_exceeds_model_cap",
            "full payload 的 captureFile/outputFile 均未在宿主定位到真实文件，无法核对完整输出体积",
        );
    }
    Ok(())
}

async fn assert_capture_limit(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    tag: &str,
    agent: AgentId,
) -> Result<()> {
    let marker = format!("CHANNEL-OVERSIZE-{tag}");
    let probe = Probe::new(
        format!("channels-oversize-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": format!(
                    "head -c {OVERSIZE_BYTES} /dev/zero | tr '\\0' 'c'; printf '\\n%s\\n' '{marker}'"
                ),
                "timeoutSeconds": 120,
            }),
        )],
    );
    let outcome = probe::run(host, agent, &probe, config.turn_timeout).await?;
    record_outcome(evidence, "exec.oversize", &outcome);
    let Some(facts) =
        background_task(config, host, evidence, agent, &outcome, "oversize", tag).await?
    else {
        return Ok(());
    };
    let payload = facts.full_exec_payload();
    let Some(payload) = payload else {
        evidence.fail(
            "channels.exec.capture_limit_enforced",
            format!(
                "超量任务没有可解析 full payload：status={:?} query_error={:?}",
                facts.status(),
                facts.query.parse_error
            ),
        );
        return Ok(());
    };
    let state = outputs::state_kind(payload);
    let result = outputs::final_result_kind(payload);
    let failure = outputs::final_failure_kind(payload);
    let message = outputs::failure_message(payload, "output");
    let stdout = outputs::output_stream(payload, "stdout");
    let stderr = outputs::output_stream(payload, "stderr");
    let combined_bytes = stdout
        .as_ref()
        .map(|stream| stream.content.len())
        .unwrap_or(0)
        + stderr
            .as_ref()
            .map(|stream| stream.content.len())
            .unwrap_or(0);
    let archived = payload
        .get("archived")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let paths = outputs::exec_capture_paths(config, payload);
    evidence.fact(
        "exec.oversize.payload",
        json!({
            "state_kind": state,
            "result_kind": result,
            "failure_kind": failure,
            "message": message,
            "stdout": &stdout,
            "stderr": &stderr,
            "combined_bytes": combined_bytes,
            "archived": archived,
            "capture_paths": &paths,
        }),
    );
    let exact_message = message
        .as_deref()
        .is_some_and(|message| message.contains(OUTPUT_LIMIT_MESSAGE));
    let artifact_recorded = !paths.resource.is_null()
        || paths
            .output_artifacts
            .as_array()
            .is_some_and(|items| !items.is_empty())
        || paths.capture_file.as_ref().is_some_and(|path| path.exists)
        || paths.output_file.as_ref().is_some_and(|path| path.exists);
    evidence.check(
        "channels.exec.capture_limit_enforced",
        facts.status() == Some("failed")
            && state.as_deref() == Some("final")
            && result.as_deref() == Some("failed")
            && failure.as_deref() == Some("output")
            && exact_message
            && combined_bytes <= MODEL_OUTPUT_BYTES_CAP
            && artifact_recorded,
        "超量输出按 output failure 精确终止、归档，且模型视图仍有界",
        format!(
            "超量输出契约不符：status={:?} state={state:?} result={result:?} failure={failure:?} exact_message={exact_message} view_bytes={combined_bytes} archived={archived} artifact_recorded={artifact_recorded}",
            facts.status()
        ),
    );
    let recorded_bytes = paths
        .capture_file
        .as_ref()
        .filter(|path| path.exists)
        .and_then(|path| path.bytes)
        .or_else(|| {
            paths
                .output_file
                .as_ref()
                .filter(|path| path.exists)
                .and_then(|path| path.bytes)
        });
    if let Some(bytes) = recorded_bytes {
        evidence.check(
            "channels.exec.capture_bounded_by_limit",
            bytes > 8 * 1024 * 1024 && bytes <= CAPTURE_LIMIT_BYTES,
            format!("真实捕获文件 {bytes} 字节，证明超量输出已被硬上限有界终止"),
            format!("真实捕获文件 {bytes} 字节，不符合 64 MiB 捕获硬限语义"),
        );
    } else {
        evidence.blocked(
            "channels.exec.capture_bounded_by_limit",
            "full payload 的 captureFile/outputFile 均未在宿主定位到真实文件，无法验证磁盘占用上限",
        );
    }
    // 合并文件与每个流文件都是同一命令的落盘成本，不能只量一份。
    if let Some(path) = paths
        .capture_file
        .as_ref()
        .and_then(|path| path.resolved.as_deref())
    {
        let call_directory = std::path::Path::new(path)
            .parent()
            .and_then(std::path::Path::parent);
        if let Some(directory) = call_directory {
            let mut files = Vec::new();
            let mut total = 0_u64;
            for child in std::fs::read_dir(directory)
                .map_err(|cause| crate::error::error("capture_storage_io", cause.to_string()))?
            {
                let child = child.map_err(|cause| {
                    crate::error::error("capture_storage_io", cause.to_string())
                })?;
                if child
                    .file_type()
                    .map_err(|cause| crate::error::error("capture_storage_io", cause.to_string()))?
                    .is_dir()
                {
                    for file in std::fs::read_dir(child.path()).map_err(|cause| {
                        crate::error::error("capture_storage_io", cause.to_string())
                    })? {
                        let file = file.map_err(|cause| {
                            crate::error::error("capture_storage_io", cause.to_string())
                        })?;
                        if file
                            .file_type()
                            .map_err(|cause| {
                                crate::error::error("capture_storage_io", cause.to_string())
                            })?
                            .is_file()
                        {
                            let bytes = file
                                .metadata()
                                .map_err(|cause| {
                                    crate::error::error("capture_storage_io", cause.to_string())
                                })?
                                .len();
                            total += bytes;
                            files.push(json!({"path":file.path(),"bytes":bytes}));
                        }
                    }
                }
            }
            evidence.fact(
                "exec.oversize.total_capture_storage",
                json!({"files":files,"bytes":total,"limit":CAPTURE_LIMIT_BYTES}),
            );
            evidence.check(
                "channels.exec.total_capture_storage_bounded",
                total > 8 * 1024 * 1024 && total <= CAPTURE_LIMIT_BYTES,
                format!("全部实际捕获文件共 {total} 字节，包含头部和 framing，未超过 64 MiB"),
                format!("全部实际捕获文件共 {total} 字节，违反硬上限"),
            );
        }
    }
    assert_no_process(
        host,
        evidence,
        agent,
        &marker,
        "channels.exec.oversize_no_leftover",
    )
    .await
}

async fn background_task(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    agent: AgentId,
    outcome: &probe::ProbeOutcome,
    scope: &str,
    tag: &str,
) -> Result<Option<TaskTerminalFacts>> {
    let Some(call) = outcome.call(0) else {
        evidence.fail(
            format!("channels.exec.{scope}_task"),
            format!("probe {} 没有任何工具结论", outcome.probe_id),
        );
        return Ok(None);
    };
    let raw_output = outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id);
    let Some(task_id) = raw_output
        .as_deref()
        .and_then(|output| outputs::running_task_receipt(output, &call.expected_call_id))
    else {
        evidence.blocked(
            format!("channels.exec.{scope}_task"),
            format!(
                "exec 没有返回精确 running receipt，无法用真实任务控制面等待终态：{:?}",
                raw_output
            ),
        );
        return Ok(None);
    };
    let facts = task::wait_and_query(config, host, agent, &task_id, scope, tag).await?;
    evidence.fact(format!("exec.{scope}.task"), json!(&facts));
    Ok(Some(facts))
}

/// 记录一次 probe 的 Turn 终态；这不代表其中的后台 exec task 已终态。
fn record_outcome(evidence: &mut Evidence, prefix: &str, outcome: &probe::ProbeOutcome) {
    evidence.fact(prefix, json!(outcome));
    evidence.check(
        &format!("{prefix}.turn_terminal"),
        outcome.terminal,
        format!(
            "probe {} 进入 Turn 终态 {}",
            outcome.probe_id, outcome.turn_state
        ),
        format!(
            "probe {} 没有进入 Turn 终态（state={} thread={}）",
            outcome.probe_id, outcome.turn_state, outcome.thread_status
        ),
    );
}

/// 断言容器里没有该标记的残留进程。
async fn assert_no_process(
    host: &RuntimeHost,
    evidence: &mut Evidence,
    agent: AgentId,
    marker: &str,
    id: &str,
) -> Result<()> {
    let Some(container_id) = host.container_id(agent).await? else {
        evidence.blocked(id, "Agent 没有已发布的容器，无法扫描残留进程");
        return Ok(());
    };
    let check = outputs::container_processes(host, &container_id, marker).await?;
    evidence.fact(
        format!("{id}.scan"),
        json!({ "container_id": container_id, "raw": check.raw, "matching": check.matching }),
    );
    if !check.supported {
        evidence.blocked(
            id,
            format!("容器内进程扫描不可用，无法判定残留：{}", check.raw),
        );
        return Ok(());
    }
    evidence.check(
        id,
        check.matching.is_empty(),
        format!("容器内没有标记 {marker} 的残留进程"),
        format!("容器内仍有残留进程：{:?}", check.matching),
    );
    Ok(())
}
