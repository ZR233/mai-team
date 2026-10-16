//! Turn 驱动：用 fixture provider 发真实 turn，按持久工具历史判定真实工具执行结果。
//!
//! fixture provider 只在每个 `probe_id` 的第一次请求里发一批工具调用，因此一个 probe
//! 就是**一轮**工具调用；工具调用的 call id 由 fixture 固定为 `call_<probe_id>_<序号>`，
//! 与请求里的工具顺序一一对应，判定因而可以按序号精确对齐，不靠名字猜测。

use std::path::Path;
use std::time::Duration;

use mai_protocol::{AgentDetail, AgentId, Turn, TurnState};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout as with_timeout};

use crate::error::{Result, error};
use crate::host::RuntimeHost;

/// fixture provider 识别的用户文本标记。
pub const PROBE_MARKER: &str = "MAI_REFERENCE_PROBE";

/// fixture provider 收尾文本前缀。
pub const PROBE_DONE_MARKER: &str = "MAI_REFERENCE_PROBE_DONE";

/// 默认轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 本次运行的唯一短标签：probe_id 必须每次都不同，否则 fixture 不会重复发工具调用。
pub fn run_tag() -> String {
    let uuid = uuid::Uuid::new_v4().to_string();
    format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        &uuid[..8]
    )
}

/// 一次工具调用请求。
#[derive(Debug, Clone, Serialize)]
pub struct ProbeCall {
    pub name: String,
    pub arguments: Value,
}

impl ProbeCall {
    /// 构造一次工具调用。
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}

/// 一个 probe：probe_id + 一轮工具调用 + 可选的 provider 侧延迟。
#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    probe_id: String,
    tools: Vec<ProbeCall>,
    delay_seconds: u64,
}

impl Probe {
    /// 构造 probe；`probe_id` 必须本次运行内唯一（fixture 只在第一次请求发工具调用）。
    pub fn new(probe_id: impl Into<String>, tools: Vec<ProbeCall>) -> Self {
        Self {
            probe_id: probe_id.into(),
            tools,
            delay_seconds: 0,
        }
    }

    /// 让 fixture 在发出工具调用前先延迟若干秒（用于制造真实的“活动 turn”窗口）。
    pub fn with_provider_delay(mut self, seconds: u64) -> Self {
        self.delay_seconds = seconds;
        self
    }

    /// probe 身份。
    pub fn probe_id(&self) -> &str {
        &self.probe_id
    }

    /// 工具调用条数。
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// 发给 Runtime 的用户文本；fixture 只解析标记之后紧邻的 JSON。
    pub fn message(&self) -> Result<String> {
        let body = json!({
            "probe_id": &self.probe_id,
            "tools": &self.tools,
            "delay_seconds": self.delay_seconds,
        });
        let encoded = serde_json::to_string(&body).map_err(|cause| {
            error(
                "probe_encode_failed",
                format!("无法编码 probe {}：{cause}", self.probe_id),
            )
        })?;
        Ok(format!("{PROBE_MARKER} {encoded}"))
    }
}

/// 一条期望工具调用的真实执行事实。
#[derive(Debug, Clone, Serialize)]
pub struct ProbeCallFacts {
    pub index: usize,
    pub name: String,
    pub expected_call_id: String,
    /// 真实工具历史 是否存在（不存在说明 fixture 没发、被拒或 turn 提前结束）。
    pub observed: bool,
    pub success: Option<bool>,
    pub output_preview: String,
    pub output: String,
}

/// 一次 probe 的完整结论。
#[derive(Debug, Clone, Serialize)]
pub struct ProbeOutcome {
    pub probe_id: String,
    pub input_id: String,
    pub turn_id: Option<String>,
    pub turn_state: String,
    pub terminal: bool,
    pub thread_status: String,
    pub calls: Vec<ProbeCallFacts>,
    /// 同 turn 里出现的、不属于本 probe 期望调用的 trace（供人核对）。
    pub unexpected_traces: Vec<String>,
}

impl ProbeOutcome {
    /// 第 `index` 条调用的结论。
    pub fn call(&self, index: usize) -> Option<&ProbeCallFacts> {
        self.calls.get(index)
    }

    /// 是否全部期望调用都被观察到。
    pub fn all_observed(&self) -> bool {
        self.calls.iter().all(|call| call.observed)
    }
}

/// 观察目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watch {
    /// 该输入对应的 Turn 正在执行。
    Active,
    /// 该输入对应的 Turn 已进入终态。
    Terminal,
}

/// 一次 turn 状态观察。
#[derive(Debug, Clone)]
pub struct TurnWatch {
    pub turn: Option<Turn>,
    pub state: String,
    pub thread_status: String,
}

