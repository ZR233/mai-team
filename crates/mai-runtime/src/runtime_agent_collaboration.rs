//! mai 产品侧的 PL 协作运行时原语。
//!
//! 本模块只实现 [`pl_tool::collaboration::thread::AgentControlHost`] 背后的产品语义：按
//! `profile_id` 解析冻结的产品 Profile、canonical 角色与配置模型，创建并装配 canonical child
//! Thread，投递 PL inbox 消息，校验 parent/descendant 关系，并按 disposition 关闭子树。工具前端
//! 的 JSON 解析、消息身份与 `foreground()` 调度属于 pl-tool，本模块不复制它们，也不定义任何
//! 兼容层。
//!
//! 约束：
//! - child id 由调用方与 `call_id` 决定（UUID v5），同一个 `spawn_agent` 重试不会创建第二个
//!   child；
//! - child 使用自己的 Profile 指令，并通过 PL inbox 接收带有明确用途的初始任务消息，不复制
//!   调用方的模型会话或执行器；
//! - 关系校验的权威来源是产品 [`AgentSummary::parent_id`]，并显式防环；
//! - 不可逆清理只在调用方 Thread 达到持久化屏障之后执行。

use super::*;

use pl_core::context::{AgentMessageKind, ContextContent, OpaquePayload};
use pl_core::thread::ThreadLifecycle;
use pl_core::thread::inbox::ThreadMessage;
use pl_protocol::AgentWorkspaceAssignmentSnapshot;
use pl_tool::collaboration::thread::AgentWorkspaceDisposition;

use crate::runtime_provisioning::{AgentCreationIdentity, AgentCreationRequest};

/// 协作 child 身份的固定 UUID v5 命名空间。
///
/// 该值属于产品身份的一部分，不得随版本调整：同一 `(caller, call_id)` 必须永远映射到同一个
/// child id，否则重试会变成创建第二个 child。
const CHILD_AGENT_NAMESPACE: Uuid = Uuid::from_u128(0x6d616900_00000000_00000000_00000001);

/// 一次协作 `spawn_agent` 请求。
///
/// Profile 解析、能力校验与工作区冻结都已经由 host 完成；这里只承载 `spawn_child_agent` 真正
/// 需要的产品事实。
pub(crate) struct SpawnChildRequest {
    pub(crate) caller: AgentId,
    pub(crate) call_id: String,
    pub(crate) profile_id: String,
    pub(crate) role: AgentRole,
    /// child 自身的系统指令，来自解析后的 Profile。
    pub(crate) system_prompt: String,
    pub(crate) task_summary: String,
    pub(crate) message: String,
    /// 创建时冻结的 child 工作区收据。
    pub(crate) workspace: AgentWorkspaceAssignmentSnapshot,
}

/// 一次成功 `spawn_agent` 的产品事实。
pub(crate) struct SpawnedChild {
    pub(crate) agent_id: AgentId,
    pub(crate) workspace: AgentWorkspaceAssignmentSnapshot,
    /// 初始消息的受理序号；child 已经存在且仍在运行时为 `None`。
    pub(crate) message_sequence: Option<u64>,
}

/// `list_agents` 面向模型的一行；状态来自驻留 Thread 的 canonical 首帧。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollaborationAgentRow {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_id: Option<String>,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<AgentRole>,
    /// 驻留 Thread 的生命周期；未驻留时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) lifecycle: Option<ThreadLifecycle>,
    /// 驻留 Thread 的产品状态；未驻留时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<ThreadStatus>,
}

