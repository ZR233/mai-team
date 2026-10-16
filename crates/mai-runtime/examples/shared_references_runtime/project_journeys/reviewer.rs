//! 真实 Reviewer 的发现与容错探针。
//!
//! 自动审查的 Reviewer 由审查生命周期在**短窗口**内创建：准备上下文 → 装配容器 → 跑一轮
//! 初始 turn → 收尾后 `delete_agent` 回收。因此探针不能像 `probe::run` 那样"先等终态再收集"，
//! 必须在 Agent 存活期间持续读 canonical 工具历史，并在 Agent 已被回收时保留最后一次真实观察。
//!
//! 判定只来自真实事实：`Runtime::thread_turns`（canonical durable effects 投影）与
//! `Runtime::list_agents`（内存投影）。任何缺失都写进 `disappeared`，绝不补一个假成功。

use std::time::Duration;

use mai_protocol::{AgentId, AgentRole, AgentSummary, ProjectId, ThreadTurnPage, TurnId};
use tokio::time::sleep;

use crate::error::Result;
use crate::host::RuntimeHost;
use crate::probe::{self, Probe, ProbeCallFacts, ProbeOutcome};

/// 轮询间隔。
const POLL: Duration = Duration::from_millis(250);

/// 连续读取失败的容忍次数：超过即认定 Reviewer 已被审查生命周期回收。
const MISSING_TOLERANCE: u32 = 4;

/// 一次 Reviewer 探针的结论。
#[derive(Debug)]
pub struct ReviewerProbe {
    pub outcome: ProbeOutcome,
    /// Reviewer 在收集期间被回收时的真实错误原文。
    pub disappeared: Option<String>,
}

/// 等待真实 Reviewer Agent 出现并且已经拿到执行环境。
pub async fn wait_for_reviewer(
    host: &RuntimeHost,
    project_id: ProjectId,
    limit: Duration,
) -> Result<Option<AgentSummary>> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(summary) = current_reviewer(host, project_id).await?
            && host.container_id(summary.id).await?.is_some()
        {
            return Ok(Some(summary));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(POLL).await;
    }
}

/// 当前项目的 Reviewer Agent：优先已经发布容器的那个，其次取最新创建的一个。
async fn current_reviewer(
    host: &RuntimeHost,
    project_id: ProjectId,
) -> Result<Option<AgentSummary>> {
    let mut reviewers = host
        .runtime
        .list_agents()
        .await
        .into_iter()
        .filter(|summary| {
            summary.project_id == Some(project_id) && summary.role == Some(AgentRole::Reviewer)
        })
        .collect::<Vec<_>>();
    reviewers.sort_by_key(|summary| summary.created_at);
    if let Some(summary) = reviewers
        .iter()
        .rfind(|summary| summary.container_id.is_some())
    {
        return Ok(Some(summary.clone()));
    }
    Ok(reviewers.pop())
}

/// 发送 probe 消息（真实产品入口）；失败时返回驱动错误。
pub async fn send(host: &RuntimeHost, reviewer: AgentId, probe: &Probe) -> Result<TurnId> {
    probe::send(host, reviewer, probe).await
}

/// 收集一次已发送 probe 的真实工具结论，容忍 Reviewer 中途被回收。
pub async fn collect(
    host: &RuntimeHost,
    reviewer: AgentId,
    probe: &Probe,
    input_id: &str,
    limit: Duration,
) -> Result<ReviewerProbe> {
    let deadline = tokio::time::Instant::now() + limit;
    let mut calls: Vec<ProbeCallFacts> = Vec::new();
    let mut state = "not_observed".to_string();
    let mut disappeared: Option<String> = None;
    let mut missing = 0_u32;
    loop {
        match host
            .runtime
            .thread_turns(reviewer.to_string(), None, 50)
            .await
        {
            Ok(history) => {
                calls = collect_calls(probe, &history);
                if calls
                    .iter()
                    .all(|call| call.observed && call.success.is_some())
                {
                    state = "collected".to_string();
                    break;
                }
                missing = 0;
                if let Ok(summary) = host.agent(reviewer).await
                    && let Some(turn) = summary.last_turn.as_ref()
                {
                    state = probe::turn_state_label(&turn.state).to_string();
                }
            }
            Err(cause) => {
                missing += 1;
                state = "history_unreadable".to_string();
                disappeared = Some(cause.to_string());
                if missing >= MISSING_TOLERANCE {
                    break;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            state = format!("{state}+timeout");
            break;
        }
        sleep(POLL).await;
    }
    let terminal = probe.tool_count() > 0
        && calls.len() == probe.tool_count()
        && calls
            .iter()
            .all(|call| call.observed && call.success.is_some());
    Ok(ReviewerProbe {
        outcome: ProbeOutcome {
            probe_id: probe.probe_id().to_string(),
            input_id: input_id.to_string(),
            turn_id: None,
            turn_state: state,
            terminal,
            thread_status: String::new(),
            calls,
            unexpected_traces: Vec::new(),
        },
        disappeared,
    })
}

/// 从 canonical Turn 历史里按 fixture 固定的 call id 收集结论。
///
/// call id 由 fixture 固定为 `call_<probe_id>_<序号>`，因此判定按序号精确对齐，不靠名字猜测；
/// `name` 取真实执行到的工具名，便于人工核对。
pub fn collect_calls(probe: &Probe, history: &ThreadTurnPage) -> Vec<ProbeCallFacts> {
    let tools = history
        .turns
        .iter()
        .flat_map(|turn| turn.items.iter())
        .filter_map(|item| item.tool())
        .collect::<Vec<_>>();
    let mut calls = Vec::with_capacity(probe.tool_count());
    for index in 0..probe.tool_count() {
        let expected_call_id = format!("call_{}_{index}", probe.probe_id());
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
            name: observed
                .map(|tool| tool.invocation().name().to_string())
                .unwrap_or_default(),
            expected_call_id,
            observed: observed.is_some(),
            success: observed
                .filter(|tool| tool.state().is_terminal())
                .map(|tool| matches!(tool.state(), pl_protocol::ThreadToolState::Succeeded(_))),
            output_preview: output.chars().take(2048).collect(),
            output,
        });
    }
    calls
}