impl TurnWatch {
    /// 是否已进入终态。
    pub fn terminal(&self) -> bool {
        self.turn
            .as_ref()
            .is_some_and(|turn| turn.state.is_terminal())
    }

    /// 终态是否为取消。
    pub fn cancelled(&self) -> bool {
        matches!(
            self.turn.as_ref().map(|turn| &turn.state),
            Some(TurnState::Cancelled(_))
        )
    }

    /// 是否是成功完成。
    pub fn completed(&self) -> bool {
        matches!(
            self.turn.as_ref().map(|turn| &turn.state),
            Some(TurnState::Completed(_))
        )
    }
}

/// 发送 probe；返回受理的输入身份。
pub async fn send(host: &RuntimeHost, agent_id: AgentId, probe: &Probe) -> Result<String> {
    host.runtime
        .send_message(agent_id, probe.message()?, Vec::new())
        .await
        .map_err(|cause| {
            error(
                "turn_not_accepted",
                format!(
                    "Agent {agent_id} 拒绝了 probe {}：{cause}",
                    probe.probe_id()
                ),
            )
        })
}

/// 等待某个输入进入指定状态；超时返回最后一次观察，不静默成功。
pub async fn watch(
    host: &RuntimeHost,
    agent_id: AgentId,
    input_id: &str,
    want: Watch,
    limit: Duration,
) -> Result<TurnWatch> {
    let observe = async {
        loop {
            let snapshot = snapshot(host, agent_id, input_id).await?;
            let matched = match want {
                Watch::Active => matches!(
                    snapshot.turn.as_ref().map(|turn| &turn.state),
                    Some(TurnState::Running(_))
                ),
                Watch::Terminal => snapshot.terminal(),
            };
            if matched {
                return Ok(snapshot);
            }
            sleep(POLL_INTERVAL).await;
        }
    };
    match with_timeout(limit, observe).await {
        Ok(result) => result,
        Err(_) => {
            let last = snapshot(host, agent_id, input_id).await?;
            Err(error(
                "turn_watch_timeout",
                format!(
                    "等待输入 {input_id}（{want:?}）超时 {:?}；最后状态 {}（thread {}）",
                    limit, last.state, last.thread_status
                ),
            ))
        }
    }
}

/// 发送一个 probe 并等待它进入终态，然后按序号收集真实工具历史。
pub async fn run(
    host: &RuntimeHost,
    agent_id: AgentId,
    probe: &Probe,
    limit: Duration,
) -> Result<ProbeOutcome> {
    let input_id = send(host, agent_id, probe).await?;
    let watched = watch(host, agent_id, &input_id, Watch::Terminal, limit).await?;
    collect(host, agent_id, probe, &input_id, watched).await
}

