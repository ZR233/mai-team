//! 已提交 Thread effect 到产品 [`ThreadTurnHistory`] 的只读投影。
//!
//! 一轮 Turn 的 canonical 事实分散在多条 effect 里（输入受理、模型 step、工具 task/delivery、
//! Turn 终态各一条），并且驻留快照只保留当前状态。因此本投影不接受快照，而是把调用方通过
//! `SessionHistory` typed API 分页读到的 effect 集合按 `turn_id` 归并，重建每个终态 Turn 的完整
//! timeline。宿主只使用 pl-core 的 typed 解码与所有权校验，不读取会话 SQLite 表或键。
//!
//! 事实来源与边界：
//! - 终态 Turn 来自 effect 携带的非 `Running` [`TurnRecord`]，Turn 本体经
//!   [`super::project_turn`] 投影；该函数需要的 attempts/tasks 也从 effect 归并，因此已离开驻留
//!   快照的旧 Turn 仍能给出结构化失败与预算用量，而不是退化成描述文本。
//! - 用户消息来自 [`ContextChange`] 携带的 `User` context 记录全文（canonical input 的 model-visible
//!   投影）；只有某个 Turn 没有这样的记录时，才退回用 [`InputChange::Accepted`] 的 canonical input
//!   补齐（正文取 `input.context`），避免重复投影。
//! - 模型正文与工具调用来自 [`ContextChange`] 携带的 `Assistant` context 记录全文；没有提交
//!   context 的失败/取消 attempt 由 [`AttemptUpdate`] 补齐。
//! - 工具终态按 call identity 归并：delivery（typo 权威）优先于 task，再优先于 context 里的
//!   `ToolResult` 记录；同一条 Tool item 不会被重复投影。
//! - `context_disposition` 只在 core 的 Rewind（[`ContextReplacementReason::Rewind`]）确实把该
//!   Turn 的 context 记录移除时为 `RolledBack`。
//! - 时间单位沿用 core 的 typed 事实：`ThreadEffectBatch::committed_at` 是 Unix 秒，
//!   `TurnRecord::elapsed_ms` 是毫秒；本投影只透传 `committed_at`。
//! - 只有同时看到该 Turn 的 Running 起始 effect 与非 `Running` 终态 effect 时才输出它；缺少起始
//!   effect 表示调用方还没读到该 Turn 的全部 effect，此时宁可先不输出，也不给出不完整的 Turn。

use std::collections::{BTreeMap, BTreeSet};

use mai_protocol::{ThreadContextDisposition, ThreadTurnHistory, Turn};
use pl_core::context::{ContextContent, ContextRecord, ContextSnapshot, ContextSource};
use pl_core::thread::{
    AttemptOutcome, ContextReplacementReason, RequestAttempt, ThreadEffectBatch, TurnRecord,
    TurnState as CoreTurnState,
    input::{InputChange, InputDelivery, InputRecord, InputState},
    journal::{AttemptUpdate, ContextChange},
    task::TaskRecord,
};
use pl_protocol::{
    ThreadContentLifecycle, ThreadItem, ThreadItemState, ThreadRawItem, ThreadTextChannel,
    ThreadToolInvocation, ThreadToolItem, ThreadToolState, ThreadTurnItem,
};

use super::effect_items::{
    context_text, delivery_state, raw_payload, response_text_id, running_tool, succeeded,
    task_state, text_state, tool_item_id, turn_item_id,
};
use super::{ProjectionError, project_turn};

/// 未落地工具调用的状态优先级。
const RANK_CALL: u8 = 0;
/// context `ToolResult` 记录（仅作 delivery/task 缺失时的兜底）。
const RANK_CONTEXT_RESULT: u8 = 1;
/// task 生命周期。
const RANK_TASK: u8 = 2;
/// delivery 结果；这是 core 对工具终态的权威描述。
const RANK_DELIVERY: u8 = 3;
/// 文本类条目：同 identity 后写覆盖。
const RANK_TEXT: u8 = u8::MAX;

/// 把一组已读到的 Thread effect 归并成完整的 Turn history，最新 Turn 在前。
///
/// 调用方必须传入从最新序号开始、连续下降的一段 effect：只有同时包含某个 Turn 的起始
/// （`Running` [`TurnRecord`]）与终态 effect 时，该 Turn 才会被输出。缺少起始 effect 的 Turn 表示
/// 这段 effect 还没覆盖它的全部事实，跳过而不是给出不完整的结果。
///
/// # Errors
/// 任一终态 Turn 无法投影时报错，不返回被截断或占位的 history。
pub(crate) fn project_turn_history_from_effects(
    thread_id: &str,
    effects: &[ThreadEffectBatch],
) -> Result<Vec<ThreadTurnHistory>, ProjectionError> {
    let accumulators = collect_turns(thread_id, effects);
    let ordered = ordered_effects(effects);
    let rolled_back = rolled_back_turns(&ordered);
    // 只为同时见到起始与终态的 Turn 生成 history。
    let mut terminal: Vec<(u64, String, TurnAccumulator)> = Vec::new();
    for (turn_id, accumulator) in accumulators {
        if accumulator.terminal.is_none() || !accumulator.started {
            continue;
        }
        terminal.push((accumulator.terminal_sequence, turn_id, accumulator));
    }
    terminal.sort_by_key(|entry| std::cmp::Reverse(entry.0));

    let mut histories = Vec::with_capacity(terminal.len());
    for (sequence, turn_id, mut accumulator) in terminal {
        let record = accumulator
            .terminal
            .clone()
            .expect("terminal presence checked when collecting");
        let committed_at = accumulator.terminal_committed_at;
        let snapshot = accumulator.synthetic_snapshot();
        let turn = project_turn(
            thread_id,
            &snapshot,
            &record,
            committed_at,
            committed_at,
            sequence,
        )?;
        accumulator.at = committed_at;
        accumulator.push_turn_item(&turn);
        accumulator.assign_ordinals();
        histories.push(ThreadTurnHistory {
            turn,
            items: accumulator.items,
            context_disposition: if rolled_back.contains(&turn_id) {
                ThreadContextDisposition::RolledBack
            } else {
                ThreadContextDisposition::Active
            },
        });
    }
    Ok(histories)
}

