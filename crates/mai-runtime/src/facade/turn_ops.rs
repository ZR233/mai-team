use std::sync::Arc;
use std::time::Duration;

use mai_protocol::*;
use mai_skills::{SkillInjections, SkillsManager};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::instructions::ContainerSkillPaths;
use crate::state::AgentRecord;
use crate::turn::tools::ToolExecution;
use crate::{AgentRuntime, ProjectReviewQueueRequest, Result, RuntimeError, agents, turn};

impl turn::orchestrator::TurnOrchestratorOps for Arc<AgentRuntime> {
    async fn agent(&self, agent_id: AgentId) -> Result<Arc<AgentRecord>> {
        AgentRuntime::agent(self.as_ref(), agent_id).await
    }

    async fn ensure_agent_container_for_turn(
        &self,
        agent: &Arc<AgentRecord>,
        status: AgentStatus,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        agents::ensure_agent_container_for_turn(
            self.as_ref(),
            agent,
            status,
            turn_id,
            cancellation_token,
        )
        .await
        .map(|_| ())
    }

    async fn refresh_project_skills_for_agent(&self, agent: &AgentRecord) -> Result<()> {
        AgentRuntime::refresh_project_skills_for_agent(self.as_ref(), agent).await
    }

    async fn skills_manager_for_agent(&self, agent: &AgentRecord) -> Result<SkillsManager> {
        AgentRuntime::skills_manager_for_agent(self.as_ref(), agent).await
    }

    async fn sync_agent_skills_to_container(
        &self,
        agent: &Arc<AgentRecord>,
        skills_manager: &SkillsManager,
        skills_config: &SkillsConfigRequest,
    ) -> Result<ContainerSkillPaths> {
        AgentRuntime::sync_agent_skills_to_container(
            self.as_ref(),
            agent,
            skills_manager,
            skills_config,
        )
        .await
    }

    async fn maybe_auto_compact(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        session_id: SessionId,
        turn_id: TurnId,
        request: turn::context::ContextCompactionRequest,
        cancellation_token: &CancellationToken,
    ) -> Result<turn::context::ContextCompactionOutcome> {
        AgentRuntime::maybe_auto_compact(
            self,
            agent,
            agent_id,
            session_id,
            turn_id,
            request,
            cancellation_token,
        )
        .await
    }

    async fn agent_mcp_tools(&self, agent: &AgentRecord) -> Vec<mai_mcp::McpTool> {
        AgentRuntime::agent_mcp_tools(self.as_ref(), agent).await
    }

    async fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        AgentRuntime::project_skill_read_guard(self.as_ref(), agent).await
    }

    async fn inject_project_mcp_tools(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        session_id: SessionId,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        AgentRuntime::inject_project_mcp_tools(
            self.as_ref(),
            agent,
            agent_id,
            session_id,
            cancellation_token,
        )
        .await
    }

    async fn build_instructions(
        &self,
        agent: &AgentRecord,
        skills_manager: &SkillsManager,
        skill_injections: &SkillInjections,
        skills_config: &SkillsConfigRequest,
        mcp_tools: &[mai_mcp::McpTool],
        container_skill_paths: &ContainerSkillPaths,
    ) -> Result<String> {
        AgentRuntime::build_instructions(
            self.as_ref(),
            agent,
            skills_manager,
            skill_injections,
            skills_config,
            mcp_tools,
            container_skill_paths,
        )
        .await
    }

    async fn set_turn_status(
        &self,
        agent: &Arc<AgentRecord>,
        turn_id: TurnId,
        cancellation_token: &CancellationToken,
        enforce_current_turn: bool,
        status: AgentStatus,
    ) -> Result<()> {
        AgentRuntime::set_turn_status(
            self.as_ref(),
            agent,
            turn_id,
            cancellation_token,
            enforce_current_turn,
            status,
        )
        .await
    }

    async fn execute_tool(
        &self,
        agent: &Arc<AgentRecord>,
        agent_id: AgentId,
        turn_id: TurnId,
        name: &str,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        AgentRuntime::execute_tool(
            self,
            agent,
            agent_id,
            turn_id,
            name,
            arguments,
            cancellation_token,
        )
        .await
    }

    async fn start_next_queued_input_after_turn(&self, agent_id: AgentId) {
        AgentRuntime::start_next_queued_input_after_turn(self, agent_id).await;
    }
}

impl turn::tools::ContainerToolOps for Arc<AgentRuntime> {
    async fn container_id(&self, agent_id: AgentId) -> Result<String> {
        AgentRuntime::container_id(self.as_ref(), agent_id).await
    }
}

impl turn::tools::ToolDispatchOps for Arc<AgentRuntime> {
    async fn spawn_agent_from_tool(
        &self,
        parent_agent_id: AgentId,
        request: turn::tools::SpawnAgentToolRequest,
    ) -> Result<turn::tools::SpawnAgentToolResult> {
        let result = agents::spawn_child_agent(
            self,
            parent_agent_id,
            agents::SpawnChildAgentRequest {
                name: request.name,
                role: request.role,
                model: request.model,
                reasoning_effort: request.reasoning_effort,
                use_role_model: request.legacy_role.is_some(),
                fork_context: request.fork_context,
                collab_input: request.collab_input,
            },
        )
        .await?;
        Ok(turn::tools::SpawnAgentToolResult {
            agent: result.agent,
            turn_id: result.turn_id,
        })
    }