/// 等待该 Agent 出现一个**新的**终态 turn，并按 probe 的 call id 收集 trace。
///
/// 用于 `spawn_agent` 交付给 child 的初始消息：那条 turn 不是驱动发送的，没有驱动侧输入
/// 身份，但工具调用 id 仍由 fixture 固定为 `call_<probe_id>_<序号>`，判定照样精确。
pub async fn collect_latest(
    host: &RuntimeHost,
    agent_id: AgentId,
    probe: &Probe,
    limit: Duration,
) -> Result<ProbeOutcome> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let detail = host.agent_detail(agent_id).await?;
        if let Some(turn) = detail
            .summary
            .last_turn
            .clone()
            .filter(|turn| turn.state.is_terminal())
        {
            let watched = TurnWatch {
                state: turn_state_label(&turn.state).to_string(),
                turn: Some(turn.clone()),
                thread_status: format!("{:?}", detail.thread.status),
            };
            let outcome = collect(
                host,
                agent_id,
                probe,
                turn.input_id.as_deref().unwrap_or_default(),
                watched,
            )
            .await?;
            // child 的初始消息可能先产生别的终态 turn；只有看到本 probe 的 call id 才算收敛。
            if outcome.all_observed() || tokio::time::Instant::now() >= deadline {
                return Ok(outcome);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error(
                "probe_latest_timeout",
                format!(
                    "等待 Agent {agent_id} 完成 probe {} 超时 {limit:?}",
                    probe.probe_id()
                ),
            ));
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// 收集已受理 turn 的工具历史（不重新发送）。
pub async fn collect(
    host: &RuntimeHost,
    agent_id: AgentId,
    probe: &Probe,
    input_id: &str,
    watched: TurnWatch,
) -> Result<ProbeOutcome> {
    let turn_id = watched.turn.as_ref().map(|turn| turn.id.clone());
    // 持久历史是执行事实的唯一来源；有界诊断 trace 不承担验收或会话恢复责任。
    let history = host
        .runtime
        .thread_turns(agent_id.to_string(), None, 50)
        .await
        .map_err(|cause| error("tool_history_unreadable", cause.to_string()))?;
    let tools = history
        .turns
        .iter()
        .filter(|turn| turn_id.as_ref().is_none_or(|id| turn.turn.id == *id))
        .flat_map(|turn| turn.items.iter())
        .filter_map(|item| item.tool())
        .collect::<Vec<_>>();
    let expected_ids = (0..probe.tool_count())
        .map(|index| format!("call_{}_{index}", probe.probe_id()))
        .collect::<Vec<_>>();
    let mut calls = Vec::with_capacity(probe.tool_count());
    for (index, call) in probe.tools.iter().enumerate() {
        let expected_call_id = expected_ids[index].clone();
        let observed = tools.iter().find(|tool| {
            tool.invocation()
                .call_id()
                .unwrap_or_else(|| tool.invocation().tool_call_id())
                == expected_call_id
        });
        let output = observed
            .map(|tool| {
                tool.terminal_output()
                    .map(|output| output.result())
                    .or_else(|| tool.state().failure())
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default();
        calls.push(ProbeCallFacts {
            index,
            name: call.name.clone(),
            expected_call_id,
            observed: observed.is_some(),
            success: observed
                .filter(|tool| tool.state().is_terminal())
                .map(|tool| matches!(tool.state(), pl_protocol::ThreadToolState::Succeeded(_))),
            output_preview: output.chars().take(2048).collect(),
            output,
        });
    }
    let unexpected_traces = tools
        .iter()
        .filter(|tool| {
            !expected_ids.iter().any(|id| {
                tool.invocation()
                    .call_id()
                    .unwrap_or_else(|| tool.invocation().tool_call_id())
                    == id
            })
        })
        .map(|tool| {
            format!(
                "{}:{}",
                tool.invocation().tool_call_id(),
                tool.invocation().name()
            )
        })
        .collect::<Vec<_>>();
    Ok(ProbeOutcome {
        probe_id: probe.probe_id().to_string(),
        input_id: input_id.to_string(),
        turn_id,
        turn_state: watched.state.clone(),
        terminal: watched.terminal(),
        thread_status: watched.thread_status,
        calls,
        unexpected_traces,
    })
}

/// 读取某个输入当前的 turn 状态快照。
pub async fn snapshot(host: &RuntimeHost, agent_id: AgentId, input_id: &str) -> Result<TurnWatch> {
    let detail: AgentDetail = host.agent_detail(agent_id).await?;
    let active = detail
        .summary
        .runtime
        .as_ref()
        .and_then(|snapshot| snapshot.active_turn.clone())
        .filter(|turn| turn.input_id.as_deref() == Some(input_id));
    let last = detail
        .summary
        .last_turn
        .clone()
        .filter(|turn| turn.input_id.as_deref() == Some(input_id));
    let turn = active.or(last);
    let thread_status = detail
        .summary
        .runtime
        .as_ref()
        .map(|snapshot| format!("{:?}", snapshot.thread.status))
        .unwrap_or_else(|| format!("{:?}", detail.thread.status));
    Ok(TurnWatch {
        state: turn
            .as_ref()
            .map(|turn| turn_state_label(&turn.state).to_string())
            .unwrap_or_else(|| "not_started".to_string()),
        turn,
        thread_status,
    })
}

/// Turn 状态的稳定标签。
pub fn turn_state_label(state: &TurnState) -> &'static str {
    match state {
        TurnState::Queued(_) => "queued",
        TurnState::Running(_) => "running",
        TurnState::Completed(_) => "completed",
        TurnState::Cancelled(_) => "cancelled",
        TurnState::Failed(_) => "failed",
        TurnState::BudgetLimited(_) => "budget_limited",
    }
}

/// fixture provider 记录的请求集合（按文件名排序，缺目录时返回空表）。
pub fn provider_requests(dir: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("request-"))
        .collect::<Vec<_>>();
    files.sort_by_key(|entry| entry.file_name());
    files
        .into_iter()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .collect()
}

/// 请求里包含某段文本的条数（用于核对模型实际收到的参考清单/工具回填）。
pub fn requests_containing(requests: &[Value], needle: &str) -> usize {
    requests
        .iter()
        .filter(|request| request.to_string().contains(needle))
        .count()
}

/// 请求里出现 `function_call_output` 的条数：真实工具输出被回填给模型的证据。
pub fn requests_with_tool_output(requests: &[Value]) -> usize {
    requests_containing(requests, "function_call_output")
}