/// 从 pl-core 已提交的 typed effect 读取运行中 Turn 的可见条目。
/// 当前模型流尚未提交的正文不属于 durable history，由实时活动状态单独呈现。
pub(crate) fn project_active_turn_items_from_effects(
    thread_id: &str,
    effects: &[ThreadEffectBatch],
    turn_id: &str,
) -> Vec<ThreadItem> {
    let mut accumulators = collect_turns(thread_id, effects);
    let Some(mut turn) = accumulators.remove(turn_id) else {
        return Vec::new();
    };
    if !turn.started {
        return Vec::new();
    }
    turn.assign_ordinals();
    turn.items
}

fn ordered_effects(effects: &[ThreadEffectBatch]) -> Vec<&ThreadEffectBatch> {
    let mut ordered: Vec<_> = effects.iter().collect();
    ordered.sort_by_key(|effect| effect.sequence);
    ordered
}

fn collect_turns(
    thread_id: &str,
    effects: &[ThreadEffectBatch],
) -> BTreeMap<String, TurnAccumulator> {
    let ordered = ordered_effects(effects);
    let identities = collect_identities(&ordered);
    let mut accumulators = BTreeMap::new();
    for effect in &ordered {
        route_effect(thread_id, effect, &identities, &mut accumulators);
    }
    // 只有 Turn 没有 User context 记录时才用 canonical input 兜底。
    for (turn_id, accumulator) in accumulators.iter_mut() {
        if accumulator.has_user {
            continue;
        }
        let Some(inputs) = identities.accepted_by_turn.get(turn_id) else {
            continue;
        };
        let mut inputs = inputs.clone();
        inputs.sort_by_key(|input| input.sequence);
        for input in inputs.into_iter().rev() {
            accumulator.push_input_user(&input.record, input.sequence, input.committed_at);
        }
    }
    accumulators
}

/// 一个 Turn 终态 effect 的提交序号；用于分页游标（排他上界）。
pub(crate) fn turn_terminal_sequence(effects: &[ThreadEffectBatch], turn_id: &str) -> Option<u64> {
    effects
        .iter()
        .filter(|effect| {
            effect.turn.as_ref().is_some_and(|record| {
                record.turn_id == turn_id && record.state != CoreTurnState::Running
            })
        })
        .map(|effect| effect.sequence)
        .max()
}

/// 一轮 Turn 的归并累加器：事件按提交顺序入队，同 identity 后写覆盖并保留最初顺序。
struct TurnAccumulator {
    thread_id: String,
    turn_id: String,
    revision: u64,
    at: i64,
    started: bool,
    terminal: Option<TurnRecord>,
    terminal_sequence: u64,
    terminal_committed_at: i64,
    has_user: bool,
    items: Vec<ThreadItem>,
    index: BTreeMap<String, usize>,
    ranks: BTreeMap<String, u8>,
    attempts: Vec<RequestAttempt>,
    attempt_index: BTreeMap<String, usize>,
    tasks: BTreeMap<String, TaskRecord>,
}

impl TurnAccumulator {
    fn new(thread_id: &str, turn_id: &str) -> Self {
        Self {
            thread_id: thread_id.to_owned(),
            turn_id: turn_id.to_owned(),
            revision: 0,
            at: 0,
            started: false,
            terminal: None,
            terminal_sequence: 0,
            terminal_committed_at: 0,
            has_user: false,
            items: Vec::new(),
            index: BTreeMap::new(),
            ranks: BTreeMap::new(),
            attempts: Vec::new(),
            attempt_index: BTreeMap::new(),
            tasks: BTreeMap::new(),
        }
    }

    fn push(&mut self, item: ThreadItem, rank: u8) {
        let id = item.id.clone();
        if let Some(position) = self.index.get(&id).copied() {
            let previous_rank = self.ranks.get(&id).copied().unwrap_or(0);
            if rank >= previous_rank {
                self.items[position] = item;
                self.ranks.insert(id, rank);
            }
            return;
        }
        self.index.insert(id.clone(), self.items.len());
        self.ranks.insert(id, rank);
        self.items.push(item);
    }

    /// 把条目插到最前：用于补齐早于本轮 effect 的 canonical input。
    fn push_front(&mut self, item: ThreadItem, rank: u8) {
        let id = item.id.clone();
        if let Some(position) = self.index.get(&id).copied() {
            let previous_rank = self.ranks.get(&id).copied().unwrap_or(0);
            if rank >= previous_rank {
                self.items[position] = item;
                self.ranks.insert(id, rank);
            }
            return;
        }
        self.items.insert(0, item);
        for position in self.index.values_mut() {
            *position += 1;
        }
        self.index.insert(id.clone(), 0);
        self.ranks.insert(id, rank);
    }

