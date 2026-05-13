use mai_protocol::{ModelConfig, ModelInputItem, ProviderSecret, ToolDefinition};

pub struct ModelRequest<'a> {
    pub provider: &'a ProviderSecret,
    pub model: &'a ModelConfig,
    pub instructions: &'a str,
    pub input: &'a [ModelInputItem],
    pub tools: &'a [ToolDefinition],
    pub reasoning_effort: Option<String>,
}

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
