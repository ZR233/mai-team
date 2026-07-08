use std::collections::HashSet;

use chrono::{DateTime, Utc};
use mai_protocol::{
    AgentId, ServiceEventKind, SessionId, ToolDefinition, ToolTraceDetail, TurnId, now,
};
use pl_trace::{TraceEvent, TraceEventKind, TracePart, TracePartStatus};
use serde_json::{Value, json};

use crate::AgentRuntime;
use crate::turn::persistence::AgentLogRecord;

/// 根据 pl-core 共享工具 schema 与 mai 产品工具 schema 构造模型可见工具列表。
pub(crate) fn model_tool_definitions(
    visible_names: &HashSet<String>,
    mut product_tools: Vec<ToolDefinition>,
) -> Vec<ToolDefinition> {
    let mut tools = shared_tool_definitions(visible_names);
    tools.append(&mut product_tools);
    tools
}

/// 将 pl-core trace 投影为 mai-store 和 Web UI 仍在消费的 tool lifecycle 事件。
pub(crate) async fn project_tool_trace_events(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    events: &[TraceEvent],
) {
    for event in events {
        match &event.kind {
            TraceEventKind::TracePartStarted { item }
                if item.status == TracePartStatus::Started && item.tool.is_some() =>
            {
                project_tool_started(runtime, agent_id, session_id, turn_id, item).await;
            }
            TraceEventKind::TracePartCompleted { item } if item.tool.is_some() => {
                project_tool_completed(runtime, agent_id, session_id, turn_id, item, true).await;
            }
            TraceEventKind::TracePartFailed { item, .. } if item.tool.is_some() => {
                project_tool_completed(runtime, agent_id, session_id, turn_id, item, false).await;
            }
            TraceEventKind::TracePartDelta { .. }
            | TraceEventKind::PlanLifecycleChanged { .. }
            | TraceEventKind::InteractionChanged { .. }
            | TraceEventKind::SkillActivated { .. }
            | TraceEventKind::EnabledToolsRecorded { .. }
            | TraceEventKind::TracePartStarted { .. }
            | TraceEventKind::TracePartCompleted { .. }
            | TraceEventKind::TracePartFailed { .. } => {}
        }
    }
}

fn shared_tool_definitions(visible_names: &HashSet<String>) -> Vec<ToolDefinition> {
    shared_tool_schemas(|name| visible_names.contains(name))
        .into_iter()
        .map(definition_from_schema)
        .collect()
}

pub(crate) fn shared_tool_schemas(filter: impl Fn(&str) -> bool) -> Vec<pl_model::ToolSchema> {
    pl_core::shared_tool_schemas(pl_core::SharedToolSchemaOptions {
        bash: false,
        workspace_files: true,
        ask_user: true,
        subagents: true,
        git: true,
        container: true,
        mcp_resources: true,
        todo: true,
        plan_exit: false,
    })
    .into_iter()
    .filter(|schema| filter(tool_schema_name(schema)))
    .collect()
}

fn tool_schema_name(schema: &pl_model::ToolSchema) -> &str {
    match schema {
        pl_model::ToolSchema::Function { name, .. } => name,
        pl_model::ToolSchema::Custom { name, .. } => name,
    }
}

fn definition_from_schema(schema: pl_model::ToolSchema) -> ToolDefinition {
    match schema {
        pl_model::ToolSchema::Function {
            name,
            description,
            input_schema,
        } => ToolDefinition::function(name, description, input_schema),
        pl_model::ToolSchema::Custom { name, .. } => {
            panic!("shared tool {name} must be a function tool")
        }
    }
}

async fn project_tool_started(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    item: &TracePart,
) {
    let Some(tool) = &item.tool else {
        return;
    };
    let call_id = tool_call_id(tool);
    let arguments = arguments_value(&tool.arguments);
    let started_at = trace_time(item.created_at);
    super::persistence::record_tool_trace_started(
        runtime.deps.store.as_ref(),
        ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            call_id: call_id.clone(),
            tool_name: tool.name.clone(),
            arguments: arguments.clone(),
            output: String::new(),
            success: false,
            duration_ms: None,
            started_at: Some(started_at),
            completed_at: None,
            output_preview: String::new(),
            output_artifacts: Vec::new(),
        },
        started_at,
    )
    .await;
    super::persistence::record_agent_log(
        runtime.deps.store.as_ref(),
        AgentLogRecord {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: "info",
            category: "tool",
            message: "tool started",
            details: json!({
                "call_id": call_id,
                "tool_name": tool.name,
                "arguments_preview": super::tool_output::trace_preview_value(&arguments, 500),
            }),
        },
    )
    .await;
    runtime
        .events
        .publish(ServiceEventKind::ToolStarted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            call_id,
            tool_name: tool.name.clone(),
            arguments_preview: Some(super::tool_output::trace_preview_value(&arguments, 500)),
            arguments: Some(arguments),
        })
        .await;
}

async fn project_tool_completed(
    runtime: &AgentRuntime,
    agent_id: AgentId,
    session_id: SessionId,
    turn_id: TurnId,
    item: &TracePart,
    success: bool,
) {
    let Some(tool) = &item.tool else {
        return;
    };
    let call_id = tool_call_id(tool);
    let arguments = arguments_value(&tool.arguments);
    let output = tool.result.clone().unwrap_or_default();
    let output_preview = super::tool_output::trace_preview_output(&output, 500);
    let started_at = trace_time(item.created_at);
    let completed_at = trace_time(item.updated_at);
    let duration_ms = item
        .updated_at
        .saturating_sub(item.created_at)
        .try_into()
        .ok()
        .map(|seconds: u64| seconds.saturating_mul(1000));
    super::persistence::record_tool_trace_completed(
        runtime.deps.store.as_ref(),
        ToolTraceDetail {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            call_id: call_id.clone(),
            tool_name: tool.name.clone(),
            arguments,
            output: output.clone(),
            success,
            duration_ms,
            started_at: Some(started_at),
            completed_at: Some(completed_at),
            output_preview: output_preview.clone(),
            output_artifacts: Vec::new(),
        },
        started_at,
        completed_at,
    )
    .await;
    super::persistence::record_agent_log(
        runtime.deps.store.as_ref(),
        AgentLogRecord {
            agent_id,
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            level: if success { "info" } else { "warn" },
            category: "tool",
            message: "tool completed",
            details: json!({
                "call_id": call_id,
                "tool_name": tool.name,
                "success": success,
                "duration_ms": duration_ms,
                "output_preview": output_preview.as_str(),
            }),
        },
    )
    .await;
    runtime
        .events
        .publish(ServiceEventKind::ToolCompleted {
            agent_id,
            session_id: Some(session_id),
            turn_id,
            call_id,
            tool_name: tool.name.clone(),
            success,
            output_preview,
            duration_ms,
        })
        .await;
}

fn arguments_value(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments))
}

fn tool_call_id(tool: &pl_trace::TraceToolPart) -> String {
    tool.call_id
        .clone()
        .unwrap_or_else(|| tool.tool_call_id.clone())
}

fn trace_time(seconds: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(seconds, 0).unwrap_or_else(now)
}
