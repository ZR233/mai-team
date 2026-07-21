use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mai_protocol::{MaiProductEventEnvelope, MaiProductEventKind, now};
use mai_store::MaiStore;
use tokio::sync::{Mutex, broadcast};

pub(crate) const RECENT_EVENT_LIMIT: usize = 500;

pub(crate) struct RuntimeEvents {
    tx: broadcast::Sender<MaiProductEventEnvelope>,
    sequence: AtomicU64,
    recent: Mutex<VecDeque<MaiProductEventEnvelope>>,
    store: Arc<MaiStore>,
}

impl RuntimeEvents {
    pub(crate) fn new(
        store: Arc<MaiStore>,
        next_sequence: u64,
        recent_events: Vec<MaiProductEventEnvelope>,
    ) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            tx,
            sequence: AtomicU64::new(next_sequence),
            recent: Mutex::new(recent_events.into_iter().collect()),
            store,
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<MaiProductEventEnvelope> {
        self.tx.subscribe()
    }

    pub(crate) async fn publish(&self, kind: MaiProductEventKind) {
        let event = MaiProductEventEnvelope {
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst),
            timestamp: now(),
            kind,
        };
        if let Err(err) = self.store.append_product_event(&event).await {
            tracing::warn!("failed to persist product event: {err}");
        }
        {
            let mut recent = self.recent.lock().await;
            if recent.len() >= RECENT_EVENT_LIMIT {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.tx.send(event);
    }

    pub(crate) async fn retain_since(&self, cutoff: chrono::DateTime<chrono::Utc>) {
        self.recent
            .lock()
            .await
            .retain(|event| event.timestamp >= cutoff);
    }

    #[cfg(test)]
    pub(crate) async fn snapshot(&self) -> Vec<MaiProductEventEnvelope> {
        self.recent.lock().await.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mai_protocol::AgentResourceState;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn product_events_are_broadcast_and_persisted() {
        let dir = tempdir().expect("tempdir");
        let store = Arc::new(
            MaiStore::open_with_config_path(
                dir.path().join("runtime.sqlite3"),
                dir.path().join("config.toml"),
            )
            .await
            .expect("store"),
        );
        let events = RuntimeEvents::new(Arc::clone(&store), 1, Vec::new());
        let mut stream = events.subscribe();
        let agent_id = Uuid::new_v4();
        events
            .publish(MaiProductEventKind::OperationFailed {
                scope: "agent_resource".to_string(),
                agent_id: Some(agent_id),
                message: AgentResourceState::Failed.to_string(),
            })
            .await;

        let broadcast_completed = stream.recv().await.expect("product event");
        assert!(matches!(
            broadcast_completed.kind,
            MaiProductEventKind::OperationFailed { .. }
        ));

        let snapshot = events.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert!(matches!(
            snapshot[0].kind,
            MaiProductEventKind::OperationFailed { .. }
        ));

        let persisted = store.product_events_after(0, 10).await.expect("persisted");
        assert_eq!(persisted.len(), 1);
        assert!(matches!(
            persisted[0].kind,
            MaiProductEventKind::OperationFailed { .. }
        ));
        assert_eq!(persisted[0].sequence, 1);
    }
}
