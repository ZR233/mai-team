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
