//! mai 产品 Thread 的装配与驻留注册器。
//!
//! 该模块只接收产品层已经解析好的模型路由、资源和冷存储句柄；不读取配置文件，
//! 也不感知任何会话 SQLite。注册器保持每个 id 对应一个 `ThreadHandle` 所有权，
//! 直到对应 Thread 的实际关闭成功。

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use pl_core::{
    context::{ContextRecord, ContextSnapshot, OpaquePayload, ResourceAccess},
    model::ModelFactory,
    thread::{
        ContextCapacity, ContextReplacementReason, ReplaceContext, ThreadCheckpoint, ThreadError,
        ThreadHandle, ThreadLifecycle, cold::ColdStoreHandle, context_preparation::ContextPreparer,
        extensions::ExtensionMutation, input::InputDriverOptions,
    },
    tool::opaque::Registration,
};
use pl_model::{
    PureError,
    config::ResolvedModelRoute,
    runtime::{HostedTool, ModelRuntime, ThreadModel},
};
use tokio::sync::Notify;

/// 产品层已经解析完成的 Thread 装配输入。
///
/// `registrations` 中的工具执行器会在装配时一次性转移给 Thread，不能复制或复用。
/// `route` 由产品层负责解析；已持久化 Thread 的模型配置不可用时为 `None`，
/// 仍可恢复历史并等待后续模型替换。新 Thread 必须提供可用路由。
pub struct ThreadSpec {
    pub id: String,
    pub checkpoint: Option<ThreadCheckpoint>,
    pub route: Option<ResolvedModelRoute>,
    pub hosted_tools: Vec<HostedTool>,
    pub context_preparation: Option<ContextPreparer>,
    pub initial_context: Vec<ContextRecord>,
    pub initial_extensions: BTreeMap<String, OpaquePayload>,
    pub registrations: Vec<Registration>,
    pub resources: ResourceAccess,
    pub capacity: ContextCapacity,
    pub cold_store: Option<ColdStoreHandle>,
    pub input_driver: InputDriverOptions,
}

impl fmt::Debug for ThreadSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadSpec")
            .field("id", &self.id)
            .field("model_available", &self.route.is_some())
            .field("hosted_tools", &self.hosted_tools.len())
            .field(
                "checkpoint_revision",
                &self
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.state_revision),
            )
            .field("initial_context", &self.initial_context.len())
            .field("initial_extensions", &self.initial_extensions.len())
            .field("registrations", &self.registrations.len())
            .field("has_cold_store", &self.cold_store.is_some())
            .finish_non_exhaustive()
    }
}

/// 一个已经完整发布的驻留 Thread 访问记录。
///
/// `handle` 是指向同一 owner incarnation 的克隆；注册器仍保留一份句柄，保证
/// 实际关闭失败时资源不会被提前丢弃。`input_driver` 是产品层为该 Thread 选择的
/// 队列执行参数，调用方应在提交输入时显式使用它，而不是让注册器额外驱动输入。
#[derive(Debug, Clone)]
pub struct ResidentThread {
    pub handle: ThreadHandle,
    pub input_driver: InputDriverOptions,
}

