use super::*;

impl AgentRuntime {
    pub(super) async fn set_agent_resource_state(
        &self,
        agent: &Arc<AgentRecord>,
        state: AgentResourceState,
        error: Option<String>,
    ) -> Result<()> {
        {
            let mut summary = agent.summary.write().await;
            summary.resource = AgentResourceSnapshot { state, error };
            summary.updated_at = now();
        }
        self.persist_agent(agent).await?;
        let summary = agent.summary.read().await.clone();
        self.events
            .publish(MaiProductEventKind::AgentUpdated { agent: summary })
            .await;
        Ok(())
    }

    pub(super) async fn persist_agent(&self, agent: &AgentRecord) -> Result<()> {
        let summary = agent.summary.read().await.clone();
        self.deps
            .store
            .save_agent(&summary, agent.system_prompt.as_deref())
            .await?;
        Ok(())
    }

    pub(super) async fn task(&self, task_id: TaskId) -> Result<Arc<TaskRecord>> {
        tasks::task(&self.state, task_id).await
    }

    pub(super) async fn project(&self, project_id: ProjectId) -> Result<Arc<ProjectRecord>> {
        projects::service::project(&self.state, project_id).await
    }

    pub(super) async fn project_skill_lock(&self, project_id: ProjectId) -> Arc<RwLock<()>> {
        self.state
            .project_skill_locks
            .write()
            .await
            .entry(project_id)
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }

    pub(super) async fn project_auto_reviewer_agents(
        &self,
        project_id: ProjectId,
    ) -> Vec<AgentSummary> {
        projects::service::project_auto_reviewer_agents(&self.state, project_id).await
    }

    pub(super) async fn project_skills_from_cache(
        &self,
        project_id: ProjectId,
    ) -> Result<SkillsListResponse> {
        let lock = self.project_skill_lock(project_id).await;
        let policy = self.mai_config.read().await.skills.clone();
        projects::skills::list_from_cache(
            &self.deps.store,
            &self.cache_root,
            &lock,
            project_id,
            &policy,
        )
        .await
    }

    pub(super) fn skill_catalog_with_project_roots(
        &self,
        project_id: ProjectId,
    ) -> SkillCatalogService {
        self.deps
            .skills
            .clone_with_extra_roots(self.project_skill_roots(project_id))
    }

    pub(super) async fn skill_catalog_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> Result<SkillCatalogService> {
        if let Some(context) = agent.review_context.read().await.as_ref() {
            return Ok(self
                .deps
                .skills
                .clone_with_extra_roots(projects::skills::roots(&context.skills_cache_dir)));
        }
        let project_id = agent.summary.read().await.project_id;
        Ok(project_id
            .map(|project_id| self.skill_catalog_with_project_roots(project_id))
            .unwrap_or_else(|| self.deps.skills.clone()))
    }

    pub(super) async fn project_skill_read_guard(
        &self,
        agent: &AgentRecord,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        if agent.review_context.read().await.is_some() {
            return None;
        }
        let project_id = agent.summary.read().await.project_id?;
        let lock = self.project_skill_lock(project_id).await;
        Some(lock.read_owned().await)
    }

    pub(super) async fn refresh_project_skills_for_agent(&self, agent: &AgentRecord) -> Result<()> {
        let summary = agent.summary.read().await;
        if summary.role == Some(AgentRole::Reviewer) {
            return Ok(());
        }
        let Some(project_id) = summary.project_id else {
            return Ok(());
        };
        drop(summary);
        self.refresh_project_skills_from_project_sidecar_if_ready(project_id)
            .await
    }