    fn push_text(
        &mut self,
        id: String,
        channel: ThreadTextChannel,
        content: &[ContextContent],
        lifecycle: ThreadContentLifecycle,
    ) {
        if let Some(state) = text_state(channel, content, lifecycle) {
            let item = self.item(id, state);
            self.push(item, RANK_TEXT);
        }
    }

    fn set_tool(
        &mut self,
        call_id: &str,
        tool_id: &str,
        arguments: &str,
        state: ThreadToolState,
        rank: u8,
    ) {
        let id = tool_item_id(call_id);
        let (name, arguments) =
            match self
                .index
                .get(&id)
                .and_then(|position| match self.items[*position].state() {
                    ThreadItemState::Tool(tool) if arguments.is_empty() => Some((
                        tool.invocation().name().to_owned(),
                        tool.invocation().arguments().to_owned(),
                    )),
                    _ => None,
                }) {
                Some(previous) => previous,
                None => (tool_id.to_owned(), arguments.to_owned()),
            };
        let invocation = ThreadToolInvocation::new(call_id.to_owned(), name, arguments);
        let item = self.item(
            id,
            ThreadItemState::Tool(ThreadToolItem::new(invocation, state)),
        );
        self.push(item, rank);
    }

    fn push_attempt(&mut self, attempt: &AttemptUpdate) {
        let output = match &attempt.outcome {
            AttemptOutcome::Committed(output)
            | AttemptOutcome::Rejected { output, .. }
            | AttemptOutcome::Cancelled { result: Ok(output) } => output,
            AttemptOutcome::Running
            | AttemptOutcome::Interrupted
            | AttemptOutcome::Failed(_)
            | AttemptOutcome::Cancelled { result: Err(_) } => return,
        };
        // 先登记工具调用：即使 context 记录缺失，调用身份也不丢。
        for call in &output.tool_calls {
            self.set_tool(
                &call.call_id,
                &call.tool_id,
                call.arguments.content(),
                running_tool(),
                RANK_CALL,
            );
        }
        // 已提交的正文由同一条 commit 的 context 记录投影，避免与 Attempt 重复。
        if matches!(&attempt.outcome, AttemptOutcome::Committed(_)) {
            return;
        }
        let (lifecycle, channel) = match &attempt.outcome {
            AttemptOutcome::Rejected { reason, .. } => (
                ThreadContentLifecycle::failed(self.at, reason.to_string()),
                ThreadTextChannel::Commentary,
            ),
            AttemptOutcome::Cancelled { result: Ok(_) } => (
                ThreadContentLifecycle::cancelled(
                    self.at,
                    "模型在取消后返回，该正文未提交到 context".to_owned(),
                ),
                ThreadTextChannel::Commentary,
            ),
            AttemptOutcome::Committed(_)
            | AttemptOutcome::Running
            | AttemptOutcome::Interrupted
            | AttemptOutcome::Failed(_)
            | AttemptOutcome::Cancelled { result: Err(_) } => return,
        };
        let id = response_text_id(&attempt.attempt_id);
        self.push_text(id, channel, &output.content, lifecycle);
    }

    fn record_attempt(&mut self, attempt: &AttemptUpdate) {
        let request = RequestAttempt {
            request_metadata: attempt.request_metadata.clone(),
            usage_binding: attempt.usage_binding.clone(),
            tool_projection: attempt.tool_projection.clone(),
            turn_id: attempt.turn_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            retry_of: attempt.retry_of.clone(),
            input: ContextSnapshot {
                revision: attempt.input_revision,
                records: Vec::new().into(),
            },
            tools: attempt.tools.clone(),
            outcome: attempt.outcome.clone(),
            input_estimate: attempt.input_estimate,
        };
        match self.attempt_index.get(&attempt.attempt_id).copied() {
            Some(position) => self.attempts[position] = request,
            None => {
                self.attempt_index
                    .insert(attempt.attempt_id.clone(), self.attempts.len());
                self.attempts.push(request);
            }
        }
    }

    fn push_turn_item(&mut self, turn: &Turn) {
        let item = ThreadItem::new(
            turn_item_id(&turn.id),
            self.thread_id.clone(),
            turn.id.clone(),
            0,
            turn.revision,
            self.at,
            turn.updated_at,
            ThreadItemState::Turn(
                ThreadTurnItem::new(turn.state.clone()).with_input_id(turn.input_id.clone()),
            ),
        );
        self.push(item, RANK_TEXT);
    }

    fn item(&self, id: String, state: ThreadItemState) -> ThreadItem {
        ThreadItem::new(
            id,
            self.thread_id.clone(),
            self.turn_id.clone(),
            0,
            self.revision,
            self.at,
            self.at,
            state,
        )
    }

    /// 用归并到的 attempts/tasks 构造 canonical 快照，供 [`project_turn`] 派生失败与预算用量。
    fn synthetic_snapshot(&self) -> pl_core::thread::ThreadSnapshot {
        pl_core::thread::ThreadSnapshot {
            attempts: self.attempts.clone().into(),
            tasks: self.tasks.clone(),
            ..pl_core::thread::ThreadSnapshot::default()
        }
    }

    fn assign_ordinals(&mut self) {
        for (position, item) in self.items.iter_mut().enumerate() {
            item.ordinal = position as u64 + 1;
        }
    }
}

