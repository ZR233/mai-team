use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pl_core::context::ResourceAccess;
use pl_core::persistence::SqliteSessionStore;
use pl_core::thread::{
    ContextCapacity, ModelStepLimit, ThreadCheckpoint, cold::ColdStoreHandle,
    input::InputDriverOptions,
};

use crate::thread_host::{self, ProductRoute, ThreadToolRequest};

#[derive(Clone, Copy)]
enum ThreadAssemblyMode {
    ReuseResident,
    RequireNew,
}

impl AgentRuntime {
    /// 等待 Runtime 进入不可继续的 fail-stop 状态。
    ///
    /// 新 PL 架构把存储故障表达为每个 Thread 的 typed `PersistenceState`，core 会在故障世代内暂停
    /// 该 Thread 的准入；进程级退出信号统一由 [`crate::thread_host`] 的 fatal latch 上报，调用方
    /// 应终止当前 Runtime 并从 durable 状态重新启动。
    pub async fn wait_for_fatal_error(&self) -> RuntimeError {
        thread_host::wait_for_fatal().await
    }

    /// 先停止 PR discovery，再关闭全部驻留 Thread 并排空会话持久化 writer。
    ///
    /// Thread 关闭失败或 writer 停止失败都会保留对应 owner/句柄并返回错误，调用方可以重试同一
    /// `shutdown`，不会丢失释放责任。
    pub async fn shutdown(&self) -> Result<()> {
        self.review_discovery_scheduler.shutdown().await?;
        self.review_ci_watch_scheduler.shutdown().await?;

        let thread_failures = self.thread_kernel.shutdown().await;
        if !thread_failures.is_empty() {
            let detail = thread_failures
                .iter()
                .map(|(id, error)| format!("{id}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(RuntimeError::InvalidInput(format!(
                "Thread shutdown left {} owner(s) requiring retry: {detail}",
                thread_failures.len()
            )));
        }

