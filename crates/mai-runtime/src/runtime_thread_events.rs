//! 产品 Thread 快照、订阅帧与 Turn 历史的运行时门面。
//!
//! 事实来源只有 PL core 的 typed API：
//! - 权威状态来自 [`pl_core::thread::ThreadHandle::snapshot`] 与
//!   [`pl_core::thread::ThreadSubscription`]；
//! - 已提交历史来自 [`crate::session_history::SessionHistory`] 的 Thread effect 查询。
//!
//! 本模块不读取任何会话 SQLite 表或存储键，也不再从旧的 `pl_core::AgentSnapshot` 或
//! `mai-store` 的 Turn 列表反推 Thread 状态。投影全部经由 [`crate::thread_projection`]；
//! 无法由 typed 事实确定的内容显式报错，绝不填占位值。
//!
//! # Turn history
//!
//! API 层的 Turn history 由 [`crate::thread_projection::project_turn_history_from_effects`] 归并
//! canonical effect 得到：本模块按页读取 effect，直到覆盖足够的完整 Turn，再从归并结果取本页
//! Turn，并把页尾最旧 Turn 的终态提交序号作为下一轮 cursor。
//! [`crate::runtime_agent_api`] 的 `project_thread_snapshot` 与 `last_terminal_turn` 在本模块
//! 复用，不复制 Mode/工作区投影与终态 Turn 读取。

use std::{collections::HashMap, fmt, sync::Arc};

use mai_protocol::{
    AgentId, AgentResourceState, AgentSummary, ThreadContextDisposition, ThreadNotification,
    ThreadNotificationEnvelope, ThreadSnapshot, ThreadSubscriptionUpdate, ThreadTurnHistory,
    ThreadTurnPage, Turn,
};
use pl_core::persistence::ThreadEffectQuery;
use pl_core::thread::{
    ThreadEffectBatch, ThreadLifecycle, ThreadSnapshot as CoreThreadSnapshot,
    ThreadSubscription as CoreThreadSubscription,
};
use tokio_util::sync::CancellationToken;

use crate::runtime_agent_api::project_thread_snapshot;
use crate::state::AgentRecord;
use crate::thread_projection::{
    project_active_turn_items_from_effects, project_turn_history_from_effects,
    turn_terminal_sequence,
};
use crate::{AgentRuntime, Result, RuntimeError};

/// Turn history 单页上限，与 pl-core Thread effect 查询的页大小上限一致。
const EFFECT_PAGE_LIMIT: usize = 256;

/// 产品权限校验后直接持有的 canonical Thread 订阅。
///
/// 首帧是 `project_thread_snapshot` 生成的权威 [`ThreadSnapshot`]；其后每一帧都是把新的 canonical
/// 快照与上一帧比较后得到的 typed [`ThreadNotification`]。核心订阅是 coalescing 的 watch，
/// 因此重复或回退的 commit_sequence 会被丢弃，不会伪造增量。
pub struct MaiThreadEventSubscription {
    inner: CoreThreadSubscription,
    runtime: Arc<AgentRuntime>,
    agent_id: AgentId,
    agent: Arc<AgentRecord>,
    thread_id: String,
    lifecycle: CancellationToken,
    validator: ThreadUpdateValidator,
    state: SubscriptionState,
}

