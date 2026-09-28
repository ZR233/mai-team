//! 协作子代理终态到父 Thread inbox 的产品路由。
//!
//! 子代理的已提交 Turn 是待投递报告的权威来源；父 Thread 的已提交 inbox 是收据来源。
//! 两者都通过 PL typed 历史接口读取。驻留去重窗口会裁剪旧身份，因此重启补投必须查持久
//! inbox，不能只凭一次 `send_message_and_resume` 的返回值判断是否尚未受理。

use std::sync::{Arc, Weak};
use std::time::Duration;

use mai_protocol::{AgentId, ThreadTurnHistory};
use pl_core::context::{ContextContent, OpaquePayload};
use pl_core::persistence::ThreadEffectQuery;
use pl_core::thread::{ThreadLifecycle, inbox::ThreadMessage};
use pl_protocol::ThreadTextChannel;

use crate::thread_projection::project_turn_history_from_effects;
use crate::{AgentRuntime, Result, RuntimeError};

const EFFECT_PAGE_LIMIT: usize = 256;
const REPORT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// 启动时补冷历史，以后按已提交 effect 水位观察新终态；循环不持有 Runtime 的强引用。
pub(crate) async fn run_child_report_loop(owner: Weak<AgentRuntime>) {
    let mut interval = tokio::time::interval(REPORT_POLL_INTERVAL);
    loop {
        interval.tick().await;
        let Some(runtime) = owner.upgrade() else {
            break;
        };
        for child in runtime.list_agents().await {
            // 自动 Review reviewer 有独立 Job/Run 归档和提交语义，不给 maintainer 注入第二份结果。
            if child.parent_id.is_none() || child.review_run_id.is_some() {
                continue;
            }
            if let Err(error) = runtime.reconcile_child_reports(child.id).await {
                tracing::warn!(child_id = %child.id, %error, "child report reconciliation failed");
            }
        }
    }
}

impl AgentRuntime {
    /// 只在 child effect 水位前进时读取完整终态历史；投递失败不推进扫描水位，下轮继续补投。
    pub(crate) async fn reconcile_child_reports(&self, child_id: AgentId) -> Result<()> {
        let child = self.agent(child_id).await?;
        let Some(parent_id) = child.summary.read().await.parent_id else {
            return Ok(());
        };
        let latest = self
            .session_history
            .effects(
                child_id,
                ThreadEffectQuery {
                    before_sequence: None,
                    limit: 1,
                },
            )
            .await?;
        let Some(newest_sequence) = latest.effects.first().map(|effect| effect.sequence) else {
            return Ok(());
        };
        // 持有产品路由锁直到收据 durable，避免关闭路径与后台扫描同时补投同一终态。
        let mut progress = self.child_report_progress.lock().await;
        let seen_sequence = progress.get(&child_id).copied().unwrap_or_default();
        if seen_sequence >= newest_sequence {
            return Ok(());
        }

        let mut effects = latest.effects;
        let mut before = latest.next_before_sequence;
        while let Some(sequence) = before {
            let page = self
                .session_history
                .effects(
                    child_id,
                    ThreadEffectQuery {
                        before_sequence: Some(sequence),
                        limit: EFFECT_PAGE_LIMIT,
                    },
                )
                .await?;
            effects.extend(page.effects);
            before = page.next_before_sequence;
        }
        let mut turns = project_turn_history_from_effects(&child_id.to_string(), &effects)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        turns.sort_by_key(|history| history.turn.revision);
        for history in turns
            .into_iter()
            .filter(|history| history.turn.revision > seen_sequence)
        {
            self.deliver_child_report(child_id, parent_id, &history)
                .await?;
        }
        progress.insert(child_id, newest_sequence);
        Ok(())
    }

    /// 父 inbox 接收或确认一条固定身份的终态报告；迟到报告只唤醒，不打断在途 Turn。
    async fn deliver_child_report(
        &self,
        child_id: AgentId,
        parent_id: AgentId,
        history: &ThreadTurnHistory,
    ) -> Result<()> {
        let message = child_report_message(child_id, history)?;
        let parent = self.ensure_thread(parent_id).await?;
        if parent.handle.snapshot().lifecycle != ThreadLifecycle::Open {
            return Err(RuntimeError::InvalidInput(format!(
                "parent Thread `{parent_id}` is closed before child report delivery"
            )));
        }
        self.await_agent_durable(parent_id, parent.handle.snapshot().commit_sequence)
            .await?;
        if let Some((sequence, _)) = self
            .accepted_message_identity(parent_id, &message.id, &message.source_id)
            .await?
        {
            parent
                .handle
                .wake_accepted_message(&message.id, sequence, parent.input_driver)
                .await?;
        } else {
            parent
                .handle
                .send_message_and_resume(message, parent.input_driver)
                .await?;
        }
        self.await_agent_durable(parent_id, parent.handle.snapshot().commit_sequence)
            .await?;
        Ok(())
    }

