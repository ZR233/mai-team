use super::*;

impl AgentRuntime {
    /// 等待一个 Thread 的提交水位达到 `revision` 且已经 durable。
    ///
    /// 只观察 canonical Thread 的 typed 持久化事实（提交水位与 durable 水位），不读取旧
    /// store 的 thread runtime，也不解释任何 SQLite 原始表。
    pub(crate) async fn await_agent_durable(&self, agent_id: AgentId, revision: u64) -> Result<()> {
        let resident = self.ensure_thread(agent_id).await?;
        let mut snapshots = resident.handle.subscribe();
        loop {
            let Some(snapshot) = snapshots.next().await else {
                return Err(durable_wait_closed(agent_id, revision));
            };
            if durable_through(&snapshot, revision) {
                return Ok(());
            }
            if snapshot.lifecycle == pl_core::thread::ThreadLifecycle::Closed {
                return Err(durable_wait_closed(agent_id, revision));
            }
        }
    }

    pub async fn update_agent(
        &self,
        agent_id: AgentId,
        request: UpdateAgentRequest,
    ) -> Result<AgentSummary> {
        agents::update_agent(self, agent_id, request).await
    }

    pub async fn cleanup_orphaned_containers(&self) -> Result<Vec<String>> {
        let (active_agent_ids, active_project_ids) = {
            let agents = self.state.agents.read().await;
            let projects = self.state.projects.read().await;
            (
                agents
                    .keys()
                    .map(ToString::to_string)
                    .collect::<HashSet<_>>(),
                projects
                    .keys()
                    .map(ToString::to_string)
                    .collect::<HashSet<_>>(),
            )
        };
        Ok(self
            .deps
            .docker
            .cleanup_orphaned_managed_containers(&active_agent_ids, &active_project_ids)
            .await?)
    }

    pub async fn get_agent(&self, agent_id: AgentId) -> Result<AgentDetail> {
        let agent = self.agent(agent_id).await?;
        let resident = self.ensure_thread(agent_id).await?;
        let state = resident.handle.snapshot();
        let (summary, thread) = self.project_agent_thread(agent.as_ref(), &state).await?;
        Ok(AgentDetail {
            thread: thread.thread,
            summary,
        })
    }

    pub async fn tool_trace(&self, agent_id: AgentId, call_id: String) -> Result<ToolTraceDetail> {
        agents::tool_trace(self, agent_id, call_id).await
    }

    pub async fn tool_output_artifact(
        &self,
        agent_id: AgentId,
        call_id: String,
        artifact_id: String,
    ) -> Result<(ToolOutputArtifactInfo, PathBuf)> {
        agents::tool_output_artifact(self, agent_id, call_id, artifact_id).await
    }

    pub async fn agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<AgentLogsResponse> {
        agents::agent_logs(self, agent_id, filter).await
    }

    pub async fn tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<ToolTraceListResponse> {
        agents::tool_traces(self, agent_id, filter).await
    }

