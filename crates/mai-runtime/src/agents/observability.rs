use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentLogEntry, AgentLogsResponse, ModelInputItem, SessionId, ToolOutputArtifactInfo,
    ToolTraceDetail, ToolTraceListResponse, ToolTraceSummary,
};
use mai_store::{AgentLogFilter, ToolTraceFilter};
use serde_json::{Value, json};

use crate::state::AgentRecord;
use crate::{Result, RuntimeError, turn};

/// Provides persisted logs, tool trace records, recent event metadata, and
/// agent history access needed by read-only observability APIs.
pub(crate) trait AgentObservabilityOps: Send + Sync {
    fn agent(&self, agent_id: AgentId) -> impl Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn load_tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: String,
    ) -> impl Future<Output = Result<Option<ToolTraceDetail>>> + Send;

    fn tool_metadata(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        call_id: String,
    ) -> impl Future<Output = (Option<bool>, Option<u64>)> + Send;

    fn list_agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> impl Future<Output = Result<Vec<AgentLogEntry>>> + Send;

    fn list_tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> impl Future<Output = Result<Vec<ToolTraceSummary>>> + Send;

    fn tool_output_artifact_file_path(
        &self,
        agent_id: AgentId,
        call_id: &str,
        artifact_id: &str,
        name: &str,
    ) -> PathBuf;
}

pub(crate) async fn tool_trace(
    ops: &impl AgentObservabilityOps,
    agent_id: AgentId,
    session_id: Option<SessionId>,
    call_id: String,
) -> Result<ToolTraceDetail> {
    if let Some(trace) = ops
        .load_tool_trace(agent_id, session_id, call_id.clone())
        .await?
    {
        return Ok(trace);
    }

    let agent = ops.agent(agent_id).await?;
    let (session_id, history) = {
        let sessions = agent.sessions.lock().await;
        let selected_session = super::selected_session(&sessions, session_id).ok_or_else(|| {
            RuntimeError::SessionNotFound {
                agent_id,
                session_id: session_id.unwrap_or_default(),
            }
        })?;
        (
            selected_session.summary.id,
            selected_session.history.clone(),
        )
    };
    let mut tool_name = None;
    let mut arguments = None;
    let mut output = None;

    for item in history {
        match item {
            ModelInputItem::FunctionCall {
                call_id: item_call_id,
                name,
                arguments: raw_arguments,
            } if item_call_id == call_id => {
                tool_name = Some(name);
                arguments = Some(parse_tool_arguments(&raw_arguments));
            }
            ModelInputItem::FunctionCallOutput {
                call_id: item_call_id,
                output: item_output,
            } if item_call_id == call_id => {
                output = Some(item_output);
            }
            ModelInputItem::Message { .. }
            | ModelInputItem::Reasoning { .. }
            | ModelInputItem::FunctionCall { .. } => {}
            ModelInputItem::FunctionCallOutput { .. } => {}
        }
    }

    let tool_name = tool_name.ok_or_else(|| RuntimeError::ToolTraceNotFound {
        agent_id,
        call_id: call_id.clone(),
    })?;
    let output = output.unwrap_or_default();
    let (event_success, duration_ms) = ops
        .tool_metadata(agent_id, session_id, call_id.clone())
        .await;
    Ok(ToolTraceDetail {
        agent_id,
        session_id: Some(session_id),
        turn_id: None,
        call_id,
        tool_name,
        arguments: arguments.unwrap_or_else(|| json!({})),
        success: event_success.unwrap_or(!output.is_empty()),
        output_preview: turn::tools::trace_preview_output(&output, 500),
        output,
        duration_ms,
        started_at: None,
        completed_at: None,
        output_artifacts: Vec::new(),
    })
}

pub(crate) async fn tool_output_artifact(
    ops: &impl AgentObservabilityOps,
    agent_id: AgentId,
    session_id: Option<SessionId>,
    call_id: String,
    artifact_id: String,
) -> Result<(ToolOutputArtifactInfo, PathBuf)> {
    let trace = tool_trace(ops, agent_id, session_id, call_id.clone()).await?;
    let artifact = trace
        .output_artifacts
        .into_iter()
        .find(|artifact| artifact.id == artifact_id && artifact.call_id == call_id)
        .ok_or_else(|| RuntimeError::InvalidInput("tool output artifact not found".to_string()))?;
    let path = ops.tool_output_artifact_file_path(
        artifact.agent_id,
        &artifact.call_id,
        &artifact.id,
        &artifact.name,
    );
    Ok((artifact, path))
}

pub(crate) async fn agent_logs(
    ops: &impl AgentObservabilityOps,
    agent_id: AgentId,
    filter: AgentLogFilter,
) -> Result<AgentLogsResponse> {
    ops.agent(agent_id).await?;
    Ok(AgentLogsResponse {
        logs: ops.list_agent_logs(agent_id, filter).await?,
    })
}

pub(crate) async fn tool_traces(
    ops: &impl AgentObservabilityOps,
    agent_id: AgentId,
    filter: ToolTraceFilter,
) -> Result<ToolTraceListResponse> {
    ops.agent(agent_id).await?;
    Ok(ToolTraceListResponse {
        tool_calls: ops.list_tool_traces(agent_id, filter).await?,
    })
}

fn parse_tool_arguments(raw_arguments: &str) -> Value {
    serde_json::from_str(raw_arguments).unwrap_or_else(|_| json!({ "raw": raw_arguments }))
}