impl AgentRuntime {
    /// 解析调用方冻结的产品 Profile，用于校验它的协作能力。
    ///
    /// 调用方没有冻结 `profile_id`，或 Profile 已经缺失/禁用时显式失败：产品不猜测一个默认
    /// 能力集，也不放行没有 Profile 的 Agent 去创建或关闭其它 Agent。
    pub(crate) async fn collaboration_caller_profile(
        &self,
        caller: AgentId,
    ) -> Result<AgentProfile> {
        let agent = self.agent(caller).await?;
        let profile_id = agent.summary.read().await.profile_id.clone();
        let Some(profile_id) = profile_id else {
            return Err(RuntimeError::InvalidInput(format!(
                "agent `{caller}` has no frozen product profile; collaboration capabilities are \
                 unavailable"
            )));
        };
        self.profile_by_id(&profile_id).ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "agent `{caller}` references unknown or disabled profile `{profile_id}`"
            ))
        })
    }

    /// 解析一个启用中的产品 Profile 及其 canonical 角色。
    ///
    /// 角色来自 Profile 的 `default_model_role`；未声明时使用 [`AgentRole`] 的默认值。角色决定了
    /// child 冻结的模型路由与工作区边界。
    pub(crate) fn resolve_collaboration_profile(
        &self,
        profile_id: &str,
    ) -> Result<(AgentProfile, AgentRole)> {
        let profile = self.profile_by_id(profile_id).ok_or_else(|| {
            RuntimeError::InvalidInput(format!("unknown or disabled agent profile `{profile_id}`"))
        })?;
        let role = match profile.default_model_role.as_deref() {
            Some(role) => role.parse::<AgentRole>().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "profile `{profile_id}` declares unsupported default_model_role `{role}`"
                ))
            })?,
            None => AgentRole::default(),
        };
        Ok((profile, role))
    }

    /// 校验调用方拥有创建子 Agent 的产品能力。
    pub(crate) async fn ensure_spawn_capability(&self, caller: AgentId) -> Result<()> {
        let profile = self.collaboration_caller_profile(caller).await?;
        if !profile.capabilities.spawn_agents {
            return Err(denied(caller, "spawn child agents"));
        }
        Ok(())
    }

    /// 校验调用方拥有向自己的子 Agent 投递消息的产品能力。
    pub(crate) async fn ensure_send_capability(&self, caller: AgentId) -> Result<()> {
        let profile = self.collaboration_caller_profile(caller).await?;
        if !communication_allows_children(profile.capabilities.communication.as_deref()) {
            return Err(denied(caller, "send messages to child agents"));
        }
        Ok(())
    }

    /// 校验调用方拥有关闭子 Agent 的产品能力。
    pub(crate) async fn ensure_close_capability(&self, caller: AgentId) -> Result<()> {
        let profile = self.collaboration_caller_profile(caller).await?;
        if !profile.capabilities.close_agents {
            return Err(denied(caller, "close child agents"));
        }
        Ok(())
    }

    /// 一个 Agent 的祖先链，从自身到根；出现环时显式失败而不是无限上溯。
    pub(crate) async fn collaboration_ancestry(&self, agent_id: AgentId) -> Result<Vec<AgentId>> {
        let mut path = Vec::new();
        let mut cursor = Some(agent_id);
        while let Some(id) = cursor {
            if path.contains(&id) {
                return Err(RuntimeError::InvalidInput(format!(
                    "agent ancestry of `{agent_id}` contains a cycle"
                )));
            }
            let record = self.agent(id).await?;
            let parent = record.summary.read().await.parent_id;
            path.push(id);
            cursor = parent;
        }
        Ok(path)
    }

    /// 校验 `target` 是 `caller` 的直接子 Agent。
    pub(crate) async fn ensure_direct_child(&self, caller: AgentId, target: AgentId) -> Result<()> {
        if caller == target {
            return Err(relationship_error(caller, target, "a direct child"));
        }
        let record = self.agent(target).await?;
        if record.summary.read().await.parent_id != Some(caller) {
            return Err(relationship_error(caller, target, "a direct child"));
        }
        Ok(())
    }

    /// 校验 `target` 是 `caller` 的严格后代（不等于 caller）。
    pub(crate) async fn ensure_descendant(&self, caller: AgentId, target: AgentId) -> Result<()> {
        if caller == target {
            return Err(relationship_error(caller, target, "a strict descendant"));
        }
        let ancestry = self.collaboration_ancestry(target).await?;
        if ancestry.iter().skip(1).any(|ancestor| *ancestor == caller) {
            Ok(())
        } else {
            Err(relationship_error(caller, target, "a strict descendant"))
        }
    }

    /// `root` 的严格子树，返回 `(深度, id)`；深度从 1 开始，同层按 id 升序。
    pub(crate) async fn agent_descendants(&self, root: AgentId) -> Vec<(usize, AgentId)> {
        let summaries = self.list_agents().await;
        let mut children: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
        for summary in &summaries {
            if let Some(parent_id) = summary.parent_id {
                children.entry(parent_id).or_default().push(summary.id);
            }
        }
        for kids in children.values_mut() {
            kids.sort_unstable();
        }

        let mut descendants = Vec::new();
        let mut seen = HashSet::new();
        seen.insert(root);
        let mut frontier = vec![(0_usize, root)];
        while let Some((depth, id)) = frontier.pop() {
            let Some(kids) = children.get(&id) else {
                continue;
            };
            for kid in kids {
                if !seen.insert(*kid) {
                    continue;
                }
                descendants.push((depth + 1, *kid));
                frontier.push((depth + 1, *kid));
            }
        }
        descendants.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        descendants
    }

    /// 列出 `caller` 自身与其严格子树；关系与状态都来自产品事实与驻留 Thread 首帧。
    pub(crate) async fn collaboration_list(
        &self,
        caller: AgentId,
    ) -> Result<Vec<CollaborationAgentRow>> {
        self.agent(caller).await?;
        let mut scoped = vec![(0_usize, caller)];
        scoped.extend(self.agent_descendants(caller).await);

        let mut rows = Vec::with_capacity(scoped.len());
        for (_, agent_id) in scoped {
            let record = self.agent(agent_id).await?;
            let summary = record.summary.read().await.clone();
            let (lifecycle, status) = match self.resident_thread(agent_id) {
                Some(resident) => {
                    let snapshot = resident.handle.snapshot();
                    (
                        Some(snapshot.lifecycle),
                        Some(thread_projection::status(&snapshot)),
                    )
                }
                None => (None, None),
            };
            rows.push(CollaborationAgentRow {
                id: agent_id.to_string(),
                parent_id: summary.parent_id.map(|parent| parent.to_string()),
                name: summary.name.clone(),
                role: summary.role,
                lifecycle,
                status,
            });
        }
        Ok(rows)
    }

    /// 幂等创建 child 产品 Agent、装配 canonical Thread 并提交稳定身份的初始消息。
    ///
    /// 失败时创建租约回收全部产品资源；child 已经存在时不会创建第二个 Agent，只在它空闲时重投
    /// 递同一条初始消息（core 按消息 id 返回原收据）。
    pub(crate) async fn spawn_child_agent(
        self: &Arc<Self>,
        request: SpawnChildRequest,
    ) -> Result<SpawnedChild> {
        let child_id = child_agent_id(request.caller, &request.call_id);
        if let Some(existing) = self.existing_child(child_id, &request).await? {
            let sequence = self.resume_existing_child(existing, &request).await?;
            return Ok(SpawnedChild {
                agent_id: existing,
                workspace: request.workspace,
                message_sequence: sequence,
            });
        }

        let parent = self.agent(request.caller).await?;
        let parent_summary = parent.summary.read().await.clone();
        // 父容器必须先驻留：child 容器从父容器克隆，父子 Thread 关系是产品事实。
        let parent_container_id = self.container_id(request.caller).await?;
        let resource = self
            .create_agent_resource(AgentCreationRequest {
                agent_id: child_id,
                request: CreateAgentRequest {
                    name: Some(request.task_summary.clone()),
                    provider_id: None,
                    model: None,
                    reasoning_effort: None,
                    docker_image: Some(parent_summary.docker_image.clone()),
                    parent_id: Some(request.caller),
                    system_prompt: Some(request.system_prompt.clone()),
                },
                container_source: agents::ContainerSource::CloneFrom {
                    parent_container_id,
                    docker_image: parent_summary.docker_image.clone(),
                    workspace_volume: None,
                },
                task_id: parent_summary.task_id,
                project_id: parent_summary.project_id,
                role: Some(request.role),
                identity: AgentCreationIdentity {
                    profile_id: Some(request.profile_id.clone()),
                    workspace: Some(request.workspace.clone()),
                },
            })
            .await?;
        self.register_prepared_child_agent(resource).await?;

        let resident = self.ensure_thread(child_id).await?;
        let sequence = resident
            .handle
            .send_message_and_continue(
                initial_child_message(request.caller, &request.call_id, &request.message),
                resident.input_driver,
            )
            .await?;
        Ok(SpawnedChild {
            agent_id: child_id,
            workspace: request.workspace,
            message_sequence: Some(sequence),
        })
    }

    /// 以 leaf→root 顺序关闭 `target` 的整棵子树。
    ///
    /// `Preserve` 保留产品记录与工作区（容器会停止，工作区卷可再次挂载）；`Cleanup` 额外删除
    /// 工作区卷与产品记录。调用方必须已经在更上层建立持久化屏障。
    pub(crate) async fn close_agent_tree(
        self: &Arc<Self>,
        target: AgentId,
        disposition: AgentWorkspaceDisposition,
    ) -> Result<()> {
        let order = descendant_close_order(target, self.agent_descendants(target).await);
        let cleanup = matches!(disposition, AgentWorkspaceDisposition::Cleanup);
        for agent_id in order {
            self.close_agent_keeping_workspace(agent_id).await?;
            let summary = self.agent(agent_id).await?.summary.read().await.clone();
            if summary.parent_id.is_some() && summary.review_run_id.is_none() {
                self.reconcile_child_reports(agent_id).await?;
            }
            if cleanup {
                self.cleanup_agent_workspace(agent_id).await?;
            }
        }
        if cleanup {
            // 记录、artifact 与删除事件由既有的产品清理路径统一处理。
            self.delete_agent(target).await?;
        }
        Ok(())
    }

    /// 解析一个已经存在的确定性 child；父子关系或 Profile 不一致时显式失败。
    async fn existing_child(
        &self,
        child_id: AgentId,
        request: &SpawnChildRequest,
    ) -> Result<Option<AgentId>> {
        let existing = { self.state.agents.read().await.get(&child_id).cloned() };
        let Some(record) = existing else {
            return Ok(None);
        };
        let summary = record.summary.read().await.clone();
        if summary.parent_id != Some(request.caller) {
            return Err(RuntimeError::InvalidInput(format!(
                "deterministic child id `{child_id}` is already used by an unrelated agent"
            )));
        }
        if summary.profile_id.as_deref() != Some(request.profile_id.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "deterministic child id `{child_id}` is already used by profile `{:?}`, not `{}`",
                summary.profile_id, request.profile_id
            )));
        }
        Ok(Some(child_id))
    }

    /// 已有的 child 不重新创建；只在它空闲时重投递同身份的初始消息。
    async fn resume_existing_child(
        &self,
        child_id: AgentId,
        request: &SpawnChildRequest,
    ) -> Result<Option<u64>> {
        let resident = self.ensure_thread(child_id).await?;
        let snapshot = resident.handle.snapshot();
        if snapshot.lifecycle != ThreadLifecycle::Open
            || thread_projection::status(&snapshot) != ThreadStatus::Idle
        {
            return Ok(None);
        }
        let message = initial_child_message(request.caller, &request.call_id, &request.message);
        self.await_agent_durable(child_id, snapshot.commit_sequence)
            .await?;
        if let Some((sequence, digest)) = self
            .accepted_message_identity(child_id, &message.id, &message.source_id)
            .await?
        {
            if digest != message.digest() {
                return Err(RuntimeError::InvalidInput(format!(
                    "child `{child_id}` accepted the initial message id with different content"
                )));
            }
            let woken = resident
                .handle
                .wake_accepted_message(&message.id, sequence, resident.input_driver)
                .await?;
            return Ok(woken.then_some(sequence));
        }
        let sequence = resident
            .handle
            .send_message_and_continue(message, resident.input_driver)
            .await?;
        Ok(Some(sequence))
    }

    /// 关闭一个 Agent 的 canonical Thread 与容器资源，保留产品记录与工作区。
    async fn close_agent_keeping_workspace(&self, agent_id: AgentId) -> Result<()> {
        self.state.thread_subscriptions.invalidate(agent_id).await;
        self.close_thread(agent_id).await?;
        self.close_agent(agent_id).await
    }

    fn profile_by_id(&self, profile_id: &str) -> Option<AgentProfile> {
        self.deps.agent_profiles.resolve(profile_id)
    }
}

