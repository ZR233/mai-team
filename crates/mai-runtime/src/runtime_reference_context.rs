//! 模型事实仅取自实际 applied snapshot；同一 Turn 的事实保持冻结。

use mai_protocol::{AgentId, AppliedReferenceSnapshot};
use mai_references::ReferenceApplications;
use pl_core::{
    context::ContextContent,
    thread::{
        RuntimeFact,
        context_preparation::{
            ContextPreparation, ContextPreparationHook, ContextPreparationRequest,
        },
    },
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub(super) struct ReferenceContextPreparer {
    agent_id: AgentId,
    applications: Arc<ReferenceApplications>,
    frozen: Mutex<Option<(String, Arc<AppliedReferenceSnapshot>)>>,
}

impl std::fmt::Debug for ReferenceContextPreparer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceContextPreparer")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl ReferenceContextPreparer {
    pub(super) fn new(agent_id: AgentId, applications: Arc<ReferenceApplications>) -> Self {
        Self {
            agent_id,
            applications,
            frozen: Mutex::new(None),
        }
    }
}

impl ContextPreparationHook for ReferenceContextPreparer {
    async fn before_step(&self, request: ContextPreparationRequest) -> ContextPreparation {
        let turn_id = request.model.turn_id.to_string();
        let mut frozen = self.frozen.lock().await;
        if frozen
            .as_ref()
            .is_none_or(|(current, _)| *current != turn_id)
        {
            match self.applications.leases.acquire(self.agent_id).await {
                Ok(lease) => *frozen = Some((turn_id, lease.shared_references())),
                Err(error) => {
                    return ContextPreparation::Failed {
                        error: pl_core::model::ModelError {
                            kind: pl_core::model::ModelFailureKind::Unavailable,
                            details: None,
                            usage: Box::default(),
                            source: Some(Box::new(error)),
                        },
                        mutations: Vec::new(),
                    };
                }
            }
        }
        let snapshot = &frozen.as_ref().expect("本轮快照已冻结").1;
        let mut facts = request
            .current_facts
            .iter()
            .filter(|fact| fact.source_id != "mai.references")
            .cloned()
            .collect::<Vec<_>>();
        if let Some(text) = mai_references::render_model_manifest(snapshot) {
            facts.push(RuntimeFact {
                source_id: "mai.references".into(),
                content: vec![ContextContent::Text { text: text.into() }],
            });
        }
        ContextPreparation::Prepared {
            expected_extension_sequence: request.extension_sequence,
            rejected_mutations: Vec::new(),
            replacement: None,
            facts,
            mutations: Vec::new(),
        }
    }
}
