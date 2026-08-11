use std::collections::HashMap;

use mai_protocol::{
    AgentId, AgentResourceState, AgentSummary, ThreadSnapshot, ThreadSubscriptionRequest,
    ThreadSubscriptionUpdate, ThreadTurnPage,
};
use tokio_util::sync::CancellationToken;

use crate::{AgentRuntime, Result, RuntimeError};

/// 产品权限校验后直接持有的 PL Thread subscription。
#[derive(Debug)]
pub struct MaiThreadEventSubscription {
    inner: pl_core::ThreadEventSubscription,
    thread_id: String,
    lifecycle: CancellationToken,
    validator: ThreadUpdateValidator,
}

impl MaiThreadEventSubscription {
    /// 接收同一 Thread 的 authoritative snapshot 或 notification。
    pub async fn recv(&mut self) -> Option<pl_protocol::ThreadSubscriptionUpdate> {
        let update = tokio::select! {
            _ = self.lifecycle.cancelled() => None,
            update = self.inner.recv() => update,
        }?;
        if let Err(error) = self.validator.validate(&self.thread_id, &update) {
            tracing::warn!(thread_id = %self.thread_id, %error, "closing invalid Thread subscription");
            return None;
        }
        Some(update)
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

impl AgentRuntime {
    /// 校验产品 Thread 所有权后建立隔离的 PL subscription。
    pub async fn subscribe_thread(&self, thread_id: String) -> Result<MaiThreadEventSubscription> {
        let product_agent_id = parse_thread_agent_id(&thread_id)?;
        let agent = self.agent(product_agent_id).await?;
        let summary = agent.summary.read().await.clone();
        ensure_readable_product_thread(&summary)?;
        let lifecycle = self
            .state
            .thread_subscriptions
            .guard(product_agent_id)
            .await
            .ok_or_else(|| RuntimeError::ThreadNotFound(thread_id.clone()))?;
        let framework_id = pl_core::ThreadId::new(thread_id.clone())?;
        let snapshot = self
            .framework_handle()?
            .snapshot(framework_id.clone())
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        ensure_live_canonical_thread(&snapshot)?;
        let mut inner = self
            .framework_handle()?
            .subscribe_thread(ThreadSubscriptionRequest {
                thread_id: thread_id.clone(),
            })
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        inner
            .replace_bootstrap_thread(crate::agent_host::thread_metadata(&summary, &snapshot))
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        Ok(MaiThreadEventSubscription {
            inner,
            thread_id,
            lifecycle,
            validator: ThreadUpdateValidator::default(),
        })
    }

    /// 读取一个产品 Thread 的 authoritative snapshot。
    pub async fn thread_snapshot(&self, thread_id: String) -> Result<ThreadSnapshot> {
        let product_agent_id = parse_thread_agent_id(&thread_id)?;
        let agent = self.agent(product_agent_id).await?;
        let summary = agent.summary.read().await.clone();
        ensure_readable_product_thread(&summary)?;
        let framework_id = pl_core::ThreadId::new(thread_id)?;
        let snapshot = self
            .framework_handle()?
            .snapshot(framework_id.clone())
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        ensure_live_canonical_thread(&snapshot)?;
        let mut thread = self
            .framework_handle()?
            .thread_snapshot(&framework_id)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        thread.thread = crate::agent_host::thread_metadata(&summary, &snapshot);
        Ok(thread)
    }

    /// 从 durable store 分页读取一个 Thread 的 Turn history。
    pub async fn thread_turns(
        &self,
        thread_id: String,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ThreadTurnPage> {
        let product_agent_id = parse_thread_agent_id(&thread_id)?;
        let agent = self.agent(product_agent_id).await?;
        let summary = agent.summary.read().await;
        ensure_readable_product_thread(&summary)?;
        Ok(self
            .deps
            .store
            .list_thread_turns(&thread_id, cursor, limit)
            .await?)
    }
}

#[derive(Debug, Default)]
struct ThreadUpdateValidator {
    revision: Option<u64>,
    items: HashMap<String, ItemRevision>,
}

#[derive(Debug, Clone)]
struct ItemRevision {
    thread_id: String,
    turn_id: String,
    revision: u64,
}

impl ThreadUpdateValidator {
    fn validate(&mut self, thread_id: &str, update: &ThreadSubscriptionUpdate) -> Result<()> {
        match update {
            ThreadSubscriptionUpdate::Snapshot { snapshot } => {
                if self.revision.is_some() {
                    return invalid_thread_update("subscription emitted more than one snapshot");
                }
                if snapshot.thread.id != thread_id {
                    return invalid_thread_update(format!(
                        "snapshot belongs to {}, expected {thread_id}",
                        snapshot.thread.id
                    ));
                }
                if snapshot
                    .active_turn
                    .as_ref()
                    .is_some_and(|turn| turn.thread_id != thread_id)
                {
                    return invalid_thread_update("snapshot active Turn crossed Thread ownership");
                }
                let mut items = HashMap::with_capacity(snapshot.items.len());
                for item in &snapshot.items {
                    if item.thread_id != thread_id {
                        return invalid_thread_update(format!(
                            "Item {} belongs to another Thread",
                            item.id
                        ));
                    }
                    if items
                        .insert(
                            item.id.clone(),
                            ItemRevision {
                                thread_id: item.thread_id.clone(),
                                turn_id: item.turn_id.clone(),
                                revision: item.revision,
                            },
                        )
                        .is_some()
                    {
                        return invalid_thread_update(format!(
                            "snapshot contains duplicate Item {}",
                            item.id
                        ));
                    }
                }
                self.items = items;
                self.revision = Some(snapshot.revision);
            }
            ThreadSubscriptionUpdate::Notification { notification } => {
                if notification.thread_id != thread_id {
                    return invalid_thread_update(format!(
                        "notification belongs to {}, expected {thread_id}",
                        notification.thread_id
                    ));
                }
                let Some(revision) = self.revision else {
                    return invalid_thread_update("notification arrived before snapshot");
                };
                if matches!(
                    notification.notification,
                    mai_protocol::ThreadNotification::Lagged { .. }
                ) {
                    return Ok(());
                }
                let expected = revision.saturating_add(1);
                if notification.revision != expected {
                    return invalid_thread_update(format!(
                        "Thread revision gap: expected {expected}, got {}",
                        notification.revision
                    ));
                }
                self.validate_notification(thread_id, &notification.notification)?;
                self.revision = Some(notification.revision);
            }
        }
        Ok(())
    }

    fn validate_notification(
        &mut self,
        thread_id: &str,
        notification: &mai_protocol::ThreadNotification,
    ) -> Result<()> {
        match notification {
            mai_protocol::ThreadNotification::TurnStarted { turn }
            | mai_protocol::ThreadNotification::TurnUpdated { turn }
            | mai_protocol::ThreadNotification::TurnCompleted { turn } => {
                if turn.thread_id != thread_id {
                    return invalid_thread_update("Turn notification crossed Thread ownership");
                }
            }
            mai_protocol::ThreadNotification::ItemStarted { item }
            | mai_protocol::ThreadNotification::ItemCompleted { item } => {
                if item.thread_id != thread_id {
                    return invalid_thread_update(format!(
                        "Item {} crossed Thread ownership",
                        item.id
                    ));
                }
                if let Some(current) = self.items.get(&item.id) {
                    if current.thread_id != item.thread_id || current.turn_id != item.turn_id {
                        return invalid_thread_update(format!(
                            "Item {} crossed Thread or Turn ownership",
                            item.id
                        ));
                    }
                    if item.revision < current.revision {
                        return invalid_thread_update(format!(
                            "Item {} revision regressed from {} to {}",
                            item.id, current.revision, item.revision
                        ));
                    }
                }
                self.items.insert(
                    item.id.clone(),
                    ItemRevision {
                        thread_id: item.thread_id.clone(),
                        turn_id: item.turn_id.clone(),
                        revision: item.revision,
                    },
                );
            }
            mai_protocol::ThreadNotification::ItemDelta { delta } => {
                let Some(item) = self.items.get_mut(&delta.item_id) else {
                    return invalid_thread_update(format!(
                        "delta references missing Item {}",
                        delta.item_id
                    ));
                };
                let expected = item.revision.saturating_add(1);
                if delta.revision != expected {
                    return invalid_thread_update(format!(
                        "Item {} revision gap: expected {expected}, got {}",
                        delta.item_id, delta.revision
                    ));
                }
                item.revision = delta.revision;
            }
            mai_protocol::ThreadNotification::InteractionChanged { interaction } => {
                if interaction.scope.thread_id != thread_id {
                    return invalid_thread_update("interaction crossed Thread ownership");
                }
            }
            mai_protocol::ThreadNotification::ThreadRuntimeUpdated { runtime } => {
                if runtime.thread_id != thread_id {
                    return invalid_thread_update("runtime snapshot crossed Thread ownership");
                }
            }
            mai_protocol::ThreadNotification::Lagged { .. } => {}
        }
        Ok(())
    }
}

fn invalid_thread_update<T>(message: impl Into<String>) -> Result<T> {
    Err(RuntimeError::InvalidInput(message.into()))
}

pub(crate) fn ensure_readable_product_thread(summary: &AgentSummary) -> Result<()> {
    match summary.state.resource {
        AgentResourceState::Provisioning
        | AgentResourceState::Ready
        | AgentResourceState::Failed => Ok(()),
        AgentResourceState::Deleting | AgentResourceState::Deleted => {
            Err(RuntimeError::ThreadNotFound(summary.id.to_string()))
        }
    }
}

pub(crate) fn ensure_live_canonical_thread(snapshot: &pl_core::AgentSnapshot) -> Result<()> {
    if snapshot.lifecycle != pl_core::AgentLifecycleState::Active {
        return Err(RuntimeError::ThreadNotFound(
            snapshot.identity.id.to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn ensure_live_message_target(
    summary: &AgentSummary,
    snapshot: &pl_core::AgentSnapshot,
) -> Result<()> {
    if summary.state.resource != AgentResourceState::Ready {
        return Err(RuntimeError::ThreadNotFound(summary.id.to_string()));
    }
    ensure_live_canonical_thread(snapshot)
}

fn parse_thread_agent_id(thread_id: &str) -> Result<AgentId> {
    AgentId::parse_str(thread_id).map_err(|error| {
        RuntimeError::InvalidInput(format!("invalid product Thread id `{thread_id}`: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use mai_protocol::{
        AgentState, ThreadItem, ThreadItemContent, ThreadItemDelta, ThreadItemDeltaField,
        ThreadItemStatus, ThreadNotification, ThreadNotificationEnvelope, TokenUsage,
    };
    use pl_core::{
        AgentActivityState, AgentId as CanonicalAgentId, AgentIdentity, AgentLifecycleState,
        AgentRoleId, AgentSnapshot,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn resource_recovery_keeps_thread_readable_but_blocks_messages() {
        let id = Uuid::new_v4();
        let mut summary = summary(id);
        let mut snapshot = snapshot(id);

        assert!(ensure_readable_product_thread(&summary).is_ok());
        assert!(ensure_live_message_target(&summary, &snapshot).is_err());
        summary.state.resource = AgentResourceState::Ready;
        assert!(ensure_readable_product_thread(&summary).is_ok());
        assert!(ensure_live_message_target(&summary, &snapshot).is_ok());
        summary.state.resource = AgentResourceState::Failed;
        assert!(ensure_readable_product_thread(&summary).is_ok());
        assert!(ensure_live_message_target(&summary, &snapshot).is_err());
        summary.state.resource = AgentResourceState::Deleting;
        assert!(ensure_readable_product_thread(&summary).is_err());
        summary.state.resource = AgentResourceState::Deleted;
        assert!(ensure_readable_product_thread(&summary).is_err());
        summary.state.resource = AgentResourceState::Ready;
        snapshot.lifecycle = AgentLifecycleState::Closing;
        assert!(ensure_live_message_target(&summary, &snapshot).is_err());
    }

    #[test]
    fn subscription_validator_rejects_cross_thread_and_revision_gaps() {
        let mut validator = ThreadUpdateValidator::default();
        validator
            .validate(
                "thread-a",
                &ThreadSubscriptionUpdate::Snapshot {
                    snapshot: Box::new(ThreadSnapshot::empty("thread-a")),
                },
            )
            .expect("authoritative snapshot");

        assert!(
            validator
                .validate(
                    "thread-a",
                    &notification(
                        "thread-b",
                        1,
                        ThreadNotification::ItemStarted {
                            item: Box::new(item(1)),
                        },
                    ),
                )
                .is_err()
        );
        assert!(
            validator
                .validate(
                    "thread-a",
                    &notification(
                        "thread-a",
                        2,
                        ThreadNotification::ItemStarted {
                            item: Box::new(item(1)),
                        },
                    ),
                )
                .is_err()
        );
        validator
            .validate(
                "thread-a",
                &notification(
                    "thread-a",
                    1,
                    ThreadNotification::ItemStarted {
                        item: Box::new(item(1)),
                    },
                ),
            )
            .expect("first Item revision");
        assert!(
            validator
                .validate(
                    "thread-a",
                    &notification(
                        "thread-a",
                        2,
                        ThreadNotification::ItemDelta {
                            delta: ThreadItemDelta {
                                item_id: "item-a".to_string(),
                                revision: 3,
                                field: ThreadItemDeltaField::Text,
                                delta: "gap".to_string(),
                                chunk_index: None,
                            },
                        },
                    ),
                )
                .is_err()
        );
    }

    fn notification(
        thread_id: &str,
        revision: u64,
        notification: ThreadNotification,
    ) -> ThreadSubscriptionUpdate {
        ThreadSubscriptionUpdate::Notification {
            notification: Box::new(ThreadNotificationEnvelope {
                thread_id: thread_id.to_string(),
                revision,
                emitted_at: i64::try_from(revision).expect("revision timestamp"),
                notification,
            }),
        }
    }

    fn item(revision: u64) -> ThreadItem {
        ThreadItem {
            id: "item-a".to_string(),
            thread_id: "thread-a".to_string(),
            turn_id: "turn-a".to_string(),
            ordinal: 0,
            revision,
            status: ThreadItemStatus::Streaming,
            created_at: 1,
            updated_at: 1,
            completed_at: None,
            error: None,
            content: ThreadItemContent::AgentMessage {
                channel: mai_protocol::AgentMessageChannel::Commentary,
                text: String::new(),
            },
            usage: None,
        }
    }

    fn summary(id: AgentId) -> AgentSummary {
        let now = Utc::now();
        AgentSummary {
            id,
            parent_id: None,
            task_id: None,
            project_id: None,
            role: None,
            name: "thread".to_string(),
            state: AgentState::default(),
            container_id: None,
            docker_image: String::new(),
            provider_id: "test".to_string(),
            provider_name: "test".to_string(),
            model: "test".to_string(),
            reasoning_effort: None,
            created_at: now,
            updated_at: now,
            token_usage: TokenUsage::default(),
        }
    }

    fn snapshot(id: AgentId) -> AgentSnapshot {
        AgentSnapshot {
            identity: AgentIdentity {
                id: CanonicalAgentId::new(id.to_string()).expect("canonical id"),
                parent_id: None,
                role: AgentRoleId::new("executor").expect("role"),
                depth: 0,
            },
            lifecycle: AgentLifecycleState::Active,
            activity: AgentActivityState::Idle,
            active_turn_id: None,
            pending_inputs: 0,
            progress: None,
            last_turn: None,
            revision: 0,
            event_sequence: 0,
            updated_at: 0,
        }
    }
}
