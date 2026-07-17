mod events;
mod lifecycle;
mod policy;
mod protocol;
mod repository;
mod sessions;
mod trace_projection;
mod turn_factory;

use std::sync::{Arc, Weak};

use pl_core::AgentRuntimeHost;
use tokio::sync::RwLock;

use crate::{AgentRuntime, MaiConfig, RuntimeError};

pub(crate) use events::{MaiAgentEventSink, synchronize_runtime_state};
pub(crate) use lifecycle::MaiAgentLifecycle;
pub(crate) use policy::{MaiPolicyContext, compile_execution_policy};
pub(crate) use protocol::{protocol_uuid, runtime_state};
pub(crate) use repository::MaiAgentRepository;
pub(crate) use sessions::{
    ResolvedAgentSessionId, aggregate_usage, history_messages, last_assistant_response,
    load_runtime, project_sessions, selected_session, session_state,
};
pub(crate) use turn_factory::{MaiAgentTurnFactory, product_agent};

/// mai 对 PL agent framework 四个 host 端口的聚合实现。
#[derive(Clone)]
pub(crate) struct MaiAgentHost {
    repository: MaiAgentRepository,
    turn_factory: MaiAgentTurnFactory,
    lifecycle: MaiAgentLifecycle,
    events: MaiAgentEventSink,
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
            events: MaiAgentEventSink::new(runtime),
        }
    }
}

impl AgentRuntimeHost for MaiAgentHost {
    type Error = RuntimeError;
    type Repository = MaiAgentRepository;
    type TurnFactory = MaiAgentTurnFactory;
    type Lifecycle = MaiAgentLifecycle;
    type Events = MaiAgentEventSink;

    fn repository(&self) -> &Self::Repository {
        &self.repository
    }

    fn turn_factory(&self) -> &Self::TurnFactory {
        &self.turn_factory
    }

    fn lifecycle(&self) -> &Self::Lifecycle {
        &self.lifecycle
    }

    fn events(&self) -> &Self::Events {
        &self.events
    }
}
