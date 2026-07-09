use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentLogEntry, AgentLogsResponse, SessionId, ToolOutputArtifactInfo, ToolTraceDetail,
    ToolTraceListResponse, ToolTraceSummary,
};
use mai_store::{AgentLogFilter, ToolTraceFilter};
use pl_protocol::Message as ModelMessage;

use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

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

    fn load_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> impl Future<Output = Result<Vec<ModelMessage>>> + Send;

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
    let session_id = {
        let sessions = agent.sessions.lock().await;
        let selected_session = super::selected_session(&sessions, session_id).ok_or_else(|| {
            RuntimeError::SessionNotFound {
                agent_id,
                session_id: session_id.unwrap_or_default(),
            }
        })?;
        selected_session.summary.id
    };
    let history = ops.load_agent_history(agent_id, session_id).await?;
    let projection =
        pl_core::tool_history_projection(&history, &call_id, 500).ok_or_else(|| {
            RuntimeError::ToolTraceNotFound {
                agent_id,
                call_id: call_id.clone(),
            }
        })?;
    let (event_success, duration_ms) = ops
        .tool_metadata(agent_id, session_id, call_id.clone())
        .await;
    let success = event_success.unwrap_or_else(|| projection.inferred_success());
    Ok(ToolTraceDetail {
        agent_id,
        session_id: Some(session_id),
        turn_id: None,
        call_id,
        tool_name: projection.tool_name().to_string(),
        arguments: projection.arguments().clone(),
        success,
        output_preview: projection.output_preview().to_string(),
        output: projection.output().to_string(),
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