/// Thread 装配、驻留访问和关闭过程中的错误。
#[derive(Debug, thiserror::Error)]
pub enum ThreadKernelError {
    #[error("Thread kernel is closed")]
    Closed,
    #[error("Thread identity is invalid or already resident: {0}")]
    Identity(String),
    #[error("initial context cannot replace restored Thread history during assembly")]
    InitialContextOnRecovery,
    #[error("a new Thread requires an available model route")]
    NewThreadWithoutModel,
    #[error("Thread is still being assembled: {0}")]
    Preparing(String),
    #[error("model binding construction failed")]
    Binding(#[from] PureError),
    #[error("model session construction failed")]
    Model(#[from] pl_core::model::ModelError),
    #[error("Thread setup or close failed")]
    Thread(#[from] ThreadError),
    #[error("Thread {id} setup failed ({setup}); cleanup also failed ({cleanup})")]
    Cleanup {
        id: String,
        setup: Box<ThreadError>,
        cleanup: Box<ThreadError>,
    },
}

#[derive(Debug)]
struct Entry {
    incarnation: Arc<()>,
    input_driver: InputDriverOptions,
    ready: bool,
    thread: ThreadHandle,
}

#[derive(Debug, Default)]
struct State {
    closing: bool,
    creating: BTreeSet<String>,
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Default)]
struct Registry {
    state: Mutex<State>,
    changed: Notify,
}

impl Registry {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Reservation {
    registry: Arc<Registry>,
    id: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.registry.state().creating.remove(&self.id);
        self.registry.changed.notify_waiters();
    }
}

/// 按产品 Thread id 装配并驻留唯一 owner 的注册器。
///
/// 克隆 `ThreadKernel` 只共享同一个注册表。注册器自身不提供旧 AgentRuntime 或
/// TurnEngine 兼容门面；执行入口由调用方使用返回的 `ThreadHandle` 接线。
#[derive(Debug, Clone, Default)]
pub struct ThreadKernel(Arc<Registry>);

impl ThreadKernel {
    /// 校验输入、打开模型会话、绑定资源并发布一个 Thread。
    ///
    /// 装配遵循 canonical 顺序：context preparation、resources、capacity、工具注册、
    /// 冷存储、扩展和初始 context。任何一步失败都会先关闭未发布 owner；关闭失败时
    /// 保留注册项供调用方重试，避免掩盖资源清理错误。
    ///
    /// # Errors
    /// 拒绝空 id、重复/并发 id、checkpoint 身份不匹配、恢复时携带 initial context，
    /// 以及模型、Thread 或工具注册失败。装配失败清理失败会以 [`ThreadKernelError::Cleanup`]
    /// 返回并保留驻留项。
    pub async fn assemble(&self, spec: ThreadSpec) -> Result<ResidentThread, ThreadKernelError> {
        let reservation = self.reserve(&spec.id)?;
        self.assemble_reserved(spec, &reservation).await
    }

    /// 只返回已经完整发布且仍处于 Open 生命周期的驻留 Thread。
    ///
    /// 关闭中的内核、未完成装配的 entry 或已经关闭的 owner 都不会暴露给调用方。
    pub fn lookup(&self, id: &str) -> Option<ResidentThread> {
        let state = self.0.state();
        if state.closing {
            return None;
        }
        state
            .entries
            .get(id)
            .filter(|entry| {
                entry.ready && entry.thread.snapshot().lifecycle == ThreadLifecycle::Open
            })
            .map(|entry| ResidentThread {
                handle: entry.thread.clone(),
                input_driver: entry.input_driver,
            })
    }

    /// 关闭并移除一个驻留 Thread。
    ///
    /// # Errors
    /// 返回仍在装配中的 id 或实际 Thread 关闭失败。关闭失败时 entry 会保留，
    /// 后续可以用同一个方法重试。
    pub async fn close(&self, id: &str) -> Result<(), ThreadKernelError> {
        if self.0.state().creating.contains(id) {
            return Err(ThreadKernelError::Preparing(id.to_owned()));
        }
        let candidate = {
            let mut state = self.0.state();
            state.entries.get_mut(id).map(|entry| {
                entry.ready = false;
                (entry.incarnation.clone(), entry.thread.clone())
            })
        };
        let Some((incarnation, thread)) = candidate else {
            return Ok(());
        };

        if thread.snapshot().lifecycle != ThreadLifecycle::Closed {
            let closed = thread.close().await;
            if let Err(error) = closed
                && thread.snapshot().lifecycle != ThreadLifecycle::Closed
            {
                return Err(error.into());
            }
        }

        let removed = {
            let mut state = self.0.state();
            if state
                .entries
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.incarnation, &incarnation))
            {
                state.entries.remove(id)
            } else {
                None
            }
        };
        drop(removed);
        Ok(())
    }

    /// 封闭装配入口，等待在途装配结束，并按 id 顺序关闭全部驻留 Thread。
    ///
    /// 返回所有仍需重试的失败项；失败项会继续保留在注册器中。内核保持封闭状态，
    /// 后续再次调用 `shutdown` 只会重试剩余 entry。
    pub async fn shutdown(&self) -> Vec<(String, ThreadKernelError)> {
        self.0.state().closing = true;
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.0.state().creating.is_empty() {
                break;
            }
            changed.await;
        }