/// 归并所需的归属索引：input→turn、call→turn，以及按 turn 归档的 canonical input（兜底正文）。
struct Identities {
    call_turn: BTreeMap<String, String>,
    accepted_by_turn: BTreeMap<String, Vec<AcceptedInput>>,
}

#[derive(Clone)]
struct AcceptedInput {
    sequence: u64,
    committed_at: i64,
    record: InputRecord,
}

/// 预扫一遍 effect，建立 input→turn 与 call→turn 的归属索引。
fn collect_identities(effects: &[&ThreadEffectBatch]) -> Identities {
    let mut input_turn = BTreeMap::new();
    // 先收集 input 的 Turn 归属：Transition 与已带消费状态的 Accepted。
    for effect in effects {
        for change in effect.inputs.iter() {
            let (id, turn) = match change {
                InputChange::Accepted(record) => (
                    record.input.id.clone(),
                    input_record_turn(record).map(str::to_owned),
                ),
                InputChange::Transition { id, state, .. } => {
                    (id.clone(), input_state_turn(state).map(str::to_owned))
                }
            };
            if let Some(turn) = turn {
                input_turn.insert(id, turn);
            }
        }
    }
    // 再把每个 canonical input 归到它的 Turn 下（Transitions 已建立归属）。
    let mut accepted_by_turn: BTreeMap<String, Vec<AcceptedInput>> = BTreeMap::new();
    for effect in effects {
        for change in effect.inputs.iter() {
            if let InputChange::Accepted(record) = change
                && let Some(turn) = input_turn.get(&record.input.id)
            {
                accepted_by_turn
                    .entry(turn.clone())
                    .or_default()
                    .push(AcceptedInput {
                        sequence: effect.sequence,
                        committed_at: effect.committed_at,
                        record: record.clone(),
                    });
            }
        }
    }
    let mut call_turn = BTreeMap::new();
    for effect in effects {
        if let Some(attempt) = effect.attempt.as_ref()
            && let AttemptOutcome::Committed(output) | AttemptOutcome::Rejected { output, .. } =
                &attempt.outcome
        {
            for call in &output.tool_calls {
                call_turn.insert(call.call_id.clone(), attempt.turn_id.clone());
            }
        }
        for task in effect.tasks.iter() {
            call_turn.insert(task.call_id.clone(), task.turn_id.clone());
        }
        if let Some(change) = effect.context.as_ref() {
            for record in context_records(change) {
                let Some(turn_id) = record.turn_id.as_ref() else {
                    continue;
                };
                for call in &record.tool_calls {
                    call_turn.insert(call.call_id.clone(), turn_id.clone());
                }
                if let ContextSource::ToolResult { call_id, .. } = &record.source {
                    call_turn.insert(call_id.clone(), turn_id.clone());
                }
            }
        }
    }
    Identities {
        call_turn,
        accepted_by_turn,
    }
}

/// 把一条 effect 携带的 typed 事实路由到对应的 Turn 累加器。
fn route_effect(
    thread_id: &str,
    effect: &ThreadEffectBatch,
    identities: &Identities,
    accumulators: &mut BTreeMap<String, TurnAccumulator>,
) {
    if let Some(record) = effect.turn.as_ref() {
        let accumulator = accumulator(
            accumulators,
            thread_id,
            &record.turn_id,
            effect.sequence,
            effect.committed_at,
        );
        if record.state == CoreTurnState::Running {
            accumulator.started = true;
        } else {
            accumulator.terminal = Some(record.clone());
            accumulator.terminal_sequence = effect.sequence;
            accumulator.terminal_committed_at = effect.committed_at;
        }
    }

    if let Some(change) = effect.context.as_ref() {
        for record in context_records(change) {
            let Some(turn_id) = record.turn_id.as_deref() else {
                continue;
            };
            let accumulator = accumulator(
                accumulators,
                thread_id,
                turn_id,
                effect.sequence,
                effect.committed_at,
            );
            match &record.source {
                ContextSource::Assistant => accumulator.push_assistant(record),
                ContextSource::ToolResult { call_id, tool_id } => {
                    let result = context_text(&record.content);
                    let at = accumulator.at;
                    accumulator.set_tool(
                        call_id,
                        tool_id,
                        "",
                        succeeded(at, result),
                        RANK_CONTEXT_RESULT,
                    );
                }
                ContextSource::User => accumulator.push_context_user(record),
                // Instruction/Runtime 是当前上下文事实，不是 timeline 条目。
                ContextSource::Instruction | ContextSource::Runtime { .. } => {}
            }
        }
    }

    if let Some(attempt) = effect.attempt.as_ref() {
        let accumulator = accumulator(
            accumulators,
            thread_id,
            &attempt.turn_id,
            effect.sequence,
            effect.committed_at,
        );
        accumulator.push_attempt(attempt);
        accumulator.record_attempt(attempt);
    }

    for task in effect.tasks.iter() {
        let accumulator = accumulator(
            accumulators,
            thread_id,
            &task.turn_id,
            effect.sequence,
            effect.committed_at,
        );
        accumulator.tasks.insert(task.id.clone(), task.clone());
        let at = accumulator.at;
        accumulator.set_tool(
            &task.call_id,
            &task.tool_id,
            "",
            task_state(task, at),
            RANK_TASK,
        );
    }

    for delivery in effect.deliveries.iter() {
        let Some(turn_id) = identities.call_turn.get(&delivery.call_id) else {
            continue;
        };
        let accumulator = accumulator(
            accumulators,
            thread_id,
            turn_id,
            effect.sequence,
            effect.committed_at,
        );
        let at = accumulator.at;
        accumulator.set_tool(
            &delivery.call_id,
            &delivery.tool_id,
            "",
            delivery_state(delivery, at),
            RANK_DELIVERY,
        );
    }
}