    async fn send_input_to_agent(
        &self,
        target: AgentId,
        session_id: Option<SessionId>,
        message: String,
        skill_mentions: Vec<String>,
        interrupt: bool,
    ) -> Result<Value> {
        AgentRuntime::send_input_to_agent(
            self,
            target,
            session_id,
            message,
            skill_mentions,
            interrupt,
        )
        .await
    }

    async fn wait_agents_output_with_cancel(
        &self,
        agent_ids: Vec<AgentId>,
        timeout: Duration,
        cancellation_token: &CancellationToken,
    ) -> Result<Value> {
        AgentRuntime::wait_agents_output_with_cancel(
            self.as_ref(),
            agent_ids,
            timeout,
            cancellation_token,
        )
        .await
    }

    async fn list_agents(&self) -> Vec<AgentSummary> {
        AgentRuntime::list_agents(self.as_ref()).await
    }

    async fn close_agent(&self, agent_id: AgentId) -> Result<AgentStatus> {
        AgentRuntime::close_agent(self.as_ref(), agent_id).await
    }

    async fn resume_agent(&self, agent_id: AgentId) -> Result<AgentSummary> {
        AgentRuntime::resume_agent(self.as_ref(), agent_id).await
    }

    async fn list_mcp_resources(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> Result<Value> {
        let broker = self
            .agent_resource_broker(agent, agent_id, cancellation_token)
            .await?;
        broker.list_resources(server.as_deref(), cursor).await
    }

    async fn list_mcp_resource_templates(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
        server: Option<String>,
        cursor: Option<String>,
    ) -> Result<Value> {
        let broker = self
            .agent_resource_broker(agent, agent_id, cancellation_token)
            .await?;
        broker
            .list_resource_templates(server.as_deref(), cursor)
            .await
    }

    async fn read_mcp_resource(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
        server: String,
        uri: String,
    ) -> Result<Value> {
        let broker = self
            .agent_resource_broker(agent, agent_id, cancellation_token)
            .await?;
        broker.read_resource(&server, &uri).await
    }

    async fn save_task_plan(
        &self,
        agent_id: AgentId,
        title: String,
        markdown: String,
    ) -> Result<TaskSummary> {
        AgentRuntime::save_task_plan(self, agent_id, title, markdown).await
    }

    async fn submit_review_result(
        &self,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> Result<TaskReview> {
        AgentRuntime::submit_review_result(self, agent_id, passed, findings, summary).await
    }

    async fn save_artifact(
        &self,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> Result<ArtifactInfo> {
        AgentRuntime::save_artifact(self, agent_id, path, display_name).await
    }

    async fn execute_project_github_api_get(
        &self,
        agent: &AgentRecord,
        path: String,
    ) -> Result<ToolExecution> {
        AgentRuntime::execute_project_github_api_get(self.as_ref(), agent, &path).await
    }

    async fn execute_project_github_api_request(
        &self,
        agent: &AgentRecord,
        request: turn::tools::GithubApiRequest,
    ) -> Result<ToolExecution> {
        AgentRuntime::execute_project_github_api_request(self.as_ref(), agent, &request).await
    }

    async fn queue_project_review_prs(
        &self,
        agent: &AgentRecord,
        prs: Vec<turn::tools::QueueProjectReviewPr>,
    ) -> Result<ToolExecution> {
        let agent_summary = agent.summary.read().await.clone();
        let project_id = agent_summary.project_id.ok_or_else(|| {
            RuntimeError::InvalidInput(
                "queue_project_review_prs is only available to project agents".to_string(),
            )
        })?;
        if !matches!(
            agent_summary.role,
            Some(AgentRole::Explorer | AgentRole::Reviewer)
        ) {
            return Err(RuntimeError::InvalidInput(
                "queue_project_review_prs is only available to project selector and reviewer agents"
                    .to_string(),
            ));
        }

        let mut queued = Vec::new();
        let mut deduped = Vec::new();
        let mut ignored = Vec::new();
        for pr in prs {
            let reason = pr
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("selector")
                .to_string();
            let summary = self
                .enqueue_project_review(ProjectReviewQueueRequest {
                    project_id,
                    pr: pr.number,
                    head_sha: pr.head_sha,
                    delivery_id: None,
                    reason,
                })
                .await?;
            queued.extend(summary.queued);
            deduped.extend(summary.deduped);
            ignored.extend(summary.ignored);
        }
        Ok(ToolExecution::new(
            true,
            json!({
                "queued": queued,
                "deduped": deduped,
                "ignored": ignored,
            })
            .to_string(),
            false,
        ))
    }

    async fn execute_project_git_tool(
        &self,
        agent: &AgentRecord,
        name: String,
        arguments: Value,
    ) -> Result<ToolExecution> {
        AgentRuntime::execute_project_git_tool(self.as_ref(), agent, &name, arguments).await
    }

    async fn execute_mcp_tool(
        &self,
        agent: &AgentRecord,
        model_name: String,
        arguments: Value,
        cancellation_token: CancellationToken,
    ) -> Result<ToolExecution> {
        if agent.summary.read().await.project_id.is_some() {
            return AgentRuntime::execute_project_mcp_tool(
                self.as_ref(),
                agent,
                &model_name,
                arguments,
                cancellation_token,
            )
            .await;
        }
        let manager =
            agent.mcp.read().await.clone().ok_or_else(|| {
                RuntimeError::InvalidInput("MCP manager not initialized".to_string())
            })?;
        let output = tokio::select! {
            output = manager.call_model_tool(&model_name, arguments) => output?,
            _ = cancellation_token.cancelled() => {
                return Err(RuntimeError::TurnCancelled);
            }
        };
        Ok(ToolExecution::new(true, output.to_string(), false))
    }
}