        let ids = self.0.state().entries.keys().cloned().collect::<Vec<_>>();
        let mut failures = Vec::new();
        for id in ids {
            if let Err(error) = self.close(&id).await {
                failures.push((id, error));
            }
        }
        failures
    }

    fn reserve(&self, id: &str) -> Result<Reservation, ThreadKernelError> {
        if id.is_empty() {
            return Err(ThreadKernelError::Identity(id.to_owned()));
        }
        let mut state = self.0.state();
        if state.closing {
            return Err(ThreadKernelError::Closed);
        }
        if state.creating.contains(id) || state.entries.contains_key(id) {
            return Err(ThreadKernelError::Identity(id.to_owned()));
        }
        state.creating.insert(id.to_owned());
        Ok(Reservation {
            registry: self.0.clone(),
            id: id.to_owned(),
        })
    }

    async fn assemble_reserved(
        &self,
        spec: ThreadSpec,
        reservation: &Reservation,
    ) -> Result<ResidentThread, ThreadKernelError> {
        if spec.id != reservation.id || spec.id.is_empty() {
            return Err(ThreadKernelError::Identity(spec.id));
        }
        if spec
            .checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.thread_id != spec.id)
        {
            return Err(ThreadKernelError::Identity(spec.id));
        }
        if spec.checkpoint.is_some() && !spec.initial_context.is_empty() {
            return Err(ThreadKernelError::InitialContextOnRecovery);
        }
        if spec.checkpoint.is_none() && spec.route.is_none() {
            return Err(ThreadKernelError::NewThreadWithoutModel);
        }
        let recovered = spec.checkpoint.is_some();
        let has_model = spec.route.is_some();
        ContextSnapshot {
            revision: 0,
            records: spec.initial_context.clone().into(),
        }
        .validate_complete()
        .map_err(ThreadError::from)?;

        let thread = if let Some(route) = &spec.route {
            let model =
                ThreadModel::new(ModelRuntime::from_route(route)?, route.reasoning_config())
                    .with_hosted_tools(spec.hosted_tools);
            ThreadHandle::resume(
                spec.id.clone(),
                ModelFactory::new(model).open_session().await?,
                spec.checkpoint,
            )?
        } else {
            ThreadHandle::resume_without_model(spec.id.clone(), spec.checkpoint)?
        };

        let thread_id = spec.id.clone();
        let input_driver = spec.input_driver;
        let incarnation = Arc::new(());
        {
            let mut state = self.0.state();
            state.entries.insert(
                thread_id.clone(),
                Entry {
                    incarnation: incarnation.clone(),
                    input_driver,
                    ready: false,
                    thread: thread.clone(),
                },
            );
        }

        let registry = self.0.clone();
        let owner = thread.clone();
        let setup_thread_id = thread_id.clone();
        let ThreadSpec {
            context_preparation,
            initial_context,
            initial_extensions,
            registrations,
            resources,
            capacity,
            cold_store,
            ..
        } = spec;
        let setup = async move {
            owner.set_context_preparation(context_preparation).await?;
            owner.set_resources(resources).await?;
            owner.set_capacity(capacity).await?;
            owner.register_tools(registrations).await?;
            if let Some(store) = cold_store {
                owner.attach_storage(store).await?;
            }
            if !initial_extensions.is_empty() {
                owner
                    .mutate_extensions(
                        initial_extensions
                            .into_iter()
                            .map(|(id, payload)| ExtensionMutation::Put {
                                id,
                                expected_revision: None,
                                payload,
                            })
                            .collect(),
                    )
                    .await?;
            }
            if !initial_context.is_empty() {
                owner
                    .replace_context(ReplaceContext {
                        expected_revision: 0,
                        reason: ContextReplacementReason::Rebuild,
                        records: initial_context,
                    })
                    .await?;
            }
            if recovered && has_model {
                // 恢复 checkpoint 中已受理但未消费的输入；执行与幂等状态仍由 PL 持有。
                owner.resume_inputs(input_driver).await?;
            }

            let mut state = registry.state();
            if state.closing {
                return Err(ThreadError::Closed);
            }
            let entry = state
                .entries
                .get_mut(&setup_thread_id)
                .ok_or(ThreadError::Closed)?;
            entry.ready = true;
            Ok::<_, ThreadError>(())
        }
        .await;

        if let Err(setup) = setup {
            match thread.close().await {
                Ok(()) => {
                    let removed = self.0.state().entries.remove(&thread_id);
                    drop(removed);
                }
                Err(cleanup) => {
                    return Err(ThreadKernelError::Cleanup {
                        id: thread_id,
                        setup: Box::new(setup),
                        cleanup: Box::new(cleanup),
                    });
                }
            }
            return Err(ThreadKernelError::Thread(setup));
        }

        Ok(ResidentThread {
            handle: thread,
            input_driver,
        })
    }
}
