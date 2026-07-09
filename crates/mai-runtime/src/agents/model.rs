use mai_protocol::ModelConfig;
use pl_model::{
    MissingCandidatePolicy, ModelParameterCandidateError, ModelParameterCandidateRequest,
};

use crate::model_profile::reasoning_parameter;
use crate::{Result, RuntimeError};

pub(crate) fn normalize_reasoning_effort(
    model: &ModelConfig,
    effort: Option<&str>,
    default_when_missing: bool,
) -> Result<Option<String>> {
    let Some(reasoning) = &model.reasoning else {
        return Ok(None);
    };
    let Some(parameter) = reasoning_parameter(model) else {
        return Ok(None);
    };

    parameter
        .resolve_candidate(ModelParameterCandidateRequest {
            requested: effort,
            default_candidate: reasoning.default_variant.as_deref(),
            missing: if default_when_missing {
                MissingCandidatePolicy::UseDefault
            } else {
                MissingCandidatePolicy::Omit
            },
            disabled_values: &["none"],
        })
        .map_err(|error| reasoning_effort_error(model, error))
}

fn reasoning_effort_error(
    model: &ModelConfig,
    error: ModelParameterCandidateError,
) -> RuntimeError {
    match error {
        ModelParameterCandidateError::UnsupportedCandidate { candidate, .. } => {
            RuntimeError::InvalidInput(format!(
                "reasoning effort `{}` is not supported by model `{}`",
                candidate, model.id
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reasoning_effort_normalization_uses_pl_model_candidate_resolution() {
        let source = include_str!("model.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        assert!(
            production.contains("resolve_candidate"),
            "reasoning effort 候选值解析应由 pl-model ModelParameter 统一提供"
        );
        for forbidden in [
            "reasoning.variants.iter().any",
            "fn default_reasoning_effort",
            ".default_variant\n        .as_ref()",
        ] {
            assert!(
                !production.contains(forbidden),
                "mai-runtime 不应复制 reasoning effort 候选值解析逻辑 `{forbidden}`"
            );
        }
    }
}
