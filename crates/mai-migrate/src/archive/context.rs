use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use pl_protocol::{
    AgentMessageChannel, ThreadAttachment, ThreadItem, ThreadItemContent, ThreadToolCall,
};
use serde_json::Value;

use super::{ContextRow, item, next_ordinal, required_object, required_str};

pub(super) fn context_items(
    thread_id: &str,
    turn_id: &str,
    timestamp: i64,
    rows: &[ContextRow],
) -> Result<Vec<ThreadItem>> {
    let mut items = Vec::<ThreadItem>::new();
    let mut tools = HashMap::<String, usize>::new();
    for row in rows {
        let kind = required_str(&row.value, "type")?;
        match kind {
            "message" => {
                let message = required_object(&row.value, "message")?;
                let role = required_str(message, "role")?;
                let text = message
                    .get("content")
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                    .context("历史 message 缺少 content.text")?;
                if role == "assistant" {
                    append_assistant_tool_calls(
                        &mut items, &mut tools, thread_id, turn_id, timestamp, row, message,
                    )?;
                }
                let content = match role {
                    "user" => Some(ThreadItemContent::UserMessage {
                        text: text.to_string(),
                        attachments: Vec::<ThreadAttachment>::new(),
                    }),
                    "assistant" if !text.is_empty() => Some(ThreadItemContent::AgentMessage {
                        channel: AgentMessageChannel::Final,
                        text: text.to_string(),
                    }),
                    "assistant" => None,
                    "system" => Some(ThreadItemContent::AgentMessage {
                        channel: AgentMessageChannel::Commentary,
                        text: text.to_string(),
                    }),
                    other => bail!("不支持的历史 message role `{other}`"),
                };
                if let Some(content) = content {
                    items.push(item(
                        row.id.clone(),
                        thread_id,
                        turn_id,
                        next_ordinal(&items)?,
                        1,
                        timestamp,
                        content,
                    ));
                }
            }
            "toolResult" => {
                apply_tool_result(&mut items, &mut tools, thread_id, turn_id, timestamp, row)?;
            }
            "pinnedContext" => {
                let section = required_object(&row.value, "section")?;
                let content = required_str(section, "content")?;
                items.push(item(
                    row.id.clone(),
                    thread_id,
                    turn_id,
                    next_ordinal(&items)?,
                    1,
                    timestamp,
                    ThreadItemContent::Plan {
                        content: content.to_string(),
                    },
                ));
            }
            "sessionNote" => {
                let note = required_object(&row.value, "note")?;
                let content = required_str(note, "content")?;
                items.push(item(
                    row.id.clone(),
                    thread_id,
                    turn_id,
                    next_ordinal(&items)?,
                    1,
                    timestamp,
                    ThreadItemContent::Plan {
                        content: content.to_string(),
                    },
                ));
            }
            other => bail!("不支持的 ModelContextItem 类型 `{other}`"),
        }
    }
    Ok(items)
}

fn append_assistant_tool_calls(
    items: &mut Vec<ThreadItem>,
    tools: &mut HashMap<String, usize>,
    thread_id: &str,
    turn_id: &str,
    timestamp: i64,
    row: &ContextRow,
    message: &Value,
) -> Result<()> {
    let Some(encoded) = message
        .get("metadata")
        .and_then(|metadata| metadata.get("tool_calls"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let calls = serde_json::from_str::<Vec<Value>>(encoded)
        .context("无法解析 assistant metadata.tool_calls")?;
    for (index, call) in calls.into_iter().enumerate() {
        let tool_call_id = required_str(&call, "id")?.to_string();
        let name = required_str(&call, "name")?.to_string();
        let payload = required_object(&call, "payload")?;
        let arguments = payload
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let tool = ThreadToolCall {
            tool_call_id: tool_call_id.clone(),
            call_id: call
                .get("call_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            provider_item_id: None,
            name,
            arguments: serde_json::to_string(&arguments)?,
            result: None,
            output_artifacts: Vec::new(),
            exit_code: None,
            timed_out: false,
            working_directory: None,
            denial_reason: None,
        };
        let item_id = format!("{}:tool:{index}", row.id);
        tools.insert(tool_call_id, items.len());
        items.push(item(
            item_id,
            thread_id,
            turn_id,
            next_ordinal(items)?,
            1,
            timestamp,
            ThreadItemContent::ToolCall { tool },
        ));
    }
    Ok(())
}

fn apply_tool_result(
    items: &mut Vec<ThreadItem>,
    tools: &mut HashMap<String, usize>,
    thread_id: &str,
    turn_id: &str,
    timestamp: i64,
    row: &ContextRow,
) -> Result<()> {
    let message = required_object(&row.value, "message")?;
    let metadata = required_object(message, "metadata")?;
    let tool_call_id = required_str(metadata, "tool_call_id")?.to_string();
    let name = required_str(metadata, "tool_name")?.to_string();
    let result = message
        .get("content")
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .context("toolResult 缺少 message.content.text")?
        .to_string();
    if let Some(index) = tools.get(&tool_call_id).copied() {
        let ThreadItemContent::ToolCall { tool } = &mut items[index].content else {
            bail!("tool 索引指向了非工具 Item");
        };
        tool.result = Some(result);
        items[index].revision = items[index].revision.saturating_add(1);
        items[index].updated_at = timestamp;
        return Ok(());
    }
    let tool = ThreadToolCall {
        tool_call_id: tool_call_id.clone(),
        call_id: metadata
            .get("tool_call_call_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        provider_item_id: None,
        name,
        arguments: metadata
            .get("tool_call_arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string(),
        result: Some(result),
        output_artifacts: Vec::new(),
        exit_code: None,
        timed_out: false,
        working_directory: None,
        denial_reason: None,
    };
    tools.insert(tool_call_id, items.len());
    items.push(item(
        row.id.clone(),
        thread_id,
        turn_id,
        next_ordinal(items)?,
        1,
        timestamp,
        ThreadItemContent::ToolCall { tool },
    ));
    Ok(())
}
