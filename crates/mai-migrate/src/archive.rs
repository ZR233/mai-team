use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use pl_protocol::{
    AgentMessageChannel, ThreadContextDisposition, ThreadItem, ThreadItemContent, ThreadItemStatus,
    ThreadToolCall, ThreadTurnHistory, Turn, TurnState,
};
use serde_json::Value;

mod context;

#[derive(Debug, Clone)]
pub(crate) struct ContextRow {
    pub id: String,
    pub position: i64,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewArchiveSource {
    pub run_id: String,
    pub reviewer_thread_id: Option<String>,
    pub requested_turn_id: Option<String>,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub messages_json: String,
    pub events_json: String,
}

pub(crate) fn session_history(
    thread_id: &str,
    turn_id: &str,
    created_at: i64,
    updated_at: i64,
    disposition: ThreadContextDisposition,
    rows: &[ContextRow],
) -> Result<Option<ThreadTurnHistory>> {
    let items = context::context_items(thread_id, turn_id, updated_at, rows)?;
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(ThreadTurnHistory {
        turn: Turn {
            id: turn_id.to_string(),
            thread_id: thread_id.to_string(),
            state: TurnState::Completed,
            failure: None,
            started_at: Some(created_at),
            updated_at,
            completed_at: Some(updated_at),
        },
        items,
        context_disposition: disposition,
    }))
}

pub(crate) fn review_history(source: &ReviewArchiveSource) -> Result<ThreadTurnHistory> {
    let messages = serde_json::from_str::<Vec<Value>>(&source.messages_json)
        .with_context(|| format!("review run {} 的 messages_json 非法", source.run_id))?;
    let events = serde_json::from_str::<Vec<Value>>(&source.events_json)
        .with_context(|| format!("review run {} 的 events_json 非法", source.run_id))?;
    validate_event_sequences(&source.run_id, &events)?;
    let turn_id = source
        .requested_turn_id
        .as_deref()
        .map(str::to_string)
        .or_else(|| event_turn_id(&events))
        .unwrap_or_else(|| format!("review:{}", source.run_id));
    let thread_id = source
        .reviewer_thread_id
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| format!("archived-review:{}", source.run_id));
    let mut items = latest_review_parts(&thread_id, &turn_id, &events)?;
    if items.is_empty() {
        items = archived_messages(&thread_id, &turn_id, &messages)?;
    } else {
        let mut user_items = archived_user_messages(&thread_id, &turn_id, &messages)?;
        user_items.append(&mut items);
        for (ordinal, item) in user_items.iter_mut().enumerate() {
            item.ordinal = u64::try_from(ordinal)?;
        }
        items = user_items;
    }
    let completed_at = source.finished_at.unwrap_or(source.started_at);
    Ok(ThreadTurnHistory {
        turn: Turn {
            id: turn_id,
            thread_id,
            state: review_turn_state(&source.status)?,
            failure: None,
            started_at: Some(source.started_at),
            updated_at: completed_at,
            completed_at: Some(completed_at),
        },
        items,
        context_disposition: ThreadContextDisposition::RolledBack,
    })
}

fn latest_review_parts(
    thread_id: &str,
    turn_id: &str,
    events: &[Value],
) -> Result<Vec<ThreadItem>> {
    let mut parts = BTreeMap::<String, Value>::new();
    for event in events {
        let kind = required_object(event, "kind")?;
        if required_str(kind, "type")? != "partChanged" {
            continue;
        }
        let part = required_object(kind, "part")?;
        if required_str(part, "turnId")? != turn_id {
            continue;
        }
        let id = required_str(part, "partId")?.to_string();
        let revision = required_u64(part, "revision")?;
        if let Some(previous) = parts.get(&id) {
            let previous_revision = required_u64(previous, "revision")?;
            if revision < previous_revision {
                bail!("review part {id} revision 回退: {previous_revision} -> {revision}");
            }
        }
        parts.insert(id, part.clone());
    }
    let mut items = Vec::new();
    for part in parts.into_values() {
        if let Some(item) = review_part(thread_id, turn_id, &part)? {
            items.push(item);
        }
    }
    items.sort_by_key(|item| item.ordinal);
    Ok(items)
}

fn review_part(thread_id: &str, turn_id: &str, part: &Value) -> Result<Option<ThreadItem>> {
    let content = required_object(part, "content")?;
    let content = match required_str(content, "type")? {
        "text" => {
            let text = content
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match required_str(content, "channel")? {
                "user" => ThreadItemContent::UserMessage {
                    text,
                    attachments: Vec::new(),
                },
                "commentary" => ThreadItemContent::AgentMessage {
                    channel: AgentMessageChannel::Commentary,
                    text,
                },
                "final" => ThreadItemContent::AgentMessage {
                    channel: AgentMessageChannel::Final,
                    text,
                },
                other => bail!("未知 text channel `{other}`"),
            }
        }
        "reasoning" => ThreadItemContent::Reasoning {
            summary: Vec::new(),
            content: content
                .get("text")
                .and_then(Value::as_str)
                .map(|text| vec![text.to_string()])
                .unwrap_or_default(),
        },
        "tool" => ThreadItemContent::ToolCall {
            tool: serde_json::from_value(
                content
                    .get("tool")
                    .cloned()
                    .context("tool part 缺少 tool")?,
            )?,
        },
        "inference" | "turn" => return Ok(None),
        other => bail!("未知 review part content `{other}`"),
    };
    Ok(Some(ThreadItem {
        id: required_str(part, "partId")?.to_string(),
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        ordinal: required_u64(part, "order")?,
        revision: required_u64(part, "revision")?,
        status: parse_item_status(required_str(part, "status")?)?,
        created_at: required_i64(part, "createdAt")?,
        updated_at: required_i64(part, "updatedAt")?,
        completed_at: part.get("completedAt").and_then(Value::as_i64),
        error: part
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string),
        content,
        usage: None,
    }))
}