fn accumulator<'a>(
    accumulators: &'a mut BTreeMap<String, TurnAccumulator>,
    thread_id: &str,
    turn_id: &str,
    sequence: u64,
    committed_at: i64,
) -> &'a mut TurnAccumulator {
    let accumulator = accumulators
        .entry(turn_id.to_owned())
        .or_insert_with(|| TurnAccumulator::new(thread_id, turn_id));
    accumulator.revision = sequence;
    accumulator.at = committed_at;
    accumulator
}

impl TurnAccumulator {
    /// context 里的用户消息（canonical input 的 model-visible 投影）。
    fn push_context_user(&mut self, record: &ContextRecord) {
        let Some(state) = text_state(
            ThreadTextChannel::User,
            &record.content,
            ThreadContentLifecycle::completed(self.at),
        ) else {
            return;
        };
        self.has_user = true;
        let item = self.item(record.id.clone(), state);
        self.push(item, RANK_TEXT);
    }

    /// canonical input 兜底：Turn 没有任何 User context 记录时按 input 身份投影。
    fn push_input_user(&mut self, input: &InputRecord, sequence: u64, committed_at: i64) {
        self.revision = sequence;
        self.at = committed_at;
        self.has_user = true;
        let state = text_state(
            ThreadTextChannel::User,
            &input.input.context,
            ThreadContentLifecycle::completed(self.at),
        )
        .unwrap_or_else(|| {
            ThreadItemState::Raw(ThreadRawItem {
                payloads: vec![raw_payload(&input.input.payload)],
                notice: "无法解码的产品输入载荷".to_owned(),
                recorded_at: self.at,
            })
        });
        let item = self.item(input.input.id.clone(), state);
        self.push_front(item, RANK_TEXT);
    }

    fn push_assistant(&mut self, record: &ContextRecord) {
        // 含工具调用的模型步骤是中间解说；没有工具调用的才是本 Turn 的最终回复。
        let channel = if record.tool_calls.is_empty() {
            ThreadTextChannel::Final
        } else {
            ThreadTextChannel::Commentary
        };
        self.push_text(
            record.id.clone(),
            channel,
            &record.content,
            ThreadContentLifecycle::completed(self.at),
        );
        for call in &record.tool_calls {
            self.set_tool(
                &call.call_id,
                &call.tool_id,
                call.arguments.content(),
                running_tool(),
                RANK_CALL,
            );
        }
    }
}

/// context 变更携带的记录全文；`Append` 只带新增后缀，`Replace` 带完整快照。
fn context_records(change: &ContextChange) -> &[ContextRecord] {
    match change {
        ContextChange::Append { records, .. } => &records[..],
        ContextChange::Replace(snapshot) => &snapshot.records[..],
    }
}

/// 归并所有 Rewind replacement：确实移除了某 Turn context 记录的 Turn 标记为回退。
fn rolled_back_turns(effects: &[&ThreadEffectBatch]) -> BTreeSet<String> {
    let mut removed = BTreeSet::new();
    for effect in effects {
        for replacement in effect.replacements.iter() {
            if replacement.reason != ContextReplacementReason::Rewind {
                continue;
            }
            let retained = replacement
                .current
                .records
                .iter()
                .filter_map(|record| record.turn_id.as_ref())
                .collect::<BTreeSet<_>>();
            for record in replacement.previous.records.iter() {
                if let Some(turn) = record.turn_id.as_ref()
                    && !retained.contains(turn)
                {
                    removed.insert(turn.clone());
                }
            }
        }
    }
    removed
}

fn input_record_turn(record: &InputRecord) -> Option<&str> {
    if let Some(turn) = input_state_turn(&record.state) {
        return Some(turn);
    }
    match &record.delivery {
        InputDelivery::CurrentTurn { turn_id } => Some(turn_id.as_str()),
        InputDelivery::NextTurn => None,
    }
}

