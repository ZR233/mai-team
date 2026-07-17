use super::*;

impl AgentRuntime {
    pub(super) async fn start_environment_container_in_background(
        self: &Arc<Self>,
        agent: Arc<AgentRecord>,
    ) {
        if let Err(err) = self
            .set_agent_resource_state(&agent, AgentResourceState::Provisioning, None)
            .await
        {
            tracing::warn!("failed to persist environment container startup status: {err}");
            return;
        }
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime.start_environment_container(agent).await {
                tracing::warn!("failed to start environment container: {err}");
            }
        });
    }

    pub(super) async fn start_environment_container(
        self: Arc<Self>,
        agent: Arc<AgentRecord>,
    ) -> Result<()> {
        match agents::ensure_agent_container_with_source(
            self.as_ref(),
            &agent,
            &agents::ContainerSource::FreshImage,
        )
        .await
        {
            Ok(_) => Ok(agent.summary.read().await.clone()),
            Err(err) => {
                let message = err.to_string();
                let agent_id = agent.summary.read().await.id;
                if let Err(store_err) = self
                    .set_agent_resource_state(
                        &agent,
                        AgentResourceState::Failed,
                        Some(message.clone()),
                    )
                    .await
                {
                    tracing::warn!("failed to persist environment root agent failure: {store_err}");
                }
                self.events
                    .publish(ServiceEventKind::Error {
                        agent_id: Some(agent_id),
                        session_id: None,
                        turn_id: None,
                        message,
                    })
                    .await;
                Err(err)
            }
        }
        .map(|_| ())
    }

    pub(super) async fn create_unconfigured_environment_root_agent(
        &self,
        request: tasks::CreateEnvironmentRootAgentRequest,
    ) -> Result<Arc<AgentRecord>> {
        let id = Uuid::new_v4();
        let created_at = now();
        let docker_image = request
            .docker_image
            .as_deref()
            .map(str::trim)
            .filter(|image| !image.is_empty())
            .unwrap_or_else(|| self.deps.docker.image())
            .to_string();
        let summary = AgentSummary {
            id,
            parent_id: None,
            task_id: Some(request.environment_id),
            project_id: None,
            role: Some(AgentRole::Planner),
            name: request.name,
            state: mai_protocol::AgentState::default(),
            container_id: None,
            docker_image,
            provider_id: UNCONFIGURED_PROVIDER_ID.to_string(),
            provider_name: UNCONFIGURED_PROVIDER_NAME.to_string(),
            model: UNCONFIGURED_MODEL_ID.to_string(),
            reasoning_effort: None,
            created_at,
            updated_at: created_at,
            token_usage: TokenUsage::default(),
        };
        self.deps.store.save_agent(&summary, None).await?;
        let agent = Arc::new(AgentRecord {
            runtime_agent_id: RwLock::new(pl_core::AgentId::new(id.to_string())?),
            summary: RwLock::new(summary.clone()),
            container: RwLock::new(None),
            mcp: RwLock::new(None),
            review_context: RwLock::new(None),
            system_prompt: None,
        });
        self.state
            .agents
            .write()
            .await
            .insert(id, Arc::clone(&agent));
        self.events
            .publish(ServiceEventKind::AgentCreated { agent: summary })
            .await;
        Ok(agent)
    }
}
