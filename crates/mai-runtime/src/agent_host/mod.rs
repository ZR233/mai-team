mod events;
mod lifecycle;
mod policy;
mod profile_catalog;
mod repository;
mod review_manifest;
mod thread;
mod trace_projection;
mod turn_factory;

use std::sync::{Arc, Weak};

use pl_core::AgentRuntimeHost;
use tokio::sync::RwLock;

use crate::{AgentRuntime, MaiConfig, RuntimeError};

pub(crate) use events::{MaiAgentCommitObserver, synchronize_runtime_state};
pub(crate) use lifecycle::MaiAgentLifecycle;
pub(crate) use policy::{MaiPolicyContext, compile_execution_policy};
pub(crate) use profile_catalog::{
    product_snapshot as product_agent_profile, resolve_route, snapshot as agent_profile,
    snapshots as agent_profiles, workspace_assignment as agent_workspace_assignment,
};
pub(crate) use repository::MaiAgentRepository;
pub(crate) use thread::{
    aggregate_usage, canonical_id, last_agent_response, load_runtime, product_thread_purpose,
    thread_metadata,
};
pub(crate) use turn_factory::{MaiAgentTurnFactory, product_agent};

/// mai 对 PL agent framework 四个 host 端口的聚合实现。
#[derive(Clone)]
pub(crate) struct MaiAgentHost {
    repository: MaiAgentRepository,
    turn_factory: MaiAgentTurnFactory,
    lifecycle: MaiAgentLifecycle,
    observer: MaiAgentCommitObserver,
}

impl MaiAgentHost {
    pub(crate) fn new(
        runtime: Weak<AgentRuntime>,
        store: Arc<mai_store::MaiStore>,
        config: Arc<RwLock<MaiConfig>>,
    ) -> Self {
        Self {
            repository: MaiAgentRepository::new(store),
            turn_factory: MaiAgentTurnFactory::new(runtime.clone(), config),
            lifecycle: MaiAgentLifecycle::new(runtime.clone()),
            observer: MaiAgentCommitObserver::new(runtime),
        }
    }

    pub(crate) async fn await_durable(
        &self,
        thread_id: &pl_core::ThreadId,
        revision: u64,
    ) -> crate::Result<()> {
        pl_core::ThreadRepository::await_durable(&self.repository, thread_id, revision).await
    }

    pub(crate) async fn restore_thread(
        &self,
        thread_id: &pl_core::ThreadId,
    ) -> crate::Result<Option<pl_core::RestoredAgentRuntime>> {
        pl_core::ThreadRepository::restore_thread(&self.repository, thread_id).await
    }

    pub(crate) async fn shutdown_repository(&self) -> crate::Result<()> {
        self.repository.shutdown().await
    }

    pub(crate) async fn wait_for_repository_failure(&self) -> crate::RuntimeError {
        self.repository.wait_for_failure().await
    }
}

impl AgentRuntimeHost for MaiAgentHost {
    type Error = RuntimeError;
    type Repository = MaiAgentRepository;
    type TurnFactory = MaiAgentTurnFactory;
    type Lifecycle = MaiAgentLifecycle;
    type Observer = MaiAgentCommitObserver;

    fn repository(&self) -> &Self::Repository {
        &self.repository
    }

    fn turn_factory(&self) -> &Self::TurnFactory {
        &self.turn_factory
    }

    fn lifecycle(&self) -> &Self::Lifecycle {
        &self.lifecycle
    }

    fn observer(&self) -> &Self::Observer {
        &self.observer
    }
}