/// 关系校验失败的产品错误。
fn relationship_error(caller: AgentId, target: AgentId, expected: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!(
        "agent `{target}` is not {expected} of caller `{caller}`"
    ))
}

/// 能力校验失败的产品错误。
fn denied(caller: AgentId, operation: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("agent `{caller}` is not permitted to {operation}"))
}

/// 由调用方与 `call_id` 派生确定性 child id；同一 `spawn_agent` 重试复用同一个身份。
pub(crate) fn child_agent_id(caller: AgentId, call_id: &str) -> AgentId {
    Uuid::new_v5(
        &CHILD_AGENT_NAMESPACE,
        format!("{caller}:{call_id}").as_bytes(),
    )
}

/// 构造 child 的稳定初始消息：身份只由 caller 与 call_id 决定，正文同时进入模型可见 context。
fn initial_child_message(caller: AgentId, call_id: &str, message: &str) -> ThreadMessage {
    ThreadMessage {
        id: format!("initial:{caller}:{call_id}"),
        source_id: format!("agent:{caller}"),
        kind: AgentMessageKind::Task,
        payload: OpaquePayload::text(message.to_string()),
        context: vec![ContextContent::Text {
            text: Arc::from(message),
        }],
    }
}

/// leaf→root 关闭顺序：深度降序，同深度按 id 升序，最后是子树根 `target`。
fn descendant_close_order(target: AgentId, descendants: Vec<(usize, AgentId)>) -> Vec<AgentId> {
    let mut ordered = descendants;
    ordered.push((0, target));
    ordered.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    ordered.into_iter().map(|(_, agent_id)| agent_id).collect()
}

