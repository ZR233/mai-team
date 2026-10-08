//! mai 产品侧的 PL 协作 host 与 Thread 注册项。
//!
//! [`MaiAgentControlHost`] 实现 [`pl_tool::collaboration::thread::AgentControlHost`]：它把 PL
//! 工具前端已经解析完成、身份已经确定的事实翻译成 mai 的运行时调用，最终产出与错误都保留
//! 原始身份。host 只持 [`AgentRuntime`] 的弱引用，因为它的注册项由 Thread 内核持有，强引用会
//! 与 Thread 成环。
//!
//! `spawn` / `send` / `list` / `interrupt` / `close` 的父子关系、Profile 能力、工作区处置与
//! 持久化屏障全部由 [`crate::runtime_agent_collaboration`] 判定；本模块不重新实现任何协作语义，
//! 也不把消息降级成用户输入。

use std::sync::{Arc, Weak};

use pl_core::context::{ContextContent, OpaquePayload};
use pl_core::thread::inbox::ThreadMessage;
use pl_core::tool::ToolOutput;
use pl_core::tool::opaque::{CallContext, Registration, ToolError};
use pl_protocol::AgentWorkspaceAssignmentSnapshot;
use pl_tool::collaboration::thread::{
    AgentControlHost, AgentMessage, AgentSpawn, AgentWorkspaceDisposition,
};
use serde_json::json;
use uuid::Uuid;

use crate::runtime_agent_collaboration::{SpawnChildRequest, child_agent_id};
use crate::turn::tool_sets::{ThreadCollaborationTools, collaboration_registrations};
use crate::{AgentRuntime, Result};

use super::child_workspace_assignment;

/// 协作工具产出的产品编码；payload 与模型可见文本同源，避免两种视图分叉。
const AGENT_CONTROL_FORMAT: &str = "mai.agent-control";
const AGENT_CONTROL_VERSION: u32 = 1;

/// 一个普通产品 Thread 的协作 host。
///
/// 只持 [`AgentRuntime`] 的弱引用：工具注册项随 Thread 生命周期存续，强引用会让 Thread 内核
/// 无法被回收。
pub(crate) struct MaiAgentControlHost {
    runtime: Weak<AgentRuntime>,
}

impl MaiAgentControlHost {
    pub(crate) fn new(runtime: &Arc<AgentRuntime>) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
        }
    }

    fn runtime(&self) -> std::result::Result<Arc<AgentRuntime>, ToolError> {
        self.runtime
            .upgrade()
            .ok_or_else(|| ToolError::new(AgentControlError::RuntimeClosed))
    }
}

impl std::fmt::Debug for MaiAgentControlHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MaiAgentControlHost")
    }
}

/// 每个普通 Thread 独占的协作工具注册项。
pub(crate) struct MaiThreadCollaboration {
    host: Arc<MaiAgentControlHost>,
}

impl MaiThreadCollaboration {
    pub(crate) fn new(runtime: &Arc<AgentRuntime>) -> Self {
        Self {
            host: Arc::new(MaiAgentControlHost::new(runtime)),
        }
    }
}

impl std::fmt::Debug for MaiThreadCollaboration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MaiThreadCollaboration")
    }
}

impl ThreadCollaborationTools for MaiThreadCollaboration {
    fn registrations(&self) -> Result<Vec<Registration>> {
        collaboration_registrations(Arc::clone(&self.host))
    }
}

/// 协作 host 自身的失败；产品错误一律以 [`crate::RuntimeError`] 原样保留。
#[derive(Debug, thiserror::Error)]
enum AgentControlError {
    #[error("mai runtime is closed; agent control is unavailable")]
    RuntimeClosed,
    #[error("invalid agent identity `{0}`")]
    InvalidIdentity(String),
}

/// 一次 `spawn_agent` 的收据；`messageAccepted` 只表示初始消息被受理，不表示 child 已完工。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SpawnReceipt<'a> {
    agent_id: String,
    profile_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<&'a AgentWorkspaceAssignmentSnapshot>,
    message_accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_sequence: Option<u64>,
}

impl AgentControlHost for MaiAgentControlHost {
    async fn spawn(
        &self,
        context: CallContext,
        request: AgentSpawn,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let runtime = self.runtime()?;
        let caller = parse_agent_id(&context.thread_id)?;
        // 持久化屏障：child 创建是不可逆副作用，必须等调用方在本次调用之前已提交的 effect
        // 全部 durable 之后才开始。
        runtime
            .await_agent_durable(caller, context.history_fence)
            .await
            .map_err(ToolError::new)?;
        runtime
            .ensure_spawn_capability(caller)
            .await
            .map_err(ToolError::new)?;

        let (child_profile, role) = runtime
            .resolve_collaboration_profile(&request.profile_id)
            .map_err(ToolError::new)?;
        let parent = runtime.agent(caller).await.map_err(ToolError::new)?;
        let parent_summary = parent.summary.read().await.clone();
        let workspace =
            child_workspace_assignment(&parent_summary, role, request.writable_paths.as_deref())
                .map_err(ToolError::new)?;
        let profile_id = request.profile_id.clone();

        let child_request = SpawnChildRequest {
            caller,
            call_id: context.call_id.clone(),
            profile_id: profile_id.clone(),
            role,
            system_prompt: child_profile.prompt.clone(),
            task_summary: request.task_summary.as_str().to_string(),
            message: request.message.clone(),
            workspace: workspace.clone(),
        };

        match runtime.spawn_child_agent(child_request).await {
            Ok(child) => encode_output(&SpawnReceipt {
                agent_id: child.agent_id.to_string(),
                profile_id: &profile_id,
                workspace: Some(&child.workspace),
                message_accepted: true,
                message_sequence: child.message_sequence,
            }),
            Err(error) => {
                // 失败时仍然把确定的 child 身份与冻结的工作区事实交给模型，同时保留原始错误。
                let receipt = encode_output(&SpawnReceipt {
                    agent_id: child_agent_id(caller, &context.call_id).to_string(),
                    profile_id: &profile_id,
                    workspace: Some(&workspace),
                    message_accepted: false,
                    message_sequence: None,
                })?;
                Err(ToolError::new(error).with_output(receipt))
            }
        }
    }

