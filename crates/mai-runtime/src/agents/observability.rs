use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentLogEntry, AgentLogsResponse, ToolOutputArtifactInfo, ToolTraceDetail,
    ToolTraceListResponse, ToolTraceSummary,
};
use mai_store::{AgentLogFilter, ToolTraceFilter};

use crate::state::AgentRecord;
use crate::{Result, RuntimeError};

/// Provides persisted logs, tool trace records, recent event metadata, and
/// agent history access needed by read-only observability APIs.
pub(crate) trait AgentObservabilityOps: Send + Sync {
    fn agent(&self, agent_id: AgentId) -> impl Future<Output = Result<Arc<AgentRecord>>> + Send;

    fn load_tool_trace(
        &self,
        agent_id: AgentId,
        call_id: String,
    ) -> impl Future<Output = Result<Option<ToolTraceDetail>>> + Send;

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
    call_id: String,
) -> Result<ToolTraceDetail> {
    ops.load_tool_trace(agent_id, call_id.clone())
        .await?
        .ok_or(RuntimeError::ToolTraceNotFound { agent_id, call_id })
}

pub(crate) async fn tool_output_artifact(
    ops: &impl AgentObservabilityOps,
    agent_id: AgentId,
    call_id: String,
    artifact_id: String,
) -> Result<(ToolOutputArtifactInfo, PathBuf)> {
    let trace = tool_trace(ops, agent_id, call_id.clone()).await?;
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
    if !tokio::fs::try_exists(&path).await? {
        return Err(RuntimeError::InvalidInput(
            "tool output artifact has expired".to_string(),
        ));
    }
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
