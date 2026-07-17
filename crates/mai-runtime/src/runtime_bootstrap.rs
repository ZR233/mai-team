use super::*;

impl AgentRuntime {
    pub async fn new(
        docker: DockerClient,
        store: Arc<MaiStore>,
        config: RuntimeConfig,
    ) -> Result<Arc<Self>> {
        Self::new_with_github_backend(docker, store, config, None).await
    }

    pub async fn new_with_github_backend(
        docker: DockerClient,
        store: Arc<MaiStore>,
        config: RuntimeConfig,
        github_backend: Option<Arc<dyn GithubAppBackend>>,
    ) -> Result<Arc<Self>> {
        let skills = SkillsManager::new_with_system_root(
            &config.repo_root,
            config.system_skills_root.as_ref(),
        );
        let agent_profiles = AgentProfilesManager::new_with_system_root(
            &config.repo_root,
            config.system_agents_root.as_ref(),
        );
        let snapshot = store.load_runtime_snapshot(RECENT_EVENT_LIMIT).await?;
        let mut agents = HashMap::new();
        for persisted in snapshot.agents {
            let summary = persisted.summary;
            let agent = Arc::new(AgentRecord {
                runtime_agent_id: RwLock::new(
                    pl_core::AgentId::new(persisted.runtime_agent_id)
                        .map_err(RuntimeError::Model)?,
                ),
                summary: RwLock::new(summary.clone()),
                container: RwLock::new(None),
                mcp: RwLock::new(None),
                review_context: RwLock::new(None),
                system_prompt: persisted.system_prompt,
            });
            agents.insert(summary.id, agent);
        }
        let mut tasks = HashMap::new();
        for persisted in snapshot.tasks {
            let mut summary = persisted.summary;
            let mut agent_count = 0;
            for agent in agents.values() {
                if agent.summary.read().await.task_id == Some(summary.id) {
                    agent_count += 1;
                }
            }
            summary.agent_count = agent_count;
            summary.review_rounds = persisted.reviews.len() as u64;
            let task = Arc::new(TaskRecord {
                summary: RwLock::new(summary.clone()),
                plan: RwLock::new(persisted.plan),
                plan_history: RwLock::new(persisted.plan_history),
                reviews: RwLock::new(persisted.reviews),
                artifacts: RwLock::new(persisted.artifacts),
                workflow_lock: Mutex::new(()),
            });
            tasks.insert(summary.id, task);
        }
        let mut projects = HashMap::new();
        for mut summary in snapshot.projects {
            let project_id = summary.id;
            let project_agents = agents
                .values()
                .filter(|agent| {
                    agent
                        .summary
                        .try_read()
                        .ok()
                        .and_then(|summary| summary.project_id)
                        == Some(project_id)
                })
                .count();
            if project_agents == 0 {
                summary.status = ProjectStatus::Failed;
                summary.clone_status = ProjectCloneStatus::Failed;
                summary.last_error = Some("maintainer agent is missing".to_string());
                store.save_project(&summary).await?;
            }
            projects.insert(summary.id, Arc::new(ProjectRecord::new(summary)));
        }
        let sidecar_image = runtime_sidecar_image(config.sidecar_image);
        let github_api_base_url = config
            .github_api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_GITHUB_API_BASE_URL)
            .to_string();
        let git_binary = config
            .git_binary
            .clone()
            .unwrap_or_else(|| "git".to_string());
        let projects_root = config.projects_root;
        let workspace_manager = projects::workspace::LocalProjectWorkspaceManager::new(
            git_binary.clone(),
            projects_root.clone(),
        );
        let github_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(GITHUB_HTTP_TIMEOUT_SECS))
            .build()?;
        let github_backend = github_backend.unwrap_or_else(|| {
            Arc::new(DirectGithubAppBackend::new(
                Arc::clone(&store),
                github_http.clone(),
                github_api_base_url.clone(),
            ))
        });
        let git_accounts = Arc::new(github::GitAccountService::new(
            Arc::clone(&store),
            github_http.clone(),
            github_api_base_url.clone(),
            Arc::clone(&github_backend),
        ));
        let mai_config = Arc::new(RwLock::new(config::load_or_initialize(&store).await?));

        let runtime = Arc::new(Self {
            deps: RuntimeDeps {
                docker,
                store: Arc::clone(&store),
                skills,
                agent_profiles,
                github_http,
                github_backend,
                git_accounts,
            },
            state: RuntimeState::new(agents, tasks, projects),
            events: RuntimeEvents::new(
                Arc::clone(&store),
                snapshot.next_sequence,
                snapshot.recent_events,
            ),
            mai_config: Arc::clone(&mai_config),
            agent_framework: OnceCell::new(),
            cache_root: config.cache_root,
            artifact_files_root: config.artifact_files_root,
            sidecar_image,
            github_api_base_url,
            workspace_manager,
        });
        runtime.reconcile_project_workspaces().await?;
        runtime.restore_project_repositories().await;
        let host =
            agent_host::MaiAgentHost::new(Arc::downgrade(&runtime), Arc::clone(&store), mai_config);
        let framework = pl_core::AgentRuntime::start(
            host,
            pl_core::AgentRuntimeOptions {
                command_capacity: 128,
                cancel_grace: Duration::from_millis(
                    runtime
                        .mai_config
                        .read()
                        .await
                        .containers
                        .turn_cancel_grace_ms,
                ),
                restored_inputs: pl_core::RestoredInputPolicy::Hold,
            },
        )
        .await
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        runtime.agent_framework.set(framework).map_err(|_| {
            RuntimeError::InvalidInput("agent framework already started".to_string())
        })?;
        runtime.bootstrap_framework_agents().await?;
        runtime
            .framework_handle()?
            .start_restored_inputs()
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        runtime.reconcile_project_review_singletons().await;
        let cleanup_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            projects::review::cleanup::run_project_review_cleanup_loop(&cleanup_runtime).await;
        });
        runtime.start_enabled_project_review_workers().await;
        Ok(runtime)
    }

    pub(super) fn framework_handle(&self) -> Result<pl_core::AgentRuntimeHandle> {
        self.agent_framework
            .get()
            .map(pl_core::AgentRuntime::handle)
            .ok_or_else(|| RuntimeError::InvalidInput("agent framework is not started".to_string()))
    }

    pub(super) async fn bootstrap_framework_agents(self: &Arc<Self>) -> Result<()> {
        let handle = self.framework_handle()?;
        let existing = handle
            .list()
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?
            .into_iter()
            .map(|snapshot| snapshot.identity.id)
            .collect::<HashSet<_>>();
        let records = self
            .state
            .agents
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut identity_by_product = HashMap::new();
        let mut summaries = HashMap::new();
        for agent in &records {
            let summary = agent.summary.read().await.clone();
            identity_by_product.insert(summary.id, agent.runtime_agent_id.read().await.clone());
            summaries.insert(summary.id, summary);
        }
        let mut registrations = Vec::new();
        for agent in records {
            let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
            if existing.contains(&runtime_agent_id) {
                continue;
            }
            let summary = agent.summary.read().await.clone();
            let parent_id = summary
                .parent_id
                .and_then(|parent| identity_by_product.get(&parent).cloned());
            let role = summary
                .role
                .map(|role| role.to_string())
                .unwrap_or_else(|| "executor".to_string());
            registrations.push((
                framework_depth(summary.id, &summaries),
                pl_core::AgentRegistration {
                    identity: pl_core::AgentIdentity {
                        id: runtime_agent_id,
                        parent_id,
                        role: pl_core::AgentRoleId::new(role)?,
                        depth: framework_depth(summary.id, &summaries),
                    },
                    sessions: vec![initial_framework_session(&summary)],
                },
            ));
        }
        registrations.sort_by_key(|(depth, _)| *depth);
        for (_, registration) in registrations {
            handle
                .register(registration)
                .await
                .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        }
        for snapshot in handle
            .list()
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?
        {
            agent_host::synchronize_runtime_state(self, snapshot).await?;
        }
        Ok(())
    }

    pub(super) async fn register_framework_agent(&self, product_agent_id: AgentId) -> Result<()> {
        let Some(framework) = self.agent_framework.get() else {
            return Ok(());
        };
        let handle = framework.handle();
        let agent = self.agent(product_agent_id).await?;
        let runtime_agent_id = agent.runtime_agent_id.read().await.clone();
        match handle.snapshot(runtime_agent_id.clone()).await {
            Ok(_) => return Ok(()),
            Err(pl_core::AgentRuntimeError::NotFound(_)) => {}
            Err(error) => return Err(RuntimeError::InvalidInput(error.to_string())),
        }
        let summary = agent.summary.read().await.clone();
        let parent_id = if let Some(parent_id) = summary.parent_id {
            Some(
                self.agent(parent_id)
                    .await?
                    .runtime_agent_id
                    .read()
                    .await
                    .clone(),
            )
        } else {
            None
        };
        let records = self
            .state
            .agents
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut summaries = HashMap::new();
        for record in records {
            let summary = record.summary.read().await.clone();
            summaries.insert(summary.id, summary);
        }
        handle
            .register(pl_core::AgentRegistration {
                identity: pl_core::AgentIdentity {
                    id: runtime_agent_id,
                    parent_id,
                    role: pl_core::AgentRoleId::new(
                        summary
                            .role
                            .map(|role| role.to_string())
                            .unwrap_or_else(|| "executor".to_string()),
                    )?,
                    depth: framework_depth(product_agent_id, &summaries),
                },
                sessions: vec![initial_framework_session(&summary)],
            })
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        Ok(())
    }
}