    async fn send(
        &self,
        caller: &str,
        message: AgentMessage,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let runtime = self.runtime()?;
        let caller_id = parse_agent_id(caller)?;
        let target = parse_agent_id(&message.target)?;
        runtime
            .ensure_send_capability(caller_id)
            .await
            .map_err(ToolError::new)?;
        runtime
            .ensure_direct_child(caller_id, target)
            .await
            .map_err(ToolError::new)?;
        let resident = runtime
            .ensure_thread(target)
            .await
            .map_err(ToolError::new)?;
        // 必须原样传递 pl-tool 分配的消息身份：core 用它做幂等与冲突判定，host 不重新编号。
        let delivery = ThreadMessage {
            id: message.id.clone(),
            source_id: format!("agent:{caller}"),
            kind: pl_core::context::AgentMessageKind::Task,
            payload: OpaquePayload::text(message.message.clone()),
            context: vec![ContextContent::Text {
                text: Arc::from(message.message),
            }],
        };
        let sequence = resident
            .handle
            .send_message_and_continue(delivery, resident.input_driver)
            .await
            .map_err(ToolError::new)?;
        encode_output(&json!({
            "target": message.target,
            "messageId": message.id,
            "sequence": sequence,
        }))
    }

    async fn list(&self, caller: &str) -> std::result::Result<ToolOutput, ToolError> {
        let runtime = self.runtime()?;
        let caller_id = parse_agent_id(caller)?;
        // 只解析一次调用方 Profile：既确认调用方存在，也确认它仍是可协作的产品 Agent。
        runtime
            .collaboration_caller_profile(caller_id)
            .await
            .map_err(ToolError::new)?;
        let rows = runtime
            .collaboration_list(caller_id)
            .await
            .map_err(ToolError::new)?;
        encode_output(&rows)
    }

    async fn interrupt(
        &self,
        caller: &str,
        target: &str,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let runtime = self.runtime()?;
        let caller_id = parse_agent_id(caller)?;
        let target_id = parse_agent_id(target)?;
        runtime
            .ensure_descendant(caller_id, target_id)
            .await
            .map_err(ToolError::new)?;
        let resident = runtime
            .ensure_thread(target_id)
            .await
            .map_err(ToolError::new)?;
        let interrupted = resident
            .handle
            .interrupt_turn(None)
            .await
            .map_err(ToolError::new)?;
        encode_output(&json!({
            "target": target,
            "interrupted": interrupted,
        }))
    }

    async fn close(
        &self,
        caller: &str,
        target: &str,
        disposition: AgentWorkspaceDisposition,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let runtime = self.runtime()?;
        let caller_id = parse_agent_id(caller)?;
        let target_id = parse_agent_id(target)?;
        runtime
            .ensure_close_capability(caller_id)
            .await
            .map_err(ToolError::new)?;
        runtime
            .ensure_descendant(caller_id, target_id)
            .await
            .map_err(ToolError::new)?;
        // `close` 端口没有 history_fence：以调用方 Thread 在工具开始时的提交水位作为固定屏障，
        // 等它 durable 之后再执行不可逆的工作区清理。
        if let Some(resident) = runtime.resident_thread(caller_id) {
            let revision = resident.handle.snapshot().commit_sequence;
            runtime
                .await_agent_durable(caller_id, revision)
                .await
                .map_err(ToolError::new)?;
        }
        runtime
            .close_agent_tree(target_id, disposition)
            .await
            .map_err(ToolError::new)?;
        encode_output(&json!({
            "target": target,
            "lifecycle": pl_core::thread::ThreadLifecycle::Closed,
            "workspaceDisposition": disposition,
        }))
    }
}

/// 把工具的 Thread 身份解析成产品 AgentId；非法身份是显式错误，不做任何猜测。
fn parse_agent_id(value: &str) -> std::result::Result<Uuid, ToolError> {
    Uuid::parse_str(value)
        .map_err(|_| ToolError::new(AgentControlError::InvalidIdentity(value.to_string())))
}

/// 用协作工具的统一编码产出一条 [`ToolOutput`]。
fn encode_output(value: &impl serde::Serialize) -> std::result::Result<ToolOutput, ToolError> {
    let encoded = serde_json::to_string(value).map_err(ToolError::new)?;
    let payload = OpaquePayload::new(AGENT_CONTROL_FORMAT, AGENT_CONTROL_VERSION, encoded.clone())
        .map_err(ToolError::new)?;
    Ok(ToolOutput::new(
        payload,
        vec![ContextContent::Text {
            text: Arc::from(encoded),
        }],
    ))
}