        let session_failures = self.session_history.shutdown().await;
        if !session_failures.is_empty() {
            let detail = session_failures
                .iter()
                .map(|(agent_id, error)| format!("{agent_id}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(RuntimeError::InvalidInput(format!(
                "session store shutdown left {} writer(s) requiring retry: {detail}",
                session_failures.len()
            )));
        }
        Ok(())
    }

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
        let skills = SkillCatalogService::new_with_system_root(
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
                summary: RwLock::new(summary.clone()),
                registration_pending: AtomicBool::new(false),
                container: RwLock::new(None),
                mcp: RwLock::new(None),
                active_mcp_servers: RwLock::new(Vec::new()),
                review_context: RwLock::new(None),
                skill_catalog: RwLock::new(None),
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
        let sessions_root = config.sessions_root.clone();

        let runtime = Arc::new(Self {
            self_ref: OnceLock::new(),
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
            thread_kernel: thread_kernel::ThreadKernel::default(),
            session_history: session_history::SessionHistory::new(sessions_root),
            child_report_progress: Mutex::new(HashMap::new()),
            review_discovery_scheduler:
                projects::review::discovery::ProjectReviewDiscoveryScheduler::new(),
            review_ci_watch_scheduler:
                projects::review::ci_watch::ProjectReviewCiWatchScheduler::new(),
            cache_root: config.cache_root,
            artifact_files_root: config.artifact_files_root,
            sidecar_image,
            github_api_base_url,
            github_get_cache: github::GithubGetCache::default(),
            pull_request_state_refreshes: github::PullRequestStateRefreshCoordinator::default(),
            workspace_manager,
        });
        runtime
            .self_ref
            .set(Arc::downgrade(&runtime))
            .expect("a new runtime has no prior self reference");
        runtime.reconcile_project_workspaces().await?;
        runtime.restore_project_repositories().await;
        runtime
            .cleanup_orphan_project_review_repository_views()
            .await;
        runtime.cleanup_orphan_project_review_host_contexts().await;
        let cleanup_runtime = Arc::clone(&runtime);
        projects::review::cleanup::cleanup_project_review_history(&cleanup_runtime).await?;
        tokio::spawn(async move {
            projects::review::cleanup::run_project_review_cleanup_loop(&cleanup_runtime).await;
        });
        let resource_cleanup_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            projects::review::cleanup::run_project_review_resource_cleanup_loop(
                &resource_cleanup_runtime,
            )
            .await;
        });
        runtime
            .review_ci_watch_scheduler
            .start(Arc::clone(&runtime))
            .await;
        runtime
            .review_discovery_scheduler
            .start(Arc::clone(&runtime))
            .await;
        runtime.start_enabled_project_review_workers().await;
        tokio::spawn(crate::runtime_child_reports::run_child_report_loop(
            Arc::downgrade(&runtime),
        ));
        Ok(runtime)
    }

    /// 按产品父子图顺序惰性装配并返回一个驻留 Thread。
    ///
    /// 祖先先装配，符合 PL `design/14` 对子 Thread 的父必须先激活的要求；同 id 的并发装配归一为
    /// 同一个已发布 owner。
    pub(crate) async fn ensure_thread(
        &self,
        product_agent_id: AgentId,
    ) -> Result<thread_kernel::ResidentThread> {
        let owner = self.self_ref.get().and_then(Weak::upgrade).ok_or_else(|| {
            RuntimeError::InvalidInput("runtime owner is unavailable".to_string())
        })?;
        let mut lineage = Vec::new();
        let mut current = Some(product_agent_id);
        while let Some(agent_id) = current {
            let agent = self.agent(agent_id).await?;
            let parent_id = agent.summary.read().await.parent_id;
            lineage.push(agent_id);
            current = parent_id;
        }
        lineage.reverse();

        let mut requested = None;
        for agent_id in lineage {
            let resident = owner.ensure_resident_thread(agent_id).await?;
            if agent_id == product_agent_id {
                requested = Some(resident);
            }
        }
        requested.ok_or(RuntimeError::AgentNotFound(product_agent_id))
    }

    /// 注册一个已经准备好产品资源的 Agent 对应的驻留 Thread。
    ///
    /// 父 Thread 先于子 Thread 装配；注册成功后该 Agent 的 canonical 生命周期由 Thread owner
    /// 承担，创建滚动回滚会关闭它。
    pub(crate) async fn register_prepared_thread(
        self: &Arc<Self>,
        resource: &mut runtime_agent_creation::PreparedAgentResource,
    ) -> Result<thread_kernel::ResidentThread> {
        let product_agent_id = resource.id();
        self.ensure_parent_thread(product_agent_id).await?;
        resource.include_canonical_runtime();
        self.assemble_thread(product_agent_id).await
    }

    /// 装配并注册一个协作 child 的驻留 Thread。
    ///
    /// 与普通装配共用父 Thread 激活顺序与回滚语义；child 的 context 只由自己的 Profile
    /// 指令和随后通过 inbox 投递的任务消息组成。
    pub(crate) async fn register_prepared_child_thread(
        self: &Arc<Self>,
        resource: &mut runtime_agent_creation::PreparedAgentResource,
    ) -> Result<thread_kernel::ResidentThread> {
        let product_agent_id = resource.id();
        self.ensure_parent_thread(product_agent_id).await?;
        resource.include_canonical_runtime();
        self.assemble_child_thread(product_agent_id).await
    }

    /// 先在父 Thread 已经驻留之后才装配子 Thread；根 Agent 直接跳过。
    async fn ensure_parent_thread(&self, product_agent_id: AgentId) -> Result<()> {
        let agent = self.agent(product_agent_id).await?;
        if let Some(parent_id) = agent.summary.read().await.parent_id {
            self.ensure_thread(parent_id).await?;
        }
        Ok(())
    }

    /// 装配一个产品 Thread：恢复只消费 core 的 typed checkpoint，新 Thread 才写入初始指令。
    ///
    /// 该入口只发布完整驻留的 owner；装配失败时 core 会先关闭尚未发布的 owner，不会留下半成品。
    pub(crate) async fn assemble_thread(
        self: &Arc<Self>,
        product_agent_id: AgentId,
    ) -> Result<thread_kernel::ResidentThread> {
        let agent = self.agent(product_agent_id).await?;
        let thread_id = product_agent_id.to_string();
        let store = self.open_session_store(product_agent_id).await?;
        let checkpoint = store
            .read_thread_checkpoint(&thread_id)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        // 恢复路径复用 core 的 typed checkpoint，不允许再用当前配置覆盖旧指令。
        let seed = if checkpoint.is_none() {
            self.seed_for_agent(&agent).await
        } else {
            thread_host::ThreadSeed::default()
        };
        self.assemble_thread_spec(
            agent,
            thread_id,
            checkpoint,
            store,
            seed,
            ThreadAssemblyMode::ReuseResident,
        )
        .await
    }

    /// 装配一个全新的 child Thread。
    ///
    /// child 只获得自己的 Profile 指令；任务正文由 canonical inbox 消息投递。只有没有
    /// checkpoint 的新 Thread 才能走此入口。
    pub(crate) async fn assemble_child_thread(
        self: &Arc<Self>,
        product_agent_id: AgentId,
    ) -> Result<thread_kernel::ResidentThread> {
        let agent = self.agent(product_agent_id).await?;
        let thread_id = product_agent_id.to_string();
        let store = self.open_session_store(product_agent_id).await?;
        let checkpoint = store
            .read_thread_checkpoint(&thread_id)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        if checkpoint.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "child Thread `{thread_id}` already has a checkpoint; inherited context assembly is \
                 only valid for a new Thread"
            )));
        }
        let seed = self.seed_for_agent(&agent).await;
        self.assemble_thread_spec(
            agent,
            thread_id,
            None,
            store,
            seed,
            ThreadAssemblyMode::RequireNew,
        )
        .await
    }

    /// 生成一个新 Thread 的静态初始指令。
    async fn seed_for_agent(&self, agent: &Arc<AgentRecord>) -> thread_host::ThreadSeed {
        let summary = agent.summary.read().await.clone();
        let system_prompt = agent.system_prompt.clone();
        let config = self.mai_config.read().await;
        thread_host::thread_seed(&summary, system_prompt.as_deref(), &config.instructions)
    }

    /// 打开某个 Agent 独占的会话存储句柄。
    async fn open_session_store(&self, product_agent_id: AgentId) -> Result<SqliteSessionStore> {
        self.session_history
            .open(product_agent_id)
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
    }

    /// 由已解析完成的产品事实构造并发布一个 canonical [`thread_kernel::ThreadSpec`]。
    async fn assemble_thread_spec(
        self: &Arc<Self>,
        agent: Arc<AgentRecord>,
        thread_id: String,
        checkpoint: Option<ThreadCheckpoint>,
        store: SqliteSessionStore,
        seed: thread_host::ThreadSeed,
        mode: ThreadAssemblyMode,
    ) -> Result<thread_kernel::ResidentThread> {
        let summary = agent.summary.read().await.clone();
        let route = {
            let config = self.mai_config.read().await;
            thread_host::resolve_agent_route(&config.models, &summary)
        };
        let route = match route {
            ProductRoute::Available(route) => Some(*route),
            ProductRoute::Unavailable {
                provider_id,
                model,
                reason,
            } => {
                if checkpoint.is_none() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "agent {} has no usable model route `{provider_id}/{model}`: {reason}",
                        summary.id
                    )));
                }
                tracing::warn!(agent_id = %summary.id, %provider_id, %model, %reason,
                    "restoring Thread history without an available model route");
                None
            }
        };
        let tools = if route.is_some() {
            thread_host::assemble_thread_tools(ThreadToolRequest {
                runtime: Arc::clone(self),
                agent: Arc::clone(&agent),
                thread_id: thread_id.clone(),
            })
            .await?
        } else {
            thread_host::ThreadToolAssembly::default()
        };
        let spec = thread_kernel::ThreadSpec {
            id: thread_id,
            checkpoint,
            route,
            hosted_tools: tools.hosted_tools,
            context_preparation: None,
            initial_context: seed.context,
            initial_extensions: seed.extensions,
            registrations: tools.registrations,
            resources: ResourceAccess::new(thread_resources::MaiResourceStore::new(
                thread_resources::thread_resources_root(&self.artifact_files_root),
            )),
            capacity: ContextCapacity::Unbounded,
            cold_store: Some(ColdStoreHandle::new(store)),
            input_driver: InputDriverOptions {
                // 主会话与独立 Review reviewer 不限步数；协作 child 每个 Turn 单独限 256 步。
                max_model_steps: if summary.parent_id.is_some()
                    && summary.profile_id.as_deref() != Some("reviewer")
                {
                    ModelStepLimit::Limited(256.try_into().expect("256 is a valid step limit"))
                } else {
                    ModelStepLimit::Unlimited
                },
            },
        };
        let assembled = match mode {
            ThreadAssemblyMode::ReuseResident => self.thread_kernel.assemble_or_get(spec).await,
            ThreadAssemblyMode::RequireNew => self.thread_kernel.assemble(spec).await,
        };
        assembled.map_err(|error| RuntimeError::InvalidInput(error.to_string()))
    }

    /// 返回一个已经完整发布的驻留 Thread；不触发装配。
    pub(crate) fn resident_thread(
        &self,
        product_agent_id: AgentId,
    ) -> Option<thread_kernel::ResidentThread> {
        self.thread_kernel.lookup(&product_agent_id.to_string())
    }

    /// 关闭并移除一个驻留 Thread；关闭失败时 owner 保留，可重试。
    pub(crate) async fn close_thread(&self, product_agent_id: AgentId) -> Result<()> {
        self.thread_kernel
            .close(&product_agent_id.to_string())
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
    }

    async fn ensure_resident_thread(
        self: &Arc<Self>,
        product_agent_id: AgentId,
    ) -> Result<thread_kernel::ResidentThread> {
        if let Some(resident) = self.resident_thread(product_agent_id) {
            return Ok(resident);
        }
        // 创建流程会先公开产品资源，再装配 Thread。读取方不能抢先用普通 seed
        // 装配协作 child，也不能提前占用 Review reviewer 的 Thread 身份。
        let agent = self.agent(product_agent_id).await?;
        if agent.registration_pending.load(Ordering::Acquire) {
            return Err(RuntimeError::ThreadNotFound(product_agent_id.to_string()));
        }
        match self.assemble_thread(product_agent_id).await {
            Ok(resident) => Ok(resident),
            // 关闭等其他状态改变后，允许使用并发路径刚发布的 owner。
            Err(error) => self.resident_thread(product_agent_id).ok_or(error),
        }
    }
}