impl fmt::Debug for MaiThreadEventSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaiThreadEventSubscription")
            .field("thread_id", &self.thread_id)
            .field("pending_updates", &self.state.pending.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct SubscriptionState {
    previous: Option<ThreadSnapshot>,
    pending: std::collections::VecDeque<ThreadSubscriptionUpdate>,
    /// 已投递通知的水位；首帧后等于权威快照的 revision。
    revision: u64,
    /// 广播生命周期标识；一次订阅内固定。
    epoch: u64,
}

impl MaiThreadEventSubscription {
    /// 接收同一 Thread 的 authoritative snapshot 或 typed notification。
    pub async fn recv(&mut self) -> Option<ThreadSubscriptionUpdate> {
        loop {
            if let Some(update) = self.state.pending.pop_front() {
                return Some(update);
            }
            let state = tokio::select! {
                _ = self.lifecycle.cancelled() => return None,
                state = self.inner.next() => state?,
            };
            let snapshot = match self.project(&state).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(thread_id = %self.thread_id, %error, "closing Thread subscription on invalid projection");
                    return None;
                }
            };
            let thread_id = self.thread_id.clone();
            if self.state.previous.is_none() {
                if let Err(error) = self.validator.validate_snapshot(&thread_id, &snapshot) {
                    tracing::warn!(thread_id = %thread_id, %error, "closing invalid Thread subscription");
                    return None;
                }
                self.state.revision = snapshot.revision;
                self.state.previous = Some(snapshot.clone());
                return Some(ThreadSubscriptionUpdate::Snapshot {
                    snapshot: Box::new(snapshot),
                });
            }

            let previous = self
                .state
                .previous
                .take()
                .expect("previous snapshot is checked above");
            if snapshot.revision <= previous.revision {
                self.state.previous = Some(previous);
                continue;
            }
            let terminal_turn = match previous.active_turn.as_ref() {
                Some(before) if turn_identity_changed(&previous, &snapshot) => {
                    self.terminal_turn(&before.id).await
                }
                _ => None,
            };
            let notifications = project_notifications(&previous, &snapshot, terminal_turn.as_ref());
            self.state.previous = Some(snapshot);
            if let Err(error) = self.enqueue(&thread_id, notifications) {
                tracing::warn!(thread_id = %thread_id, %error, "closing invalid Thread subscription");
                return None;
            }
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    async fn project(&self, state: &CoreThreadSnapshot) -> Result<ThreadSnapshot> {
        let summary = self.agent.summary.read().await.clone();
        project_thread_snapshot(&summary, state)
    }

    /// 用已提交的 typed 历史补齐刚结束 Turn 的终态投影。
    ///
    /// 权威快照只保留运行中的 Turn，已经提交的终态 Turn 只在 effect 里，因此不能从快照反推。
    /// 只有当窗口里最新终态 Turn 的身份正是刚结束的那个才采用它；否则跳过该帧，绝不伪造终态。
    async fn terminal_turn(&self, turn_id: &str) -> Option<Turn> {
        match self.runtime.last_terminal_turn(self.agent_id).await {
            Ok(Some(turn)) if turn.id == turn_id => Some(turn),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(thread_id = %self.thread_id, %error, "cannot read terminal Turn from typed history");
                None
            }
        }
    }

    fn enqueue(&mut self, thread_id: &str, notifications: Vec<ThreadNotification>) -> Result<()> {
        for notification in notifications {
            let revision = self.state.revision.saturating_add(1);
            let envelope = ThreadNotificationEnvelope::new(
                thread_id.to_owned(),
                self.state.epoch,
                self.state.revision,
                revision,
                mai_protocol::now().timestamp(),
                notification,
            );
            self.validator.validate_notification(thread_id, &envelope)?;
            self.state.revision = revision;
            self.state
                .pending
                .push_back(ThreadSubscriptionUpdate::Notification {
                    notification: Box::new(envelope),
                });
        }
        Ok(())
    }
}

impl AgentRuntime {
    /// 校验产品 Thread 所有权后建立隔离的 canonical subscription。
    pub async fn subscribe_thread(
        self: &Arc<Self>,
        thread_id: String,
    ) -> Result<MaiThreadEventSubscription> {
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
        let resident = self.ensure_thread(product_agent_id).await?;
        ensure_live_canonical_thread(&thread_id, &resident.handle.snapshot())?;
        let inner = resident.handle.subscribe();
        Ok(MaiThreadEventSubscription {
            inner,
            runtime: Arc::clone(self),
            agent_id: product_agent_id,
            agent,
            thread_id,
            lifecycle,
            validator: ThreadUpdateValidator::default(),
            state: SubscriptionState::default(),
        })
    }

    /// 读取一个产品 Thread 的 authoritative snapshot。
    pub async fn thread_snapshot(self: &Arc<Self>, thread_id: String) -> Result<ThreadSnapshot> {
        let product_agent_id = parse_thread_agent_id(&thread_id)?;
        let agent = self.agent(product_agent_id).await?;
        let summary = agent.summary.read().await.clone();
        ensure_readable_product_thread(&summary)?;
        let resident = self.ensure_thread(product_agent_id).await?;
        let state = resident.handle.snapshot();
        ensure_live_canonical_thread(&thread_id, &state)?;
        project_thread_snapshot(&summary, &state)
    }

