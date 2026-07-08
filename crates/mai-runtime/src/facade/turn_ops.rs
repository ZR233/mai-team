use std::sync::Arc;

use mai_protocol::*;
use mai_skills::{SkillInjections, SkillsManager};
use tokio_util::sync::CancellationToken;

use crate::instructions::ContainerSkillPaths;
use crate::state::AgentRecord;
use crate::{AgentRuntime, Result, agents, turn};

impl turn::orchestrator::TurnOrchestratorOps for Arc<AgentRuntime> {
    fn runtime_handle(&self) -> Arc<AgentRuntime> {
        self.clone()
    }

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

    async fn start_next_queued_input_after_turn(&self, agent_id: AgentId) {
        AgentRuntime::start_next_queued_input_after_turn(self, agent_id).await;
    }
}