/// Profile 的 `capabilities.communication` 是否允许向自己的子 Agent 投递消息。
///
/// `all` 覆盖整棵子树；`parent_and_maintainer` 只允许向上沟通，因此不能 send 到 child。
fn communication_allows_children(communication: Option<&str>) -> bool {
    matches!(communication, Some("all"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn child_agent_id_is_deterministic_and_call_scoped() {
        let caller = Uuid::new_v4();
        let other = Uuid::new_v4();

        assert_eq!(
            child_agent_id(caller, "call-1"),
            child_agent_id(caller, "call-1")
        );
        assert_ne!(
            child_agent_id(caller, "call-1"),
            child_agent_id(caller, "call-2")
        );
        assert_ne!(
            child_agent_id(caller, "call-1"),
            child_agent_id(other, "call-1")
        );
    }

    #[test]
    fn initial_child_message_keeps_stable_identity_and_agent_attribution() {
        let caller = Uuid::new_v4();

        let message = initial_child_message(caller, "call-1", "do the work");

        assert_eq!(message.id, format!("initial:{caller}:call-1"));
        assert_eq!(message.source_id, format!("agent:{caller}"));
        assert_eq!(message.payload.content(), "do the work");
        assert_eq!(
            message.context,
            vec![ContextContent::Text {
                text: Arc::from("do the work")
            }]
        );
    }

    #[test]
    fn close_order_closes_deepest_descendants_first() {
        let root = Uuid::new_v4();
        let older = Uuid::new_v4();
        let younger = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        let (first, second) = if older < younger {
            (older, younger)
        } else {
            (younger, older)
        };

        let order = descendant_close_order(root, vec![(1, younger), (2, grandchild), (1, older)]);

        assert_eq!(order, vec![grandchild, first, second, root]);
    }

    #[test]
    fn only_an_all_communication_profile_may_message_children() {
        assert!(communication_allows_children(Some("all")));
        assert!(!communication_allows_children(Some(
            "parent_and_maintainer"
        )));
        assert!(!communication_allows_children(None));
    }
}