    /// 从 pl-core 的权威当前 Turn 与已提交 typed effect 组成运行中 Review 聊天。
    /// 不为旧会话启动容器；调用方只在 reviewer 当前驻留时使用此视图。
    pub(crate) async fn active_review_turn_history(
        self: &Arc<Self>,
        agent_id: AgentId,
        input_id: &str,
    ) -> Result<Option<ThreadTurnHistory>> {
        if self.resident_thread(agent_id).is_none() {
            return Ok(None);
        }
        let thread_id = agent_id.to_string();
        let snapshot = self.thread_snapshot(thread_id.clone()).await?;
        let Some(turn) = snapshot
            .active_turn
            .filter(|turn| turn.input_id.as_deref() == Some(input_id))
        else {
            return Ok(None);
        };
        self.await_agent_durable(agent_id, snapshot.revision)
            .await?;
        let mut effects = Vec::new();
        let mut before_sequence = None;
        loop {
            let page = self
                .session_history
                .effects(
                    agent_id,
                    ThreadEffectQuery {
                        before_sequence,
                        limit: EFFECT_PAGE_LIMIT,
                    },
                )
                .await?;
            let started = page.effects.iter().any(|effect| {
                effect.turn.as_ref().is_some_and(|record| {
                    record.turn_id == turn.id && record.state == pl_core::thread::TurnState::Running
                })
            });
            effects.extend(page.effects);
            if started {
                break;
            }
            let Some(next) = page.next_before_sequence else {
                return Err(RuntimeError::InvalidInput(format!(
                    "active Review Turn {} has no durable start effect",
                    turn.id
                )));
            };
            before_sequence = Some(next);
        }
        Ok(Some(ThreadTurnHistory {
            items: project_active_turn_items_from_effects(&thread_id, &effects, &turn.id),
            turn,
            context_disposition: ThreadContextDisposition::Active,
        }))
    }