fn archived_messages(
    thread_id: &str,
    turn_id: &str,
    messages: &[Value],
) -> Result<Vec<ThreadItem>> {
    let mut items = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        items.push(archived_message(thread_id, turn_id, message, index)?);
    }
    Ok(items)
}

fn archived_user_messages(
    thread_id: &str,
    turn_id: &str,
    messages: &[Value],
) -> Result<Vec<ThreadItem>> {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .enumerate()
        .map(|(index, message)| archived_message(thread_id, turn_id, message, index))
        .collect()
}

fn archived_message(
    thread_id: &str,
    turn_id: &str,
    message: &Value,
    index: usize,
) -> Result<ThreadItem> {
    let text = required_str(message, "content")?.to_string();
    let created_at =
        chrono::DateTime::parse_from_rfc3339(required_str(message, "created_at")?)?.timestamp();
    let content = match required_str(message, "role")? {
        "user" => ThreadItemContent::UserMessage {
            text,
            attachments: Vec::new(),
        },
        "assistant" => ThreadItemContent::AgentMessage {
            channel: AgentMessageChannel::Final,
            text,
        },
        "system" => ThreadItemContent::AgentMessage {
            channel: AgentMessageChannel::Commentary,
            text,
        },
        "tool" => ThreadItemContent::ToolCall {
            tool: ThreadToolCall {
                tool_call_id: format!("review:{turn_id}:message:{index}"),
                call_id: format!("review:{turn_id}:message:{index}"),
                provider_item_id: None,
                name: "archived_tool_output".to_string(),
                arguments: String::new(),
                result: Some(text),
                output_artifacts: Vec::new(),
                exit_code: None,
                timed_out: false,
                working_directory: None,
                denial_reason: None,
            },
        },
        other => bail!("未知 review message role `{other}`"),
    };
    Ok(item(
        format!("review:{turn_id}:message:{index}"),
        thread_id,
        turn_id,
        u64::try_from(index)?,
        1,
        created_at,
        content,
    ))
}

fn validate_event_sequences(run_id: &str, events: &[Value]) -> Result<()> {
    let mut last = HashMap::<String, u64>::new();
    for event in events {
        let session_id = required_str(event, "sessionId")?;
        let position = required_object(event, "position")?;
        let sequence = required_u64(position, "sequence")?;
        if let Some(previous) = last.insert(session_id.to_string(), sequence)
            && sequence <= previous
        {
            bail!("review run {run_id} 的事件 sequence 未严格递增: {previous} -> {sequence}");
        }
    }
    Ok(())
}

fn event_turn_id(events: &[Value]) -> Option<String> {
    events.iter().find_map(|event| {
        event
            .get("turnId")
            .or_else(|| event.get("kind")?.get("turn")?.get("id"))
            .or_else(|| event.get("kind")?.get("part")?.get("turnId"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn review_turn_state(status: &str) -> Result<TurnState> {
    Ok(match status {
        "completed" | "succeeded" => TurnState::Completed,
        "interrupted" | "cancelled" => TurnState::Interrupted {
            reason: "历史 review 已中断".to_string(),
        },
        "failed" | "retryable_failed" | "permanent_failed" => TurnState::Failed {
            reason: "历史 review 失败".to_string(),
        },
        other => bail!("未知历史 review 状态: {other}"),
    })
}

fn parse_item_status(value: &str) -> Result<ThreadItemStatus> {
    Ok(match value {
        "started" => ThreadItemStatus::Started,
        "streaming" => ThreadItemStatus::Streaming,
        "awaitingApproval" | "awaiting_approval" => ThreadItemStatus::AwaitingApproval,
        "approved" => ThreadItemStatus::Approved,
        "denied" => ThreadItemStatus::Denied,
        "running" => ThreadItemStatus::Running,
        "completed" => ThreadItemStatus::Completed,
        "failed" => ThreadItemStatus::Failed,
        "interrupted" => ThreadItemStatus::Interrupted,
        "budgetLimited" | "budget_limited" => ThreadItemStatus::BudgetLimited,
        other => bail!("未知 Thread item 状态 `{other}`"),
    })
}

fn item(
    id: String,
    thread_id: &str,
    turn_id: &str,
    ordinal: u64,
    revision: u64,
    timestamp: i64,
    content: ThreadItemContent,
) -> ThreadItem {
    ThreadItem {
        id,
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        ordinal,
        revision,
        status: ThreadItemStatus::Completed,
        created_at: timestamp,
        updated_at: timestamp,
        completed_at: Some(timestamp),
        error: None,
        content,
        usage: None,
    }
}

fn next_ordinal(items: &[ThreadItem]) -> Result<u64> {
    Ok(u64::try_from(items.len())?)
}

fn required_object<'a>(value: &'a Value, field: &str) -> Result<&'a Value> {
    value
        .get(field)
        .filter(|value| value.is_object())
        .with_context(|| format!("缺少对象字段 `{field}`"))
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("缺少字符串字段 `{field}`"))
}

fn required_u64(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("缺少非负整数字段 `{field}`"))
}

fn required_i64(value: &Value, field: &str) -> Result<i64> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .with_context(|| format!("缺少整数字段 `{field}`"))
}
