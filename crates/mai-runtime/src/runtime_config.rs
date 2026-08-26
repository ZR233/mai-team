use super::*;

impl AgentRuntime {
    pub fn subscribe(&self) -> broadcast::Receiver<MaiProductEventEnvelope> {
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
        let policy = self.mai_config.read().await.skills.clone();
        self.deps.skills.list(&config, &policy).await
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
        let policy = self.mai_config.read().await.skills.clone();
        self.deps.skills.list(&normalized, &policy).await
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
            config::preserve_provider_secrets(&config.models, &mut request);
            let roles = config::agent_config_from_models(&config.models);
            let models = config::model_config_from_api(&request, &roles)?;
            let mut next = config.clone();
            next.models = models;
            crate::config::save(&self.deps.store, &next).await?;
            *config = next;
        }
        self.reconcile_active_mcp_runtimes().await?;
        self.providers_response().await
    }

    /// 按 MaiConfig.models 解析 provider/model，供 agent facade 和 smoke test 共用。
    pub async fn resolve_provider_selection(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<pl_core::ResolvedModelRoute> {
        config::resolve_provider_model(
            &self.mai_config.read().await.models,
            provider_id,
            model,
            effort,
        )
    }

    pub(crate) async fn resolve_role_provider_selection(
        &self,
        role: AgentRole,
        requested_provider: Option<&str>,
        requested_model: Option<&str>,
    ) -> Result<pl_core::ResolvedModelRoute> {
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
        Ok(route)
    }
}