    pub(super) async fn sync_agent_skills_to_container(
        &self,
        agent: &Arc<AgentRecord>,
        response: &SkillsListResponse,
    ) -> Result<()> {
        let agent_id = agent.summary.read().await.id;
        let container_id = self.container_id(agent_id).await?;
        let skills = response
            .skills
            .iter()
            .filter(|skill| {
                skill.enabled
                    && matches!(skill.scope, SkillScope::System | SkillScope::Project)
                    && skill.path.parent().is_some()
            })
            .collect::<Vec<_>>();
        if skills.is_empty() {
            return Ok(());
        }

        let cleanup = self
            .deps
            .docker
            .exec_shell(
                &container_id,
                &format!(
                    "rm -rf {root} && mkdir -p {root}",
                    root = shell_quote_word(CONTAINER_SKILLS_ROOT)
                ),
                Some("/"),
                Some(10),
            )
            .await
            .map_err(|err| {
                RuntimeError::InvalidInput(format!(
                    "failed to sync skills into agent container: {err}"
                ))
            })?;
        if cleanup.status != 0 {
            let message = preview(
                format!("{}\n{}", cleanup.stderr, cleanup.stdout).trim(),
                500,
            );
            return Err(RuntimeError::InvalidInput(format!(
                "failed to sync skills into agent container: {message}"
            )));
        }

        let mut copied_dirs = HashSet::new();
        for skill in skills {
            let Some(skill_dir) = skill.path.parent() else {
                continue;
            };
            let container_dir = instructions::container_skill_dir(skill);
            if copied_dirs.insert(container_dir.clone()) {
                self.deps
                    .docker
                    .copy_to_container(&container_id, skill_dir, &container_dir.to_string_lossy())
                    .await
                    .map_err(|err| {
                        RuntimeError::InvalidInput(format!(
                            "failed to sync skills into agent container: {err}"
                        ))
                    })?;
            }
        }
        Ok(())
    }

