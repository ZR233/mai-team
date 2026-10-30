//! 产品取消身份映射：已执行的 Turn 与尚未执行的受理输入有不同的 PL 生命周期。

use crate::Result;
use pl_core::thread::{ThreadHandle, TurnState};

pub(crate) async fn cancel(handle: &ThreadHandle, target: &str) -> Result<()> {
    let state = handle.snapshot();
    let active = state.turns.iter().rev().find(|turn| {
        turn.state == TurnState::Running
            && (turn.turn_id == target || turn.input_id.as_deref() == Some(target))
    });
    let input = match active {
        Some(active) => {
            match handle.interrupt_turn(Some(active.turn_id.clone())).await {
                Ok(_) | Err(pl_core::thread::ThreadError::InvalidIdentity) => {}
                Err(error) => return Err(error.into()),
            }
            active.input_id.as_deref().unwrap_or(target)
        }
        None => target,
    };
    // 中断尚未消费的输入不能让它在下一次 continuation 中再次被选中；
    // 从未执行的排队输入则直接走 PL 的 discard，不取消无关的活动 Turn。
    if handle.snapshot().inputs.iter().any(|record| {
        record.input.id == input && record.state == pl_core::thread::input::InputState::Pending
    }) {
        handle.discard_input(input.to_owned()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pl_core::{
        context::OpaquePayload,
        thread::input::{InputState, ThreadInput},
    };
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn cancellation_of_an_accepted_input_without_a_turn_discards_that_input() {
        let owner = ThreadHandle::resume_without_model("cancel-input".into(), None).unwrap();
        owner
            .submit_input(ThreadInput {
                id: "queued".into(),
                payload: OpaquePayload::text("queued"),
                context: vec![],
            })
            .await
            .unwrap();
        owner
            .submit_input(ThreadInput {
                id: "keep".into(),
                payload: OpaquePayload::text("keep"),
                context: vec![],
            })
            .await
            .unwrap();
        cancel(&owner, "queued").await.unwrap();
        let snapshot = owner.snapshot();
        assert_eq!(
            (
                snapshot
                    .inputs
                    .iter()
                    .map(|input| (input.input.id.as_str(), input.state.clone()))
                    .collect::<Vec<_>>(),
                snapshot
                    .terminal_inputs
                    .iter()
                    .map(|input| (input.id.as_str(), input.state.clone()))
                    .collect::<Vec<_>>()
            ),
            (
                vec![("keep", InputState::Pending)],
                vec![("queued", InputState::Discarded)]
            )
        );
    }
}
