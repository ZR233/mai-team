use crate::error::{ModelError, Result};
use futures::Stream;
use mai_protocol::{ModelOutputItem, ModelOutputToolCall, ModelResponse, TokenUsage};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;

#[derive(Debug, Clone, Default)]
pub struct ModelTurnState {
    pub previous_response_id: Option<String>,
    pub acknowledged_input_len: usize,
    pub(crate) continuation_disabled: bool,
}

impl ModelTurnState {
    pub fn acknowledge_history_len(&mut self, len: usize) {
        self.acknowledged_input_len = len;
    }
}

pub type ModelEventStream = Pin<Box<dyn Stream<Item = Result<ModelStreamEvent>> + Send>>;

#[derive(Debug, Clone)]
pub enum ModelStreamEvent {
    ResponseStarted {
        id: Option<String>,
    },
    Status {
        status: ModelStreamStatus,
    },
    TextDelta {
        output_index: usize,
        content_index: Option<usize>,
        delta: String,
    },
    ReasoningDelta {
        output_index: usize,
        content_index: Option<usize>,
        delta: String,
    },
    ToolCallStarted {
        output_index: usize,
        call_id: Option<String>,
        name: Option<String>,
    },
    ToolCallArgumentsDelta {
        output_index: usize,
        delta: String,
    },
    CustomToolInputDelta {
        item_id: String,
        call_id: Option<String>,
        delta: String,
    },
    OutputItemAdded {
        output_index: usize,
        item: ModelOutputItem,
    },
    OutputItemDone {
        output_index: usize,
        item: ModelOutputItem,
    },
    Completed {
        id: Option<String>,
        usage: Option<TokenUsage>,
        end_turn: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamStatus {
    Created,
    Queued,
    InProgress,
    Completed,
    Incomplete,
    Other(String),
}

#[derive(Debug, Clone, Default)]
pub struct ModelStreamAccumulator {
    id: Option<String>,
    usage: Option<TokenUsage>,
    completed: bool,
    outputs: Vec<ModelOutputItem>,
    done_output_indices: BTreeSet<usize>,
    text_parts: BTreeMap<usize, String>,
    reasoning_parts: BTreeMap<usize, String>,
    tool_calls: BTreeMap<usize, PendingToolCall>,
}

#[derive(Debug, Clone, Default)]
struct PendingToolCall {
    call_id: Option<String>,
    name: Option<String>,
    raw_arguments: String,
}

impl ModelStreamAccumulator {
    pub fn push(&mut self, event: &ModelStreamEvent) {
        match event {
            ModelStreamEvent::ResponseStarted { id } => {
                if id.is_some() {
                    self.id = id.clone();
                }
            }
            ModelStreamEvent::Completed { id, usage, .. } => {
                self.completed = true;
                if id.is_some() {
                    self.id = id.clone();
                }
                if let Some(usage) = usage {
                    self.usage = Some(usage.clone());
                }
            }
            ModelStreamEvent::TextDelta {
                output_index,
                delta,
                ..
            } => {
                if self.done_output_indices.contains(output_index) {
                    return;
                }
                self.text_parts
                    .entry(*output_index)
                    .or_default()
                    .push_str(delta);
            }
            ModelStreamEvent::ReasoningDelta {
                output_index,
                delta,
                ..
            } => {
                if self.done_output_indices.contains(output_index) {
                    return;
                }
                self.reasoning_parts
                    .entry(*output_index)
                    .or_default()
                    .push_str(delta);
            }
            ModelStreamEvent::ToolCallStarted {
                output_index,
                call_id,
                name,
            } => {
                if self.done_output_indices.contains(output_index) {
                    return;
                }
                let pending = self.tool_calls.entry(*output_index).or_default();
                if call_id.is_some() {
                    pending.call_id = call_id.clone();
                }
                if name.is_some() {
                    pending.name = name.clone();
                }
            }
            ModelStreamEvent::ToolCallArgumentsDelta {
                output_index,
                delta,
            } => {
                if self.done_output_indices.contains(output_index) {
                    return;
                }
                self.tool_calls
                    .entry(*output_index)
                    .or_default()
                    .raw_arguments
                    .push_str(delta);
            }
            ModelStreamEvent::OutputItemAdded { .. } => {}
            ModelStreamEvent::OutputItemDone { output_index, item } => {
                self.done_output_indices.insert(*output_index);
                self.text_parts.remove(output_index);
                self.reasoning_parts.remove(output_index);
                self.tool_calls.remove(output_index);
                self.outputs.push(item.clone());
            }
            ModelStreamEvent::CustomToolInputDelta { .. } | ModelStreamEvent::Status { .. } => {}
        }
    }

    pub fn finish(self) -> Result<ModelResponse> {
        if !self.completed {
            return Err(ModelError::Stream(
                "stream closed before response.completed".to_string(),
            ));
        }
        Ok(self.finish_partial())
    }

    pub(crate) fn finish_partial(mut self) -> ModelResponse {
        let mut indices = self
            .text_parts
            .keys()
            .chain(self.reasoning_parts.keys())
            .chain(self.tool_calls.keys())
            .copied()
            .collect::<Vec<_>>();
        indices.sort_unstable();
        indices.dedup();

        for index in indices {
            let content = self
                .text_parts
                .remove(&index)
                .filter(|text| !text.is_empty());
            let reasoning_content = self
                .reasoning_parts
                .remove(&index)
                .filter(|text| !text.trim().is_empty());
            let tool_call = self.tool_calls.remove(&index);
            match tool_call {
                Some(tool_call) => {
                    let raw_arguments = tool_call.raw_arguments;
                    let arguments = parse_arguments(&raw_arguments);
                    let output_tool_call = ModelOutputToolCall {
                        call_id: tool_call.call_id.unwrap_or_default(),
                        name: tool_call.name.unwrap_or_default(),
                        arguments,
                        raw_arguments,
                    };
                    self.outputs.push(ModelOutputItem::AssistantTurn {
                        content,
                        reasoning_content,
                        tool_calls: vec![output_tool_call],
                    });
                }
                None if reasoning_content.is_some() => {
                    self.outputs.push(ModelOutputItem::AssistantTurn {
                        content,
                        reasoning_content,
                        tool_calls: Vec::new(),
                    });
                }
                None if let Some(text) = content => {
                    self.outputs.push(ModelOutputItem::Message { text });
                }
                None => {}
            }
        }

        ModelResponse {
            id: self.id,
            output: self.outputs,
            usage: self.usage,
        }
    }
}

pub fn parse_arguments(raw_arguments: &str) -> Value {
    serde_json::from_str(raw_arguments).unwrap_or_else(|_| json!({ "raw": raw_arguments }))
}