    /// 用 canonical Thread effect 分页读取一个 Thread 的 Turn history。
    ///
    /// 一轮 Turn 的 typed 事实分散在多条 effect 里，因此本方法按页读取 canonical effect，直到归并出
    /// `limit` 个完整 Turn（同时含起始与终态 effect），再把最旧一个 Turn 的终态提交序号作为
    /// `next_cursor`；该游标是本页的排他上界，必须原样传回。宿主只使用 pl-core 的 typed 解码与
    /// 所有权校验，不查询会话 SQLite 的表或键，也不从驻留快照猜旧历史。
    pub async fn thread_turns(
        &self,
        thread_id: String,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ThreadTurnPage> {
        let product_agent_id = parse_thread_agent_id(&thread_id)?;
        let agent = self.agent(product_agent_id).await?;
        let summary = agent.summary.read().await.clone();
        ensure_readable_product_thread(&summary)?;
        // 历史查询只依赖 pl-core 的 durable effects；冷 Thread 无需为读历史而启动容器和 MCP。
        if let Some(resident) = self.resident_thread(product_agent_id) {
            self.await_agent_durable(product_agent_id, resident.handle.snapshot().commit_sequence)
                .await?;
        }
        let before_sequence = cursor.map(parse_turn_cursor).transpose()?;
        let limit = limit.clamp(1, EFFECT_PAGE_LIMIT);
        let mut effects: Vec<ThreadEffectBatch> = Vec::new();
        let mut before = before_sequence;
        let exhausted;
        let mut turns = Vec::new();
        loop {
            let page = self
                .session_history
                .effects(
                    product_agent_id,
                    ThreadEffectQuery {
                        before_sequence: before,
                        limit: EFFECT_PAGE_LIMIT,
                    },
                )
                .await?;
            if page.effects.is_empty() {
                exhausted = true;
                break;
            }
            let next_before_sequence = page.next_before_sequence;
            effects.extend(page.effects);
            turns = project_turn_history_from_effects(&thread_id, &effects)
                .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
            if turns.len() >= limit {
                exhausted = next_before_sequence.is_none();
                break;
            }
            match next_before_sequence {
                Some(sequence) => before = Some(sequence),
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        let has_more_loaded = turns.len() > limit;
        turns.truncate(limit);
        let next_cursor = if exhausted && !has_more_loaded {
            None
        } else {
            turns
                .last()
                .and_then(|turn| turn_terminal_sequence(&effects, &turn.turn.id))
                .map(|sequence| sequence.to_string())
        };
        Ok(ThreadTurnPage { turns, next_cursor })
    }
}

/// 产品通知封套的所有权与序号校验。
///
/// 校验与产品客户端一致：Thread 身份必须匹配、epoch 必须稳定、水位必须严格逐帧 +1。任何不一致
/// 都表示订阅会拼出一个有洞的视图，因此直接失败而不是继续降级。
#[derive(Debug, Default)]
struct ThreadUpdateValidator {
    epoch: Option<u64>,
    revision: Option<u64>,
}

impl ThreadUpdateValidator {
    /// 校验权威首帧，并用它的 revision 播下通知水位。
    fn validate_snapshot(&mut self, thread_id: &str, snapshot: &ThreadSnapshot) -> Result<()> {
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
        self.revision = Some(snapshot.revision);
        Ok(())
    }

    /// 校验一条出站通知封套的 Thread 身份、epoch 与连续水位。
    fn validate_notification(
        &mut self,
        thread_id: &str,
        envelope: &ThreadNotificationEnvelope,
    ) -> Result<()> {
        if envelope.thread_id != thread_id {
            return invalid_thread_update(format!(
                "notification belongs to {}, expected {thread_id}",
                envelope.thread_id
            ));
        }
        if matches!(envelope.notification, ThreadNotification::Lagged { .. }) {
            // lagged 帧携带新的 epoch 与水位，客户端据此重同步而不是继续拼接。
            self.epoch = Some(envelope.epoch);
            self.revision = Some(envelope.revision);
            return Ok(());
        }
        match self.epoch {
            None => self.epoch = Some(envelope.epoch),
            Some(epoch) if epoch == envelope.epoch => {}
            Some(epoch) => {
                return invalid_thread_update(format!(
                    "epoch changed from {epoch} to {}",
                    envelope.epoch
                ));
            }
        }
        let expected = self
            .revision
            .map(|revision| revision.saturating_add(1))
            .ok_or_else(|| {
                RuntimeError::InvalidInput("notification arrived before snapshot".to_string())
            })?;
        if envelope.revision != expected {
            return invalid_thread_update(format!(
                "Thread revision gap: expected {expected}, got {}",
                envelope.revision
            ));
        }
        self.validate_ownership(thread_id, &envelope.notification)?;
        self.revision = Some(envelope.revision);
        Ok(())
    }

    fn validate_ownership(&self, thread_id: &str, notification: &ThreadNotification) -> Result<()> {
        match notification {
            ThreadNotification::TurnStarted { turn }
            | ThreadNotification::TurnUpdated { turn }
            | ThreadNotification::TurnCompleted { turn } => {
                if turn.thread_id != thread_id {
                    return invalid_thread_update(format!(
                        "Turn {} crossed Thread ownership",
                        turn.id
                    ));
                }
            }
            ThreadNotification::InteractionChanged { interaction } => {
                if interaction.scope.thread_id != thread_id {
                    return invalid_thread_update("interaction crossed Thread ownership");
                }
            }
            ThreadNotification::ThreadRuntimeUpdated { runtime } => {
                if runtime.thread_id != thread_id {
                    return invalid_thread_update("runtime snapshot crossed Thread ownership");
                }
            }
            ThreadNotification::ActivityChanged { .. }
            | ThreadNotification::StorageChanged { .. }
            | ThreadNotification::Lagged { .. } => {}
        }
        Ok(())
    }
}

/// 由相邻两帧权威快照推导 typed 通知。
///
/// 只投影 snapshot 本身能确定的事实：Turn 生命周期、interaction、runtime、storage 与 activity。
/// 已经提交的终态 Turn 不在快照里，由调用方从 effect window 补齐后传入 `terminal_turn`；缺失时
/// 不生成 `TurnCompleted`，绝不把一个运行中的 Turn 冒充成终态。
fn project_notifications(
    previous: &ThreadSnapshot,
    next: &ThreadSnapshot,
    terminal_turn: Option<&Turn>,
) -> Vec<ThreadNotification> {
    let mut notifications = Vec::new();
    match (previous.active_turn.as_ref(), next.active_turn.as_ref()) {
        (None, Some(turn)) => {
            notifications.push(ThreadNotification::TurnStarted { turn: turn.clone() })
        }
        (Some(before), Some(after)) => {
            if before.id != after.id {
                if let Some(turn) = terminal_turn.filter(|turn| turn.id == before.id) {
                    notifications.push(ThreadNotification::TurnCompleted { turn: turn.clone() });
                }
                notifications.push(ThreadNotification::TurnStarted {
                    turn: after.clone(),
                });
            } else if before != after {
                notifications.push(ThreadNotification::TurnUpdated {
                    turn: after.clone(),
                });
            }
        }
        (Some(before), None) => {
            if let Some(turn) = terminal_turn.filter(|turn| turn.id == before.id) {
                notifications.push(ThreadNotification::TurnCompleted { turn: turn.clone() });
            }
        }
        (None, None) => {}
    }

    let known = previous
        .interactions
        .iter()
        .map(|interaction| (interaction.interaction_id.as_str(), interaction))
        .collect::<HashMap<_, _>>();
    for interaction in &next.interactions {
        if known
            .get(interaction.interaction_id.as_str())
            .is_none_or(|before| *before != interaction)
        {
            notifications.push(ThreadNotification::InteractionChanged {
                interaction: Box::new(interaction.clone()),
            });
        }
    }

    if previous.runtime != next.runtime
        && let Some(runtime) = next.runtime.clone()
    {
        notifications.push(ThreadNotification::ThreadRuntimeUpdated {
            runtime: Box::new(runtime),
        });
    }
    if previous.storage != next.storage {
        notifications.push(ThreadNotification::StorageChanged {
            storage: next.storage.clone().map(Box::new),
        });
    }
    if previous.activity != next.activity {
        notifications.push(ThreadNotification::ActivityChanged {
            activity: next.activity.clone().map(Box::new),
        });
    }
    notifications
}

/// 新快照里 active Turn 的身份是否与上一帧不同。
fn turn_identity_changed(previous: &ThreadSnapshot, next: &ThreadSnapshot) -> bool {
    previous.active_turn.as_ref().map(|turn| turn.id.as_str())
        != next.active_turn.as_ref().map(|turn| turn.id.as_str())
}

fn invalid_thread_update<T>(message: impl Into<String>) -> Result<T> {
    Err(RuntimeError::InvalidInput(message.into()))
}

fn parse_turn_cursor(cursor: &str) -> Result<u64> {
    cursor.parse::<u64>().map_err(|error| {
        RuntimeError::InvalidInput(format!("invalid Thread turn cursor `{cursor}`: {error}"))
    })
}

pub(crate) fn ensure_readable_product_thread(summary: &AgentSummary) -> Result<()> {
    match summary.resource.state {
        AgentResourceState::Provisioning
        | AgentResourceState::Ready
        | AgentResourceState::Failed => Ok(()),
        AgentResourceState::Deleting | AgentResourceState::Deleted => {
            Err(RuntimeError::ThreadNotFound(summary.id.to_string()))
        }
    }
}

/// canonical Thread 是否仍处于可观察的 open 生命周期。
pub(crate) fn ensure_live_canonical_thread(
    thread_id: &str,
    snapshot: &CoreThreadSnapshot,
) -> Result<()> {
    match snapshot.lifecycle {
        ThreadLifecycle::Open => Ok(()),
        ThreadLifecycle::Closing | ThreadLifecycle::Closed => {
            Err(RuntimeError::ThreadNotFound(thread_id.to_owned()))
        }
    }
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
        AgentResourceSnapshot, AgentSummary, RuntimeUsageSnapshot, ThreadNotification,
        ThreadNotificationEnvelope, Turn,
    };
    use pl_core::thread::ThreadSnapshot as CoreThreadSnapshot;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn thread_readability_and_liveness_follow_resource_and_lifecycle() {
        let id = Uuid::new_v4();
        let thread_id = id.to_string();
        let mut summary = summary(id);
        let mut snapshot = CoreThreadSnapshot::default();

        assert!(ensure_readable_product_thread(&summary).is_ok());
        assert!(ensure_live_canonical_thread(&thread_id, &snapshot).is_ok());

        summary.resource.state = AgentResourceState::Ready;
        assert!(ensure_readable_product_thread(&summary).is_ok());
        summary.resource.state = AgentResourceState::Failed;
        assert!(ensure_readable_product_thread(&summary).is_ok());
        summary.resource.state = AgentResourceState::Deleting;
        assert!(ensure_readable_product_thread(&summary).is_err());
        summary.resource.state = AgentResourceState::Deleted;
        assert!(ensure_readable_product_thread(&summary).is_err());

        snapshot.lifecycle = ThreadLifecycle::Closing;
        assert!(ensure_live_canonical_thread(&thread_id, &snapshot).is_err());
        snapshot.lifecycle = ThreadLifecycle::Closed;
        assert!(ensure_live_canonical_thread(&thread_id, &snapshot).is_err());
    }

    #[test]
    fn subscription_validator_enforces_ownership_and_sequence() {
        let mut validator = ThreadUpdateValidator::default();
        validator
            .validate_snapshot("thread-a", &ThreadSnapshot::empty("thread-a"))
            .expect("authoritative snapshot");
        assert!(
            validator
                .validate_snapshot("thread-a", &ThreadSnapshot::empty("thread-a"))
                .is_err()
        );

        let first = notification(
            "thread-a",
            1,
            ThreadNotification::TurnStarted {
                turn: turn("thread-a"),
            },
        );
        validator
            .validate_notification("thread-a", &first)
            .expect("first notification continues the snapshot");

        let crossed_thread = notification(
            "thread-b",
            2,
            ThreadNotification::TurnStarted {
                turn: turn("thread-b"),
            },
        );
        assert!(
            validator
                .validate_notification("thread-a", &crossed_thread)
                .is_err()
        );
        let crossed_turn = notification(
            "thread-a",
            2,
            ThreadNotification::TurnStarted {
                turn: turn("thread-b"),
            },
        );
        assert!(
            validator
                .validate_notification("thread-a", &crossed_turn)
                .is_err()
        );
        let gap = notification(
            "thread-a",
            3,
            ThreadNotification::TurnUpdated {
                turn: turn("thread-a"),
            },
        );
        assert!(validator.validate_notification("thread-a", &gap).is_err());

        let next = notification(
            "thread-a",
            2,
            ThreadNotification::TurnUpdated {
                turn: turn("thread-a"),
            },
        );
        validator
            .validate_notification("thread-a", &next)
            .expect("continuous notification");
    }

    #[test]
    fn notification_projection_reports_turn_and_terminal_facts() {
        let mut previous = ThreadSnapshot::empty("thread-a");
        let mut next = ThreadSnapshot::empty("thread-a");
        next.revision = 1;
        next.active_turn = Some(turn("thread-a"));
        assert_eq!(
            project_notifications(&previous, &next, None),
            vec![ThreadNotification::TurnStarted {
                turn: turn("thread-a"),
            }]
        );

        previous = next.clone();
        next.revision = 2;
        next.active_turn = None;
        assert_eq!(
            project_notifications(&previous, &next, Some(&turn("thread-a"))),
            vec![ThreadNotification::TurnCompleted {
                turn: turn("thread-a"),
            }]
        );
        // 没有 typed 终态事实时绝不伪造 TurnCompleted。
        assert!(project_notifications(&previous, &next, None).is_empty());
    }

    fn notification(
        thread_id: &str,
        revision: u64,
        notification: ThreadNotification,
    ) -> ThreadNotificationEnvelope {
        ThreadNotificationEnvelope::new(
            thread_id,
            0,
            revision.saturating_sub(1),
            revision,
            i64::try_from(revision).expect("revision timestamp"),
            notification,
        )
    }

    fn turn(thread_id: &str) -> Turn {
        Turn::queued("turn-a", thread_id, 0)
    }

    fn summary(id: AgentId) -> AgentSummary {
        let now = Utc::now();
        AgentSummary {
            id,
            parent_id: None,
            task_id: None,
            project_id: None,
            role: None,
            profile_id: None,
            workspace: None,
            review_run_id: None,
            name: "thread".to_string(),
            resource: AgentResourceSnapshot::default(),
            runtime: None,
            last_turn: None,
            container_id: None,
            docker_image: String::new(),
            provider_id: "test".to_string(),
            provider_name: "test".to_string(),
            model: "test".to_string(),
            reasoning_effort: None,
            created_at: now,
            updated_at: now,
            usage: RuntimeUsageSnapshot::default(),
        }
    }
}
