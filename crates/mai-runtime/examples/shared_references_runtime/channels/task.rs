//! exec Task 的真实控制面：`wait` 与分页 `get_tool_task`。
//!
//! Turn terminal 只代表模型轮次结束；后台 exec 的终态必须通过真实注册的任务工具读取。
//! 本模块不猜测任务身份，也不把 wait 工具自身成功误当作 exec 成功。

use serde::Serialize;
use serde_json::{Value, json};

use crate::error::Result;
use crate::host::RuntimeHost;
use crate::probe::{self, Probe, ProbeCall};
use crate::runtime_config::DriverConfig;

use super::outputs;

/// PL 允许的单次 wait 上限。
const WAIT_TIMEOUT_MS: u64 = 300_000;

/// full payload 拼页保护；exec opaque payload 不应接近这个页数。
const MAX_RESULT_PAGES: usize = 128;

/// 一次 `wait` 调用的可判定事实。
#[derive(Debug, Clone, Serialize)]
pub struct TaskWaitFacts {
    pub probe_id: String,
    pub turn_state: String,
    pub observed: bool,
    pub invocation_success: Option<bool>,
    pub raw_output: Option<String>,
    pub target: Option<Value>,
    pub status: Option<String>,
    pub timed_out: Option<bool>,
    pub task_id_matches: bool,
    pub tool_is_exec: bool,
}

impl TaskWaitFacts {
    /// wait 是否明确观察到目标任务已进入 exec 终态。
    pub fn terminal_exec_task(&self) -> bool {
        self.observed
            && self.invocation_success == Some(true)
            && self.task_id_matches
            && self.tool_is_exec
            && self.timed_out == Some(false)
            && self.status.as_deref().is_some_and(is_terminal_task_status)
    }
}

/// 一页 `get_tool_task` 的读取事实。
#[derive(Debug, Clone, Serialize)]
struct TaskPageFacts {
    probe_id: String,
    turn_state: String,
    observed: bool,
    invocation_success: Option<bool>,
    raw_output: Option<String>,
    task_id: Option<String>,
    status: Option<String>,
    cursor: u64,
    next_cursor: Option<u64>,
    total_bytes: Option<u64>,
    content_bytes: usize,
    format: Option<String>,
    version: Option<i64>,
    parse_error: Option<String>,
}

/// `get_tool_task` 分页拼接结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct TaskQueryFacts {
    pages: Vec<TaskPageFacts>,
    content_bytes: usize,
    total_bytes: Option<u64>,
    terminal_status: Option<String>,
    pub payload: Option<Value>,
    pub parse_error: Option<String>,
}

/// wait + query 的完整任务终态事实。
#[derive(Debug, Clone, Serialize)]
pub struct TaskTerminalFacts {
    pub task_id: String,
    pub wait: TaskWaitFacts,
    pub query: TaskQueryFacts,
}

impl TaskTerminalFacts {
    /// 是否拿到了终态任务的完整 exec payload。
    pub fn full_exec_payload(&self) -> Option<&Value> {
        let payload = self.query.payload.as_ref()?;
        (self.wait.terminal_exec_task()
            && self.query.terminal_status.as_deref() == self.wait.status.as_deref()
            && self.query.parse_error.is_none())
        .then_some(payload)
    }

    /// wait/target 报告的任务状态。
    pub fn status(&self) -> Option<&str> {
        self.wait.status.as_deref()
    }
}

/// 判断 PL task view 的状态是否为终态。
pub fn is_terminal_task_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "interrupted")
}

/// 调用一次真实 `wait` 工具；`timeout_ms` 由调用方按场景显式指定。
pub async fn wait_for_task(
    config: &DriverConfig,
    host: &RuntimeHost,
    agent: mai_protocol::AgentId,
    task_id: &str,
    scope: &str,
    tag: &str,
    timeout_ms: u64,
) -> Result<TaskWaitFacts> {
    let probe_id = format!("channels-wait-{scope}-{tag}");
    let probe = Probe::new(
        probe_id.clone(),
        vec![ProbeCall::new(
            "wait",
            json!({ "taskIds": [task_id], "timeoutMs": timeout_ms }),
        )],
    );
    let outcome = probe::run(host, agent, &probe, config.turn_timeout).await?;
    let call = outcome.call(0);
    let raw_output = call.and_then(|call| {
        outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id)
    });
    let view = raw_output.as_deref().and_then(outputs::exec_preview);
    let target = view
        .as_ref()
        .and_then(|view| view.get("tasks"))
        .and_then(Value::as_array)
        .and_then(|tasks| {
            tasks
                .iter()
                .find(|task| task.get("taskId").and_then(Value::as_str) == Some(task_id))
                .cloned()
        });
    let task_id_matches = target.is_some();
    let status = target
        .as_ref()
        .and_then(|target| target.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let tool_is_exec = target
        .as_ref()
        .and_then(|target| target.get("toolId"))
        .and_then(Value::as_str)
        .map(|tool| tool == "exec")
        .unwrap_or(false);
    Ok(TaskWaitFacts {
        probe_id,
        turn_state: outcome.turn_state.clone(),
        observed: call.is_some_and(|call| call.observed),
        invocation_success: call.map(|call| call.success).unwrap_or(None),
        raw_output,
        target,
        status,
        timed_out: view
            .as_ref()
            .and_then(|view| view.get("timedOut"))
            .and_then(Value::as_bool),
        task_id_matches,
        tool_is_exec,
    })
}