    pub async fn send_message(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> Result<TurnId> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        if summary.resource.state != AgentResourceState::Ready {
            return Err(RuntimeError::ThreadNotFound(summary.id.to_string()));
        }
        let resident = self.ensure_thread(agent_id).await?;
        let state = resident.handle.snapshot();
        if state.lifecycle != pl_core::thread::ThreadLifecycle::Open {
            return Err(RuntimeError::ThreadNotFound(summary.id.to_string()));
        }
        let mut input = product_message_input(message.clone(), skill_mentions.clone())?;
        let catalog = agent.skill_catalog.read().await.clone();
        match catalog {
            Some(catalog) => {
                // 与 Thread 的 `skill_view` 使用同一冻结目录，直接调用 PL 的用户技能选择与加载。
                let loaded = catalog
                    .load_user_invocations_with_selections(
                        &message,
                        &skill_mentions,
                        &input.id,
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .map_err(RuntimeError::Model)?;
                if let Some(instruction) = loaded.instruction {
                    input.context.push(pl_core::context::ContextContent::Text {
                        text: instruction.into(),
                    });
                }
            }
            None if !skill_mentions.is_empty() => {
                return Err(RuntimeError::InvalidInput(
                    "this Thread has no enabled Skill catalog".to_string(),
                ));
            }
            None => {}
        }
        let accepted = resident
            .handle
            .submit_input_and_continue(input, resident.input_driver)
            .await?;
        // 受理回执的身份就是 canonical InputRecord 的身份：Turn 只在后续执行时创建，
        // 因此这里返回输入身份而不是当时的空 turn id。
        Ok(accepted.input.id)
    }

    pub async fn cancel_agent(self: &Arc<Self>, agent_id: AgentId) -> Result<()> {
        self.agent(agent_id).await?;
        let resident = self.ensure_thread(agent_id).await?;
        resident.handle.interrupt_turn(None).await?;
        Ok(())
    }

    pub async fn cancel_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        turn_id: TurnId,
    ) -> Result<()> {
        self.agent(agent_id).await?;
        let resident = self.ensure_thread(agent_id).await?;
        let state = resident.handle.snapshot();
        let Some(active) = state
            .turns
            .iter()
            .rev()
            .find(|turn| turn.state == pl_core::thread::TurnState::Running)
        else {
            return Ok(());
        };
        // 标识别名：调用方可能持有开跑时受理的输入身份（`send_message` 的返回值），也可能
        // 持有投影出的活动 Turn 身份。两者指向同一段执行；都不匹配时保持旧的空操作语义。
        let matches =
            active.turn_id == turn_id || active.input_id.as_deref() == Some(turn_id.as_str());
        if !matches {
            return Ok(());
        }
        match resident
            .handle
            .interrupt_turn(Some(active.turn_id.clone()))
            .await
        {
            Ok(_) => Ok(()),
            // 运行中的 Turn 已经在比对后结束或换成了另一个 Turn；取消请求已经失去目标。
            Err(pl_core::thread::ThreadError::InvalidIdentity) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        agents::delete_agent(self, agent_id).await
    }

    pub(super) async fn cleanup_agent_tool_output_namespace(
        &self,
        agent_id: AgentId,
    ) -> Result<()> {
        let namespace = self
            .artifact_files_root
            .join("tool-output")
            .join(agent_id.to_string());
        match tokio::fs::remove_dir_all(namespace).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn cleanup_tool_output_namespaces(
        &self,
        cutoff: std::time::SystemTime,
        batch_size: usize,
    ) -> Result<usize> {
        let root = self.artifact_files_root.join("tool-output");
        let live_agents = self
            .list_agents()
            .await
            .into_iter()
            .map(|agent| agent.id)
            .collect::<std::collections::HashSet<_>>();
        let mut namespaces = match tokio::fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let mut removed = 0;
        while let Some(namespace) = namespaces.next_entry().await? {
            if removed >= batch_size {
                break;
            }
            let Some(name) = namespace.file_name().to_str().map(ToString::to_string) else {
                tracing::warn!(path = %namespace.path().display(), "tool-output namespace is not valid UTF-8");
                continue;
            };
            let Ok(agent_id) = Uuid::parse_str(&name) else {
                tracing::warn!(path = %namespace.path().display(), "tool-output namespace has an invalid agent id");
                continue;
            };
            if !live_agents.contains(&agent_id) {
                tokio::fs::remove_dir_all(namespace.path()).await?;
                removed += 1;
                continue;
            }
            let mut calls = match tokio::fs::read_dir(namespace.path()).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            while removed < batch_size {
                let Some(call) = calls.next_entry().await? else {
                    break;
                };
                if call.metadata().await?.modified()? < cutoff {
                    if call.file_type().await?.is_dir() {
                        tokio::fs::remove_dir_all(call.path()).await?;
                    } else {
                        tokio::fs::remove_file(call.path()).await?;
                    }
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    pub(super) async fn close_agent(&self, agent_id: AgentId) -> Result<()> {
        agents::close_agent(self, agent_id).await
    }

    pub(super) async fn cleanup_agent_workspace(&self, agent_id: AgentId) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let project_id = agent.summary.read().await.project_id;
        let volume = if let Some(project_id) = project_id {
            let review_context = agent.review_context.read().await.clone();
            if let Some(context) = review_context.as_deref() {
                self.cleanup_project_review_context(project_id, context)
                    .await?;
                *agent.review_context.write().await = None;
            }
            self.workspace_manager
                .cleanup_agent_workspace(project_id, agent_id)
                .await?;
            project_agent_workspace_volume(&project_id.to_string(), &agent_id.to_string())
        } else {
            agent_workspace_volume(&agent_id.to_string())
        };
        self.deps.docker.delete_volume(&volume).await?;
        Ok(())
    }

    pub async fn cancel_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        tasks::cancel_task(&self.state, self, task_id).await
    }

    pub async fn delete_task(self: &Arc<Self>, task_id: TaskId) -> Result<()> {
        tasks::delete_task(&self.state, self, task_id).await
    }

    pub async fn upload_file(
        &self,
        agent_id: AgentId,
        path: String,
        content_base64: String,
    ) -> Result<usize> {
        agents::upload_file(self, agent_id, path, content_base64).await
    }

    pub async fn download_file_tar(&self, agent_id: AgentId, path: String) -> Result<Vec<u8>> {
        agents::download_file_tar(self, agent_id, path).await
    }

    pub async fn save_artifact(
        self: &Arc<Self>,
        agent_id: AgentId,
        path: String,
        display_name: Option<String>,
    ) -> Result<ArtifactInfo> {
        tasks::save_artifact(&self.state, self.as_ref(), agent_id, path, display_name).await
    }

    pub fn artifact_file_path(&self, info: &ArtifactInfo) -> PathBuf {
        tasks::artifact_file_path(&self.artifact_files_root, info)
    }

    pub fn tool_output_artifact_file_path(
        &self,
        agent_id: AgentId,
        call_id: &str,
        artifact_id: &str,
        name: &str,
    ) -> PathBuf {
        let namespace = agent_id.to_string();
        crate::turn::container::tool_output_artifact_file_path(
            &self.artifact_files_root,
            &namespace,
            call_id,
            artifact_id,
            name,
        )
    }

    pub(super) async fn save_task_plan(
        self: &Arc<Self>,
        agent_id: AgentId,
        title: String,
        markdown: String,
    ) -> Result<TaskSummary> {
        tasks::save_task_plan(&self.state, self.as_ref(), agent_id, title, markdown).await
    }

    pub(super) async fn submit_review_result(
        self: &Arc<Self>,
        agent_id: AgentId,
        passed: bool,
        findings: String,
        summary: String,
    ) -> Result<TaskReview> {
        tasks::submit_review_result(
            &self.state,
            self.as_ref(),
            agent_id,
            passed,
            findings,
            summary,
        )
        .await
    }

    pub(super) fn spawn_task_workflow(self: &Arc<Self>, task_id: TaskId) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = tasks::run_task_workflow(&runtime.state, &runtime, task_id).await
                && let Ok(task) = runtime.task(task_id).await
            {
                let _ = runtime
                    .set_task_status(&task, TaskStatus::Failed, None, Some(err.to_string()))
                    .await;
            }
        });
    }

    pub(super) async fn spawn_task_role_agent(
        self: &Arc<Self>,
        parent_agent_id: AgentId,
        role: AgentRole,
        name: Option<String>,
    ) -> Result<AgentSummary> {
        // 父 Thread 必须先驻留：child 的容器从父容器克隆，父子 Thread 关系是产品事实。
        self.ensure_thread(parent_agent_id).await?;
        let parent = self.agent(parent_agent_id).await?;
        let parent_summary = parent.summary.read().await.clone();
        let role_id = pl_model::config::AgentRoleId::new(agent_role_label(role))?;
        let profile = self
            .mai_config
            .read()
            .await
            .models
            .resolve(&role_id)
            .map_err(RuntimeError::Model)?;
        let parent_container_id = self.container_id(parent_agent_id).await?;
        // 冻结 Profile 的 provider/model/effort 与系统指令在创建产品记录时就写入 child，
        // child 的驻留 Thread 随后由产品创建流程统一装配。
        let resource = self
            .create_agent_resource_with_container_source(
                AgentId::new_v4(),
                CreateAgentRequest {
                    name,
                    provider_id: Some(profile.provider_id.to_string()),
                    model: Some(profile.model.slug.clone()),
                    reasoning_effort: profile.effort.map(|effort| effort.as_str().to_string()),
                    docker_image: Some(parent_summary.docker_image.clone()),
                    parent_id: Some(parent_agent_id),
                    system_prompt: Some(agents::task_role_system_prompt(role).to_string()),
                },
                agents::ContainerSource::CloneFrom {
                    parent_container_id,
                    docker_image: parent_summary.docker_image.clone(),
                    workspace_volume: None,
                },
                parent_summary.task_id,
                parent_summary.project_id,
                Some(role),
            )
            .await?;
        self.register_prepared_agent(resource).await
    }

    pub(super) async fn start_agent_turn(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: String,
    ) -> Result<TurnId> {
        self.send_message(agent_id, message, Vec::new()).await
    }

    pub(super) async fn wait_agent(
        &self,
        agent_id: AgentId,
        timeout: Duration,
    ) -> Result<AgentSummary> {
        tokio::time::timeout(timeout, self.wait_thread_settled(agent_id, None))
            .await
            .map_err(|_| RuntimeError::InvalidInput("waiting for agent timed out".to_string()))??;
        Ok(self.get_agent(agent_id).await?.summary)
    }

    /// 等待一个 Thread 进入稳定状态并返回最近的终态 Turn。
    ///
    /// 稳定状态是“空闲”或“故障”：两者都表示当前已经没有可继续推进的执行，调用方必须
    /// 从 typed 事实（例如最后一条终态 Turn）判断成败，而不是把故障当成仍在运行。取消令牌
    /// 命中时返回 [`RuntimeError::TurnCancelled`]，不消费任何未返回的 Turn。
    pub(super) async fn wait_agent_turn(
        &self,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<ThreadWaitOutcome> {
        self.agent(agent_id).await?;
        self.ensure_thread(agent_id).await?;
        self.wait_thread_settled(agent_id, Some(cancellation_token))
            .await?;
        let last_turn = self.last_terminal_turn(agent_id).await?;
        Ok(ThreadWaitOutcome { last_turn })
    }

    /// 订阅一个驻留 Thread，直到它空闲或故障；取消令牌命中则立即返回取消。
    async fn wait_thread_settled(
        &self,
        agent_id: AgentId,
        cancellation: Option<&CancellationToken>,
    ) -> Result<pl_core::thread::ThreadSnapshot> {
        let resident = self.ensure_thread(agent_id).await?;
        let mut snapshots = resident.handle.subscribe();
        loop {
            let next = match cancellation {
                Some(token) => tokio::select! {
                    snapshot = snapshots.next() => snapshot,
                    () = token.cancelled() => return Err(RuntimeError::TurnCancelled),
                },
                None => snapshots.next().await,
            };
            let Some(snapshot) = next else {
                return Err(RuntimeError::ThreadNotFound(agent_id.to_string()));
            };
            match thread_projection::status(&snapshot) {
                ThreadStatus::Idle | ThreadStatus::Faulted => return Ok(snapshot),
                ThreadStatus::Closed => {
                    return Err(RuntimeError::ThreadNotFound(agent_id.to_string()));
                }
                ThreadStatus::Queued
                | ThreadStatus::Running
                | ThreadStatus::WaitingTool
                | ThreadStatus::WaitingInteraction
                | ThreadStatus::Cancelling
                | ThreadStatus::Closing => continue,
            }
        }
    }

    /// 把一个驻留 Thread 的 canonical 状态投影成产品摘要与权威首帧。
    ///
    /// typed 累计用量来自 core 的 [`pl_core::thread::UsageSummary`]，产品摘要里不保留任何
    /// 从旧 `thread_runtime` 或 SQLite 原始表反推的用量。
    async fn project_agent_thread(
        &self,
        agent: &crate::state::AgentRecord,
        state: &pl_core::thread::ThreadSnapshot,
    ) -> Result<(AgentSummary, ThreadSnapshot)> {
        let mut summary = agent.summary.read().await.clone();
        summary.usage = canonical_usage(&state.usage_summary, summary.updated_at.timestamp());
        summary.last_turn = self.last_terminal_turn(summary.id).await?;
        let thread = project_thread_snapshot(&summary, state)?;
        summary.runtime = Some(thread.clone());
        Ok((summary, thread))
    }
}

/// 一次“等待到稳定”的 canonical 结果。
///
/// 最近一条已提交的终态 Turn（core typed 投影，不从工具文本猜测）。
pub(crate) struct ThreadWaitOutcome {
    /// 最近一条终态 Turn；没有可读终态时为 `None`。
    pub last_turn: Option<Turn>,
}

impl ThreadWaitOutcome {
    /// 最近一条终态 Turn 是否被取消。
    pub(crate) fn last_turn_cancelled(&self) -> bool {
        matches!(
            self.last_turn.as_ref().map(|turn| &turn.state),
            Some(TurnState::Cancelled(_))
        )
    }
}

/// 产品消息在 [`pl_core::thread::input::ThreadInput`] 中的 payload 格式。
///
/// `context` 承载模型可见的用户文本，payload 只承载产品路由事实（技能提及）。消费这条
/// payload 的 turn 准备路径由 Thread 装配所有权负责读取；它必须按格式与版本解码，不得
/// 从非类型化 metadata 猜测。
const PRODUCT_MESSAGE_FORMAT: &str = "mai.thread.message";
const PRODUCT_MESSAGE_VERSION: u32 = 1;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProductMessagePayload<'a> {
    text: &'a str,
    skill_mentions: &'a [String],
}

/// 由一条产品消息构造 canonical [`pl_core::thread::input::ThreadInput`]。
///
/// 输入身份由产品生成，正文同时进入模型可见 context；技能提及只进入 payload，不改变模型
/// 可见文本。
fn product_message_input(
    message: String,
    skill_mentions: Vec<String>,
) -> Result<pl_core::thread::input::ThreadInput> {
    let content = serde_json::to_string(&ProductMessagePayload {
        text: &message,
        skill_mentions: &skill_mentions,
    })
    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    let payload = pl_core::context::OpaquePayload::new(
        PRODUCT_MESSAGE_FORMAT,
        PRODUCT_MESSAGE_VERSION,
        content,
    )
    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    Ok(pl_core::thread::input::ThreadInput {
        id: Uuid::new_v4().to_string(),
        payload,
        context: vec![pl_core::context::ContextContent::Text {
            text: message.into(),
        }],
    })
}

/// 用产品元数据与 canonical Thread 状态投影产品首帧。
///
/// 会话级位置取产品容器工作区；child 的冻结工作区收据优先于根 Agent 的项目默认路径。
pub(crate) fn project_thread_snapshot(
    summary: &AgentSummary,
    state: &pl_core::thread::ThreadSnapshot,
) -> Result<ThreadSnapshot> {
    let mode = product_thread_mode(summary);
    let (workspace_mode, workspace_path) = match summary.workspace.as_ref() {
        Some(assignment) => {
            let mode = match assignment.mode {
                pl_protocol::AgentWorkspaceMode::Worktree => {
                    pl_protocol::ThreadWorkspaceMode::Worktree
                }
                pl_protocol::AgentWorkspaceMode::Unrestricted
                | pl_protocol::AgentWorkspaceMode::Directory => {
                    pl_protocol::ThreadWorkspaceMode::Local
                }
            };
            (mode, assignment.root.clone())
        }
        None if summary.project_id.is_some() => (
            pl_protocol::ThreadWorkspaceMode::Local,
            crate::projects::workspace::AGENT_WORKSPACE_REPO_PATH.to_string(),
        ),
        None => (
            pl_protocol::ThreadWorkspaceMode::Local,
            "/workspace".to_string(),
        ),
    };
    let metadata = thread_projection::ThreadProjectionMetadata {
        mode,
        workspace_mode,
        workspace_path: &workspace_path,
    };
    thread_projection::project_snapshot(summary, state, &metadata)
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
}

/// Review 会话使用 Review Mode，Task/Project 会话使用 Task Mode，其余使用 Simple Mode。
fn product_thread_mode(summary: &AgentSummary) -> ThreadModeId {
    if summary.review_run_id.is_some() {
        return ThreadModeId::new(crate::skills::REVIEW_MODE_ID)
            .expect("mai Review Mode is a valid Thread ModeId");
    }
    if summary.task_id.is_some() || summary.project_id.is_some() {
        ThreadModeId::task()
    } else {
        ThreadModeId::simple()
    }
}

/// 把 core 的 typed 累计用量投影成产品用量快照。
fn canonical_usage(usage: &pl_core::thread::UsageSummary, updated_at: i64) -> RuntimeUsageSnapshot {
    RuntimeUsageSnapshot {
        has_incomplete_usage: usage.has_incomplete_usage,
        model: usage.model.clone(),
        context_window: usage.context_window,
        latest_context_tokens: usage.latest_context_tokens,
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        cached_prompt_tokens: usage.cached_prompt_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        // 缓存未命中是有效缓存样本里未被命中的输入 token。
        cache_miss_tokens: usage
            .cache_input_tokens
            .saturating_sub(usage.cache_read_tokens),
        reasoning_tokens: usage.reasoning_tokens,
        inference_count: usage.inference_count,
        total_tokens: usage.total_tokens,
        estimated_costs: runtime_cost_amounts(&usage.estimated_costs),
        estimated_cache_savings: runtime_cost_amounts(&usage.estimated_cache_savings),
        has_unpriced_usage: usage.has_unpriced_usage,
        updated_at,
    }
}

fn runtime_cost_amounts(
    costs: &[pl_core::thread::UsageCost],
) -> Vec<pl_protocol::RuntimeCostAmount> {
    costs
        .iter()
        .map(|cost| pl_protocol::RuntimeCostAmount {
            currency: cost.currency.clone(),
            amount: cost.amount,
        })
        .collect()
}

impl AgentRuntime {
    /// 从 pl-core 的 typed 持久化历史读取最近一条完整终态 Turn。
    pub(crate) async fn last_terminal_turn(&self, agent_id: AgentId) -> Result<Option<Turn>> {
        Ok(self
            .thread_turns(agent_id.to_string(), None, 1)
            .await?
            .turns
            .into_iter()
            .next()
            .map(|history| history.turn))
    }
}

/// 提交水位已覆盖 `revision`，且挂载存储时 durable 水位也已追上。
fn durable_through(snapshot: &pl_core::thread::ThreadSnapshot, revision: u64) -> bool {
    snapshot.commit_sequence >= revision
        && (!snapshot.persistence.attached || snapshot.persistence.durable_sequence >= revision)
}

fn durable_wait_closed(agent_id: AgentId, revision: u64) -> RuntimeError {
    RuntimeError::InvalidInput(format!(
        "Thread `{agent_id}` closed before revision {revision} became durable"
    ))
}
