use std::collections::HashMap;

use mai_protocol::AgentId;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct ThreadSubscriptionRegistry {
    threads: RwLock<HashMap<AgentId, ThreadSubscriptionLifecycle>>,
}

struct ThreadSubscriptionLifecycle {
    accepting: bool,
    cancellation: CancellationToken,
}

impl ThreadSubscriptionRegistry {
    pub(crate) async fn guard(&self, thread_id: AgentId) -> Option<CancellationToken> {
        let mut threads = self.threads.write().await;
        let lifecycle = threads
            .entry(thread_id)
            .or_insert_with(|| ThreadSubscriptionLifecycle {
                accepting: true,
                cancellation: CancellationToken::new(),
            });
        lifecycle
            .accepting
            .then(|| lifecycle.cancellation.child_token())
    }

    pub(crate) async fn invalidate(&self, thread_id: AgentId) {
        let mut threads = self.threads.write().await;
        let lifecycle = threads
            .entry(thread_id)
            .or_insert_with(|| ThreadSubscriptionLifecycle {
                accepting: false,
                cancellation: CancellationToken::new(),
            });
        lifecycle.accepting = false;
        lifecycle.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn invalidating_one_thread_only_terminates_its_subscriptions() {
        let registry = ThreadSubscriptionRegistry::default();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let first_guard = registry.guard(first).await.expect("first guard");
        let second_guard = registry.guard(second).await.expect("second guard");

        registry.invalidate(first).await;

        assert!(first_guard.is_cancelled());
        assert!(!second_guard.is_cancelled());
        assert!(registry.guard(first).await.is_none());
        assert!(registry.guard(second).await.is_some());
    }
}