/// 等待 exec task 到终态，再用 `get_tool_task` 拼完 full opaque payload。
pub async fn wait_and_query(
    config: &DriverConfig,
    host: &RuntimeHost,
    agent: mai_protocol::AgentId,
    task_id: &str,
    scope: &str,
    tag: &str,
) -> Result<TaskTerminalFacts> {
    let wait = wait_for_task(config, host, agent, task_id, scope, tag, WAIT_TIMEOUT_MS).await?;
    // 先从 wait target 回填状态与工具身份，避免仅凭 get 页面存在就认定目标匹配。
    let status = wait
        .target
        .as_ref()
        .and_then(|target| target.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let tool_is_exec = wait
        .target
        .as_ref()
        .and_then(|target| target.get("toolId"))
        .and_then(Value::as_str)
        .map(|tool| tool == "exec")
        .unwrap_or(false);
    let wait = TaskWaitFacts {
        status,
        tool_is_exec,
        ..wait
    };
    let query = if wait.terminal_exec_task() {
        query_task(config, host, agent, task_id, scope, tag).await?
    } else {
        TaskQueryFacts::default()
    };
    Ok(TaskTerminalFacts {
        task_id: task_id.to_string(),
        wait,
        query,
    })
}

async fn query_task(
    config: &DriverConfig,
    host: &RuntimeHost,
    agent: mai_protocol::AgentId,
    task_id: &str,
    scope: &str,
    tag: &str,
) -> Result<TaskQueryFacts> {
    let mut facts = TaskQueryFacts::default();
    let mut cursor = 0u64;
    let mut content = String::new();
    for page_index in 0..MAX_RESULT_PAGES {
        let probe_id = format!("channels-query-{scope}-{tag}-{cursor}");
        let probe = Probe::new(
            probe_id.clone(),
            vec![ProbeCall::new(
                "get_tool_task",
                json!({ "taskId": task_id, "resultCursor": cursor }),
            )],
        );
        let outcome = probe::run(host, agent, &probe, config.turn_timeout).await?;
        let call = outcome.call(0);
        let raw_output = call.and_then(|call| {
            outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id)
        });
        let view = raw_output.as_deref().and_then(outputs::exec_preview);
        let task = view.as_ref().and_then(|view| view.get("task")).cloned();
        let result = view.as_ref().and_then(|view| view.get("result"));
        let page_content = result
            .and_then(|result| result.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let next_cursor = result
            .and_then(|result| result.get("nextCursor"))
            .and_then(Value::as_u64);
        let total_bytes = result
            .and_then(|result| result.get("totalBytes"))
            .and_then(Value::as_u64);
        let format = result
            .and_then(|result| result.get("format"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let version = result
            .and_then(|result| result.get("version"))
            .and_then(Value::as_i64);
        let mut parse_error = None;
        if view.is_none() || result.is_none() {
            parse_error = Some("get_tool_task 输出不是预期的 JSON view".to_string());
        }
        if format.as_deref() != Some("pl.tool.exec") || version != Some(1) {
            parse_error = Some("get_tool_task result 格式或版本不匹配".to_string());
        }
        if cursor != content.len() as u64 {
            parse_error = Some("resultCursor 与已拼接字节不一致".to_string());
        }
        let task_id_on_page = task
            .as_ref()
            .and_then(|task| task.get("taskId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let status = task
            .as_ref()
            .and_then(|task| task.get("status"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if task_id_on_page.as_deref() != Some(task_id) {
            parse_error = Some("get_tool_task 返回了另一个 taskId".to_string());
        }
        if !status.as_deref().is_some_and(is_terminal_task_status) {
            parse_error = Some("get_tool_task 返回的任务不是终态".to_string());
        }
        if facts.terminal_status.is_none() {
            facts.terminal_status = status.clone();
        } else if facts.terminal_status != status {
            parse_error = Some("get_tool_task 分页之间任务状态变化".to_string());
        }
        facts.total_bytes = total_bytes;
        facts.pages.push(TaskPageFacts {
            probe_id,
            turn_state: outcome.turn_state.clone(),
            observed: call.is_some_and(|call| call.observed),
            invocation_success: call.map(|call| call.success).unwrap_or(None),
            raw_output,
            task_id: task_id_on_page,
            status,
            cursor,
            next_cursor,
            total_bytes,
            content_bytes: page_content.len(),
            format,
            version,
            parse_error: parse_error.clone(),
        });
        content.push_str(page_content);
        if let Some(error) = parse_error {
            facts.parse_error = Some(error);
            break;
        }
        match next_cursor {
            Some(next) if next == cursor => {
                facts.parse_error = Some("nextCursor 没有前进".to_string());
                break;
            }
            Some(next) if next != content.len() as u64 => {
                facts.parse_error = Some("nextCursor 与累计 UTF-8 字节不一致".to_string());
                break;
            }
            Some(next) => cursor = next,
            None => {
                if total_bytes != Some(content.len() as u64) {
                    facts.parse_error = Some("totalBytes 与拼接结果不一致".to_string());
                }
                break;
            }
        }
        if let Some(total) = total_bytes
            && cursor >= total
        {
            if cursor != total {
                facts.parse_error = Some("nextCursor 超过 totalBytes".to_string());
            }
            break;
        }
        if page_index + 1 == MAX_RESULT_PAGES {
            facts.parse_error = Some("get_tool_task 分页超过驱动保护上限".to_string());
        }
    }
    facts.content_bytes = content.len();
    facts.payload = serde_json::from_str::<Value>(&content).ok();
    if facts.payload.is_none() && facts.parse_error.is_none() {
        facts.parse_error = Some("拼接后的 opaque payload 不是 JSON".to_string());
    }
    Ok(facts)
}
