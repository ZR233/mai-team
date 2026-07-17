use super::*;

impl AgentRuntime {
    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.events.subscribe()
    }

    pub async fn agent_config(&self) -> Result<AgentConfigResponse> {
        let config = config::agent_config_from_models(&self.mai_config.read().await.models);
        let planner = role_preference(&config, AgentRole::Planner).cloned();
        let explorer = role_preference(&config, AgentRole::Explorer).cloned();
        let executor = role_preference(&config, AgentRole::Executor).cloned();
        let reviewer = role_preference(&config, AgentRole::Reviewer).cloned();
        let mut validation_errors = Vec::new();
        let effective_planner = self
            .resolve_effective_agent_model(
                AgentRole::Planner,
                planner.as_ref(),
                &mut validation_errors,
            )
            .await;
        let effective_explorer = self
            .resolve_effective_agent_model(
                AgentRole::Explorer,
                explorer.as_ref(),
                &mut validation_errors,
            )
            .await;
        let effective_executor = self
            .resolve_effective_agent_model(
                AgentRole::Executor,
                executor.as_ref(),
                &mut validation_errors,
            )
            .await;
        let effective_reviewer = self
            .resolve_effective_agent_model(
                AgentRole::Reviewer,
                reviewer.as_ref(),
                &mut validation_errors,
            )
            .await;
        let validation_error =
            (!validation_errors.is_empty()).then(|| validation_errors.join("; "));
        Ok(AgentConfigResponse {
            planner,
            explorer,
            executor,
            reviewer,
            effective_planner,
            effective_explorer,
            effective_executor,
            effective_reviewer,
            validation_error,
        })
    }

    pub async fn list_skills(&self) -> Result<SkillsListResponse> {
        let config = self.deps.store.load_skills_config().await?;
        Ok(self.deps.skills.list(&config)?)
    }

    pub async fn list_agent_profiles(&self) -> Result<AgentProfilesResponse> {
        Ok(self.deps.agent_profiles.list())
    }

    pub async fn update_skills_config(
        &self,
        request: SkillsConfigRequest,
    ) -> Result<SkillsListResponse> {
        let normalized = crate::skills::normalize_config(&request)?;
        self.deps.store.save_skills_config(&normalized).await?;
        Ok(self.deps.skills.list(&normalized)?)
    }

    pub async fn list_project_skills(&self, project_id: ProjectId) -> Result<SkillsListResponse> {
        if !self.project_skill_cache_dir(project_id).exists() {
            return self.detect_project_skills(project_id).await;
        }
        self.project_skills_from_cache(project_id).await
    }

    pub async fn detect_project_skills(&self, project_id: ProjectId) -> Result<SkillsListResponse> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        if summary.status != ProjectStatus::Ready
            || summary.clone_status != ProjectCloneStatus::Ready
        {
            return Err(RuntimeError::InvalidInput(
                "project repository workspace is not ready".to_string(),
            ));
        }

        self.sync_project_repository(
            project_id,
            projects::workspace::ProjectRepositorySyncTarget::DefaultBranch,
        )
        .await?;
        self.refresh_project_skill_cache_from_project_repository(project_id)
            .await?;
        self.project_skills_from_cache(project_id).await
    }

    pub async fn update_agent_config(
        &self,
        request: AgentConfigRequest,
    ) -> Result<AgentConfigResponse> {
        {
            let mut config = self.mai_config.write().await;
            let providers = config::providers_request_from_models(&config.models);
            let models = config::model_config_from_api(&providers, &request)?;
            let mut next = config.clone();
            next.models = models;
            crate::config::save(&self.deps.store, &next).await?;
            *config = next;
        }
        self.refresh_agent_model_projections().await?;
        self.agent_config().await
    }

    /// 返回 MaiConfig.models 的外部 provider 投影，secret 只以 has_api_key 暴露。
    pub async fn providers_response(&self) -> Result<ProvidersResponse> {
        Ok(config::providers_response_from_models(
            &self.mai_config.read().await.models,
        ))
    }

    /// 用 provider DTO 替换 MaiConfig.models 的 provider catalog，并保留动态角色路由。
    pub async fn update_providers(
        &self,
        mut request: ProvidersConfigRequest,
    ) -> Result<ProvidersResponse> {
        {
            let mut config = self.mai_config.write().await;
            preserve_provider_secrets(&config.models, &mut request);
            let roles = config::agent_config_from_models(&config.models);
            let mut models = config::model_config_from_api(&request, &roles)?;
            config::preserve_provider_private_fields(&config.models, &request, &mut models);
            let mut next = config.clone();
            next.models = models;
            crate::config::save(&self.deps.store, &next).await?;
            *config = next;
        }
        self.refresh_agent_model_projections().await?;
        self.providers_response().await
    }

    /// 按 MaiConfig.models 解析 provider/model，供 agent facade 和 smoke test 共用。
    pub async fn resolve_provider_selection(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderSelection> {
        config::provider_selection_from_models(
            &self.mai_config.read().await.models,
            provider_id,
            model,
        )
    }

    pub(crate) async fn resolve_role_provider_selection(
        &self,
        role: AgentRole,
        requested_provider: Option<&str>,
        requested_model: Option<&str>,
    ) -> Result<ProviderSelection> {
        let models = self.mai_config.read().await.models.clone();
        let role_id = pl_core::AgentRoleId::new(role.to_string())?;
        let route = models.resolve(&role_id).map_err(RuntimeError::Model)?;
        if requested_provider
            .filter(|value| !value.trim().is_empty())
            .is_some_and(|provider| provider != route.provider_id.as_str())
            || requested_model
                .filter(|value| !value.trim().is_empty())
                .is_some_and(|model| model != route.model.slug)
        {
            return Err(RuntimeError::InvalidInput(format!(
                "agent model is configured by the `{role_id}` role route"
            )));
        }
        config::provider_selection_from_models(
            &models,
            Some(route.provider_id.as_str()),
            Some(&route.model.slug),
        )
    }

    async fn refresh_agent_model_projections(&self) -> Result<()> {
        let models = self.mai_config.read().await.models.clone();
        let agents = self
            .state
            .agents
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for agent in agents {
            let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
            let summary = {
                let mut summary = agent.summary.write().await;
                let role = summary.role.unwrap_or_default().to_string();
                let route = models
                    .resolve(&pl_core::AgentRoleId::new(role)?)
                    .map_err(RuntimeError::Model)?;
                summary.provider_id = route.provider_id.to_string();
                summary.provider_name = route.provider_info.name;
                summary.model = route.model.slug;
                summary.reasoning_effort = route
                    .reasoning_effort
                    .map(|effort| effort.as_str().to_string());
                summary.updated_at = now();
                summary.clone()
            };
            self.deps
                .store
                .save_agent_with_runtime_id(
                    &summary,
                    agent.system_prompt.as_deref(),
                    runtime_agent_id.as_str(),
                )
                .await?;
            self.events
                .publish(ServiceEventKind::AgentUpdated { agent: summary })
                .await;
        }
        Ok(())
    }
}

fn preserve_provider_secrets(
    current: &pl_core::AgentModelConfig,
    request: &mut ProvidersConfigRequest,
) {
    for provider in &mut request.providers {
        if provider
            .api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
        {
            continue;
        }
        provider.api_key = pl_core::ProviderId::new(provider.id.clone())
            .ok()
            .and_then(|id| current.providers.get(&id))
            .and_then(|provider| provider.bearer_token.clone());
    }
}