fn input_state_turn(state: &InputState) -> Option<&str> {
    match state {
        InputState::Consumed { turn_id, .. } => Some(turn_id.as_str()),
        InputState::Pending | InputState::Discarded => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pl_core::context::{ContextContent, ContextRecord, OpaquePayload};
    use pl_core::model::{ModelStepOutput, ModelToolCall, ModelUsage};
    use pl_core::thread::input::{InputDelivery, ThreadInput};
    use pl_core::thread::journal::AttemptUpdate;
    use pl_core::thread::{
        ContextReplacement, ContextReplacementReason, RequestAttempt, ToolDelivery, ToolOutcome,
        TurnOutcome as CoreTurnOutcome,
    };
    use pl_protocol::{ThreadTextItem, ThreadToolState};
    use pretty_assertions::assert_eq;

    use super::*;

    const THREAD: &str = "thread-a";
    const COMMITTED_SECONDS: i64 = 1_700_000_000;

    fn text(value: &str) -> ContextContent {
        ContextContent::Text {
            text: Arc::from(value),
        }
    }

    fn input(id: &str, value: &str) -> InputRecord {
        InputRecord {
            accepted_sequence: 1,
            delivery: InputDelivery::NextTurn,
            input: ThreadInput {
                id: id.to_owned(),
                payload: OpaquePayload::text(value),
                context: vec![text(value)],
            },
            ordinal: 1,
            revision: 1,
            state: InputState::Pending,
        }
    }

    fn transition(id: &str, turn: &str) -> InputChange {
        InputChange::Transition {
            id: id.to_owned(),
            revision: 2,
            state: InputState::Consumed {
                turn_id: turn.to_owned(),
                attempt_id: format!("{turn}:0"),
            },
        }
    }

    fn user_context(id: &str, value: &str, turn: &str) -> ContextRecord {
        ContextRecord {
            id: id.to_owned(),
            turn_id: Some(turn.to_owned()),
            source: ContextSource::User,
            content: vec![text(value)],
            tool_calls: Vec::new(),
        }
    }

    fn assistant_context(
        id: &str,
        value: &str,
        calls: Vec<ModelToolCall>,
        turn: &str,
    ) -> ContextRecord {
        ContextRecord {
            id: id.to_owned(),
            turn_id: Some(turn.to_owned()),
            source: ContextSource::Assistant,
            content: if value.is_empty() {
                Vec::new()
            } else {
                vec![text(value)]
            },
            tool_calls: calls,
        }
    }

    fn call(call_id: &str) -> ModelToolCall {
        ModelToolCall {
            call_id: call_id.to_owned(),
            tool_id: "exec".to_owned(),
            arguments: OpaquePayload::text("{\"command\":\"ls\"}"),
        }
    }

    fn attempt(
        turn: &str,
        attempt_id: &str,
        output: Vec<ContextContent>,
        calls: Vec<ModelToolCall>,
    ) -> AttemptUpdate {
        AttemptUpdate {
            request_metadata: None,
            usage_binding: None,
            tool_projection: None,
            turn_id: turn.to_owned(),
            attempt_id: attempt_id.to_owned(),
            retry_of: None,
            input_revision: 1,
            tools: Vec::new().into(),
            outcome: AttemptOutcome::Committed(ModelStepOutput {
                attempt_id: attempt_id.to_owned(),
                base_context_revision: 1,
                content: output,
                tool_calls: calls,
                private_context: None,
                usage: ModelUsage::default(),
            }),
            input_estimate: None,
        }
    }

    fn start_effect(sequence: u64, turn: &str) -> ThreadEffectBatch {
        ThreadEffectBatch {
            committed_at: COMMITTED_SECONDS + sequence as i64,
            thread_id: THREAD.to_owned(),
            sequence,
            turn: Some(TurnRecord {
                elapsed_ms: None,
                input_id: None,
                turn_id: turn.to_owned(),
                state: CoreTurnState::Running,
                model_steps: 0,
            }),
            ..ThreadEffectBatch::default()
        }
    }

    fn terminal_effect(sequence: u64, turn: &str) -> ThreadEffectBatch {
        ThreadEffectBatch {
            committed_at: COMMITTED_SECONDS + sequence as i64,
            thread_id: THREAD.to_owned(),
            sequence,
            turn: Some(TurnRecord {
                elapsed_ms: Some(120),
                input_id: Some("input-1".to_owned()),
                turn_id: turn.to_owned(),
                state: CoreTurnState::Finished(CoreTurnOutcome::Completed),
                model_steps: 1,
            }),
            ..ThreadEffectBatch::default()
        }
    }

    fn concat_effect(sequence: u64, records: Vec<ContextRecord>) -> ThreadEffectBatch {
        ThreadEffectBatch {
            committed_at: COMMITTED_SECONDS + sequence as i64,
            thread_id: THREAD.to_owned(),
            sequence,
            context: Some(ContextChange::Append {
                revision: sequence,
                records: records.into(),
            }),
            ..ThreadEffectBatch::default()
        }
    }

    fn delivery_effect(
        sequence: u64,
        call_id: &str,
        outcome: ToolOutcome,
        result: &str,
    ) -> ThreadEffectBatch {
        ThreadEffectBatch {
            committed_at: COMMITTED_SECONDS + sequence as i64,
            thread_id: THREAD.to_owned(),
            sequence,
            deliveries: vec![ToolDelivery {
                target: pl_core::thread::ToolDeliveryTarget::CallResult,
                call_id: call_id.to_owned(),
                tool_id: "exec".to_owned(),
                output: pl_core::tool::ToolOutput::new(
                    OpaquePayload::text(result),
                    vec![text(result)],
                ),
                delivered_context: vec![text(result)],
                outcome,
            }]
            .into(),
            ..ThreadEffectBatch::default()
        }
    }

    fn project(effects: &[ThreadEffectBatch]) -> Vec<ThreadTurnHistory> {
        project_turn_history_from_effects(THREAD, effects).expect("project histories")
    }

    #[test]
    fn running_turn_exposes_committed_chat_without_a_terminal_effect() {
        let effects = vec![
            start_effect(1, "turn-1"),
            concat_effect(
                2,
                vec![
                    user_context("user-1", "please review", "turn-1"),
                    assistant_context("assistant-1", "checking files", Vec::new(), "turn-1"),
                ],
            ),
        ];

        assert_eq!(project(&effects).len(), 0);
        let items = project_active_turn_items_from_effects(THREAD, &effects, "turn-1");
        let texts = items
            .iter()
            .filter_map(ThreadItem::text)
            .map(ThreadTextItem::text)
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["please review", "checking files"]);
        assert_eq!(
            project_active_turn_items_from_effects(THREAD, &effects[1..], "turn-1"),
            Vec::new()
        );
    }

    fn text_of<'a>(history: &'a ThreadTurnHistory, value: &str) -> &'a ThreadTextItem {
        history
            .items
            .iter()
            .filter_map(ThreadItem::text)
            .find(|item| item.text() == value)
            .unwrap_or_else(|| panic!("missing text item `{value}`"))
    }

    #[test]
    fn merges_items_across_effects_without_the_current_snapshot() {
        let effects = vec![
            ThreadEffectBatch {
                inputs: vec![InputChange::Accepted(input("input-1", "please review"))].into(),
                ..start_effect(1, "turn-1")
            },
            ThreadEffectBatch {
                inputs: vec![transition("input-1", "turn-1")].into(),
                ..start_effect(2, "turn-1")
            },
            ThreadEffectBatch {
                // 已释放的旧 Turn：起始 effect 必须由调用方分页读回，投影不依赖当前快照。
                context: Some(ContextChange::Append {
                    revision: 3,
                    records: vec![
                        user_context("turn-1:0:input", "please review", "turn-1"),
                        assistant_context(
                            "turn-1:0:output",
                            "checking",
                            vec![call("call-1")],
                            "turn-1",
                        ),
                    ]
                    .into(),
                }),
                attempt: Some(attempt(
                    "turn-1",
                    "turn-1:0",
                    vec![text("checking")],
                    vec![call("call-1")],
                )),
                ..ThreadEffectBatch {
                    sequence: 3,
                    thread_id: THREAD.to_owned(),
                    committed_at: COMMITTED_SECONDS + 3,
                    ..ThreadEffectBatch::default()
                }
            },
            delivery_effect(4, "call-1", ToolOutcome::Succeeded, "listing ok"),
            terminal_effect(5, "turn-1"),
        ];

        let histories = project(&effects);
        assert_eq!(histories.len(), 1);
        let history = &histories[0];
        assert_eq!(history.turn.id, "turn-1");
        assert_eq!(
            history.context_disposition,
            ThreadContextDisposition::Active
        );

        // 用户消息来自 context 的 User 记录，且不会因同名 canonical input 重复。
        assert_eq!(
            text_of(history, "please review").channel(),
            ThreadTextChannel::User
        );
        assert_eq!(
            history
                .items
                .iter()
                .filter(|item| item
                    .text()
                    .is_some_and(|text| text.text() == "please review"))
                .count(),
            1
        );
        // 模型正文来自 context 记录全文（Attempt 已提交，不再重复投影）。
        assert_eq!(
            text_of(history, "checking").channel(),
            ThreadTextChannel::Commentary
        );
        assert_eq!(
            history
                .items
                .iter()
                .filter(|item| item.text().is_some_and(|text| text.text() == "checking"))
                .count(),
            1
        );

        let tool = history
            .items
            .iter()
            .filter_map(ThreadItem::tool)
            .find(|item| item.invocation().tool_call_id() == "call-1")
            .expect("tool item");
        assert_eq!(
            tool.terminal_output().expect("terminal output").result(),
            "listing ok"
        );
        assert_eq!(
            tool.invocation().arguments(),
            "{\"command\":\"ls\"}",
            "delivery 不应覆盖已记录的工具参数"
        );
        assert!(matches!(
            history.items.last().map(ThreadItem::state),
            Some(ThreadItemState::Turn(_))
        ));
        let ordinals: Vec<u64> = history.items.iter().map(|item| item.ordinal).collect();
        assert_eq!(ordinals, (1..=ordinals.len() as u64).collect::<Vec<_>>());
    }

    #[test]
    fn canonical_input_falls_back_when_context_has_no_user_record() {
        let effects = vec![
            ThreadEffectBatch {
                inputs: vec![InputChange::Accepted(input("input-7", "queued question"))].into(),
                ..start_effect(1, "turn-1")
            },
            ThreadEffectBatch {
                inputs: vec![transition("input-7", "turn-1")].into(),
                ..start_effect(2, "turn-1")
            },
            concat_effect(
                3,
                vec![assistant_context(
                    "turn-1:0:output",
                    "answer",
                    Vec::new(),
                    "turn-1",
                )],
            ),
            terminal_effect(4, "turn-1"),
        ];

        let histories = project(&effects);
        let history = &histories[0];
        assert_eq!(
            text_of(history, "queued question").channel(),
            ThreadTextChannel::User
        );
        // 兜底用户消息排在时间线最前，且复用 canonical input 身份。
        assert_eq!(history.items.first().expect("first item").id, "input-7");
    }

    #[test]
    fn tool_delivery_failure_and_cancellation_override_running_state() {
        let failed = delivery_effect(
            3,
            "call-fail",
            ToolOutcome::Failed(Arc::new(pl_core::tool::opaque::ToolError::new(
                std::io::Error::other("boom"),
            ))),
            "failed",
        );
        let cancelled = delivery_effect(4, "call-cancel", ToolOutcome::Cancelled, "cancelled");
        let effects = vec![
            start_effect(1, "turn-1"),
            concat_effect(
                2,
                vec![assistant_context(
                    "turn-1:0:output",
                    "",
                    vec![call("call-fail"), call("call-cancel")],
                    "turn-1",
                )],
            ),
            failed,
            cancelled,
            terminal_effect(5, "turn-1"),
        ];

        let histories = project(&effects);
        let tools: Vec<_> = histories[0]
            .items
            .iter()
            .filter_map(ThreadItem::tool)
            .collect();
        let fail = tools
            .iter()
            .find(|item| item.invocation().tool_call_id() == "call-fail")
            .expect("failed tool");
        assert!(matches!(fail.state(), ThreadToolState::Failed(_)));
        let cancel = tools
            .iter()
            .find(|item| item.invocation().tool_call_id() == "call-cancel")
            .expect("cancelled tool");
        assert!(matches!(cancel.state(), ThreadToolState::Cancelled(_)));
    }

    #[test]
    fn rewind_replacement_marks_only_the_removed_turn() {
        let rewind = ThreadEffectBatch {
            committed_at: COMMITTED_SECONDS + 4,
            thread_id: THREAD.to_owned(),
            sequence: 4,
            replacements: vec![ContextReplacement {
                reason: ContextReplacementReason::Rewind,
                previous: ContextSnapshot {
                    revision: 3,
                    records: vec![user_context("turn-1:0:input", "obsolete", "turn-1")].into(),
                },
                current: ContextSnapshot::default(),
                previous_private_context: None,
            }]
            .into(),
            ..ThreadEffectBatch::default()
        };
        let effects = vec![
            start_effect(1, "turn-1"),
            terminal_effect(2, "turn-1"),
            rewind,
            start_effect(5, "turn-2"),
            terminal_effect(6, "turn-2"),
        ];

        let histories = project(&effects);
        assert_eq!(
            histories
                .iter()
                .find(|history| history.turn.id == "turn-1")
                .expect("turn-1")
                .context_disposition,
            ThreadContextDisposition::RolledBack
        );
        assert_eq!(
            histories
                .iter()
                .find(|history| history.turn.id == "turn-2")
                .expect("turn-2")
                .context_disposition,
            ThreadContextDisposition::Active
        );
    }

    #[test]
    fn paging_returns_only_turns_fully_covered_by_read_pages() {
        // 第一页：turn-1 完整，turn-2 只有终态（起始 effect 还在下一页）。
        let first_page = vec![
            start_effect(1, "turn-1"),
            terminal_effect(2, "turn-1"),
            terminal_effect(3, "turn-2"),
        ];
        let histories = project(&first_page);
        assert_eq!(
            histories
                .iter()
                .map(|history| history.turn.id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn-1"],
            "未覆盖起始 effect 的 Turn 不能输出"
        );
        // 分页游标是最旧一个完整 Turn 的终态序号。
        assert_eq!(turn_terminal_sequence(&first_page, "turn-1"), Some(2));

        // 第二页：把 turn-2 的起始 effect 读回后，两轮都完整。
        let two_pages = vec![
            start_effect(1, "turn-1"),
            terminal_effect(2, "turn-1"),
            start_effect(3, "turn-2"),
            terminal_effect(4, "turn-2"),
        ];
        let histories = project(&two_pages);
        assert_eq!(
            histories
                .iter()
                .map(|history| history.turn.id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn-2", "turn-1"],
            "最新 Turn 在前"
        );
    }

    #[test]
    fn terminal_sequence_is_the_exclusive_page_upper_bound() {
        let effects = vec![start_effect(1, "turn-1"), terminal_effect(2, "turn-1")];
        assert_eq!(turn_terminal_sequence(&effects, "turn-1"), Some(2));
        assert_eq!(turn_terminal_sequence(&effects, "turn-missing"), None);
    }

    #[test]
    fn failure_attempt_state_is_reconstructed_from_effects() {
        let mut failing = start_effect(2, "turn-1");
        failing.attempt = Some(AttemptUpdate {
            request_metadata: None,
            usage_binding: None,
            tool_projection: None,
            turn_id: "turn-1".to_owned(),
            attempt_id: "turn-1:0".to_owned(),
            retry_of: None,
            input_revision: 1,
            tools: Vec::new().into(),
            outcome: AttemptOutcome::Failed(Arc::new(pl_core::model::ModelError {
                details: None,
                kind: pl_core::model::ModelFailureKind::Unavailable,
                usage: ModelUsage::default(),
                source: None,
            })),
            input_estimate: None,
        });
        let mut failed_turn = terminal_effect(3, "turn-1");
        if let Some(record) = failed_turn.turn.as_mut() {
            record.state = CoreTurnState::Failed {
                description: "provider unavailable".to_owned(),
            };
        }

        let histories = project(&[start_effect(1, "turn-1"), failing, failed_turn]);
        assert_eq!(histories.len(), 1);
        assert!(matches!(
            histories[0].turn.state,
            pl_protocol::TurnState::Failed(_)
        ));
        // 归并出的 attempt 让结构化失败而不是仅描述文本。
        assert!(histories[0].turn.failure().is_some());
    }

    #[test]
    fn synthetic_snapshot_keeps_attempt_identity() {
        let mut accumulator = TurnAccumulator::new(THREAD, "turn-1");
        accumulator.record_attempt(&attempt("turn-1", "turn-1:0", vec![text("x")], Vec::new()));
        let _: Vec<RequestAttempt> = accumulator.attempts.clone();
        assert_eq!(accumulator.attempts[0].attempt_id, "turn-1:0");
    }
}