    pub(super) async fn refresh_project_skills_from_project_sidecar_if_ready(
        &self,
        project_id: ProjectId,
    ) -> Result<()> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        if summary.status != ProjectStatus::Ready
            || summary.clone_status != ProjectCloneStatus::Ready
        {
            return Ok(());
        }
        self.refresh_project_skill_cache_from_project_repository(project_id)
            .await
    }

    pub(super) async fn project_review_workspace_instructions_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> Result<Option<String>> {
        let summary = agent.summary.read().await;
        if summary.role != Some(AgentRole::Reviewer) {
            return Ok(None);
        }
        drop(summary);
        Ok(agent
            .review_context
            .read()
            .await
            .as_ref()
            .map(|context| context.workspace_instructions.clone()))
    }

    pub(super) fn project_skill_cache_dir(&self, project_id: ProjectId) -> PathBuf {
        projects::skills::cache_dir(&self.cache_root, project_id)
    }

    pub(super) fn project_skill_roots(&self, project_id: ProjectId) -> Vec<(PathBuf, SkillScope)> {
        projects::skills::roots_for_project(&self.cache_root, project_id)
    }

    pub(super) fn apply_project_skill_source_paths(
        &self,
        project_id: ProjectId,
        response: &mut SkillsListResponse,
    ) {
        projects::skills::apply_project_source_paths(&self.cache_root, project_id, response);
    }

    pub(super) async fn apply_project_skill_source_paths_for_agent(
        &self,
        agent: &AgentRecord,
        project_id: ProjectId,
        response: &mut SkillsListResponse,
    ) {
        if let Some(context) = agent.review_context.read().await.as_ref() {
            projects::skills::apply_source_paths_with_workspace_root(
                &context.skills_cache_dir,
                &context.repository_view.container_path,
                response,
            );
        } else {
            self.apply_project_skill_source_paths(project_id, response);
        }
    }

    pub(super) async fn refresh_project_skill_cache(
        &self,
        project_id: ProjectId,
        sources: &[ProjectSkillSourceDir],
    ) -> Result<()> {
        let lock = self.project_skill_lock(project_id).await;
        projects::skills::refresh_cache(&self.cache_root, &lock, project_id, sources).await
    }

    pub(super) async fn refresh_project_skill_cache_from_project_repository(
        &self,
        project_id: ProjectId,
    ) -> Result<()> {
        let stage_root = self
            .cache_root
            .join("project-skill-stage")
            .join(project_id.to_string())
            .join(Uuid::new_v4().to_string());
        let result = async {
            let repository_volume = project_cache_volume(&project_id.to_string());
            let sources = self
                .project_skill_sources_from_workspace_volume(
                    project_id,
                    &repository_volume,
                    projects::mcp::PROJECT_WORKSPACE_PATH,
                    &stage_root,
                )
                .await?;
            self.refresh_project_skill_cache(project_id, &sources).await
        }
        .await;
        let _ = std::fs::remove_dir_all(&stage_root);
        result
    }

    pub(super) async fn project_skill_sources_from_workspace_volume(
        &self,
        project_id: ProjectId,
        workspace_volume: &str,
        workspace_root: &str,
        stage_root: &Path,
    ) -> Result<Vec<ProjectSkillSourceDir>> {
        let command = projects::skills::detect_existing_dirs_command(workspace_root);
        let sidecar_name = format!("mai-tool-skill-detect-{project_id}-{}", Uuid::new_v4());
        let output = self
            .deps
            .docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: None,
                env: &[],
                workspace_volume: Some(workspace_volume),
                mounts: &[],
                timeout_secs: Some(120),
            })
            .await?;
        if output.status != 0 {
            let message = preview(format!("{}{}", output.stdout, output.stderr).trim(), 500);
            return Err(RuntimeError::InvalidInput(format!(
                "project skill discovery sidecar failed: {message}"
            )));
        }

        let mut sources =
            projects::skills::detected_dirs_from_stdout(workspace_root, &output.stdout)?;
        if sources.is_empty() {
            return Ok(sources);
        }

        std::fs::create_dir_all(stage_root)?;
        let repository_stage = stage_root.join("repository");
        let _ = std::fs::remove_dir_all(&repository_stage);
        let relative_paths = sources
            .iter()
            .map(|source| source.relative_path.clone())
            .collect::<Vec<_>>();
        let export_name = format!("mai-tool-skill-export-{project_id}-{}", Uuid::new_v4());
        self.deps
            .docker
            .export_workspace_paths(&WorkspaceExportRequest {
                name: &export_name,
                image: &self.sidecar_image,
                workspace_volume,
                workspace_root,
                relative_paths: &relative_paths,
                host_path: &repository_stage,
            })
            .await
            .map_err(|err| {
                RuntimeError::InvalidInput(format!(
                    "failed to export project skill sources from workspace volume: {err}"
                ))
            })?;
        for source in &mut sources {
            source.host_path = Some(repository_stage.join(&source.relative_path));
            source.host_repository_root = Some(repository_stage.clone());
        }
        Ok(sources)
    }

    pub(super) async fn project_instruction_sources_from_workspace_volume(
        &self,
        project_id: ProjectId,
        workspace_volume: &str,
        workspace_root: &str,
        stage_root: &Path,
    ) -> Result<Vec<ProjectInstructionSourceFile>> {
        let command = projects::instructions::detect_existing_files_command(workspace_root);
        let sidecar_name = format!(
            "mai-tool-instruction-detect-{project_id}-{}",
            Uuid::new_v4()
        );
        let output = self
            .deps
            .docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: None,
                env: &[],
                workspace_volume: Some(workspace_volume),
                mounts: &[],
                timeout_secs: Some(120),
            })
            .await?;
        if output.status != 0 {
            let message = preview(format!("{}{}", output.stdout, output.stderr).trim(), 500);
            return Err(RuntimeError::InvalidInput(format!(
                "project instruction discovery sidecar failed: {message}"
            )));
        }

        let mut sources =
            projects::instructions::detected_files_from_stdout(workspace_root, &output.stdout)?;
        if sources.is_empty() {
            return Ok(sources);
        }

        std::fs::create_dir_all(stage_root)?;
        for source in &mut sources {
            let target = stage_root.join(&source.file_name);
            let copy_name = format!("mai-tool-instruction-copy-{project_id}-{}", Uuid::new_v4());
            self.deps
                .docker
                .copy_from_workspace_volume_to_file(
                    &copy_name,
                    &self.sidecar_image,
                    workspace_volume,
                    &source.container_path,
                    &target,
                )
                .await
                .map_err(|err| {
                    RuntimeError::InvalidInput(format!(
                        "failed to copy project instruction source from workspace volume: {err}"
                    ))
                })?;
            source.host_path = Some(target);
        }
        Ok(sources)
    }
}