    /// 通过 PL typed effect 历史确认固定消息 ID 的原受理序号。
    pub(crate) async fn accepted_message_identity(
        &self,
        parent_id: AgentId,
        message_id: &str,
        source_id: &str,
    ) -> Result<Option<(u64, String)>> {
        let mut before = None;
        loop {
            let page = self
                .session_history
                .effects(
                    parent_id,
                    ThreadEffectQuery {
                        before_sequence: before,
                        limit: EFFECT_PAGE_LIMIT,
                    },
                )
                .await?;
            for effect in &page.effects {
                for record in effect.inbox.iter() {
                    if record.message.id == message_id {
                        if record.message.source_id != source_id {
                            return Err(RuntimeError::InvalidInput(format!(
                                "parent Thread `{parent_id}` accepted report identity \
                                 `{message_id}` from a different source"
                            )));
                        }
                        return Ok(Some((record.sequence, record.message.digest())));
                    }
                }
            }
            match page.next_before_sequence {
                Some(sequence) => before = Some(sequence),
                None => return Ok(None),
            }
        }
    }
}

/// 报告身份只由子 Thread 和终态 effect 水位组成，重启后重新投影仍指向同一消息。
fn child_report_message(child_id: AgentId, history: &ThreadTurnHistory) -> Result<ThreadMessage> {
    let final_text = history
        .items
        .iter()
        .filter_map(|item| item.text())
        .filter(|text| text.channel() == ThreadTextChannel::Final)
        .map(|text| text.text())
        .collect::<Vec<_>>()
        .join("\n\n");
    let message = if final_text.is_empty() {
        let commentary = history
            .items
            .iter()
            .filter_map(|item| item.text())
            .filter(|text| text.channel() == ThreadTextChannel::Commentary)
            .map(|text| text.text())
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("子代理本轮结束，未提交最终总结。已有输出：\n{commentary}")
    } else {
        final_text
    };
    let report = serde_json::json!({
        "childId": child_id,
        "turnId": history.turn.id,
        "commitSequence": history.turn.revision,
        "state": history.turn.state,
        "message": message,
    });
    let body = serde_json::to_string(&report)
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    Ok(ThreadMessage {
        id: format!(
            "mai.child.{}:{child_id}:{}",
            child_id.to_string().len(),
            history.turn.revision
        ),
        source_id: format!("agent:{child_id}"),
        payload: OpaquePayload::new("mai.child.turn-report", 1, body.clone())
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?,
        context: vec![ContextContent::Text {
            text: Arc::from(body),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mai_protocol::{ThreadContextDisposition, Turn};
    use pl_protocol::{
        CompletedTurnState, ThreadContentLifecycle, ThreadItem, ThreadItemState, ThreadTextItem,
        TurnCompletion, TurnState,
    };
    use pretty_assertions::assert_eq;

    #[test]
    fn report_identity_is_stable_for_terminal_effect() {
        let child = AgentId::new_v4();
        let history = ThreadTurnHistory {
            turn: Turn {
                id: "turn-1".to_string(),
                thread_id: child.to_string(),
                input_id: None,
                revision: 42,
                state: TurnState::Completed(CompletedTurnState::new(
                    Some(1),
                    2,
                    TurnCompletion::Normal,
                )),
                updated_at: 2,
            },
            items: vec![ThreadItem::new(
                "item-1".to_string(),
                child.to_string(),
                "turn-1".to_string(),
                1,
                1,
                2,
                2,
                ThreadItemState::Text(ThreadTextItem::new(
                    ThreadTextChannel::Final,
                    "完成".to_string(),
                    Vec::new(),
                    ThreadContentLifecycle::completed(2),
                )),
            )],
            context_disposition: ThreadContextDisposition::Active,
        };

        let report = child_report_message(child, &history).expect("report");
        assert_eq!(report.id, format!("mai.child.36:{child}:42"));
        assert_eq!(report.source_id, format!("agent:{child}"));
        assert!(report.payload.content().contains("完成"));
    }
}
