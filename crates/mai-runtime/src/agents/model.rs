use mai_protocol::ModelConfig;

use crate::{Result, RuntimeError};

pub(crate) fn normalize_reasoning_effort(
    model: &ModelConfig,
    effort: Option<&str>,
    default_when_missing: bool,
) -> Result<Option<String>> {
    let Some(reasoning) = &model.reasoning else {
        return Ok(None);
    };
    match effort {
        Some(value) if value.trim().is_empty() || value == "none" => Ok(None),
        Some(value) if reasoning.variants.iter().any(|variant| variant.id == value) => {
            Ok(Some(value.to_string()))
        }
        Some(effort) => Err(RuntimeError::InvalidInput(format!(
            "reasoning effort `{}` is not supported by model `{}`",
            effort, model.id
        ))),
        None if default_when_missing => Ok(default_reasoning_effort(model)),
        None => Ok(None),
    }
}

fn default_reasoning_effort(model: &ModelConfig) -> Option<String> {
    let reasoning = model.reasoning.as_ref()?;
    reasoning
        .default_variant
        .as_ref()
        .filter(|variant| {
            reasoning
                .variants
                .iter()
                .any(|item| item.id == variant.as_str())
        })
        .cloned()
        .or_else(|| reasoning.variants.first().map(|variant| variant.id.clone()))
}
