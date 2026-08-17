use pl_model::{
    MissingCandidatePolicy, ModelInfo, ModelParameterCandidateError, ModelParameterCandidateRequest,
};

use crate::{Result, RuntimeError};

pub(crate) fn normalize_reasoning_effort(
    model: &ModelInfo,
    effort: Option<&str>,
    default_when_missing: bool,
) -> Result<Option<String>> {
    let Some(parameter) = model.effort_parameter() else {
        return Ok(None);
    };
    parameter
        .resolve_candidate(ModelParameterCandidateRequest {
            requested: effort,
            default_candidate: None,
            missing: if default_when_missing {
                MissingCandidatePolicy::UseDefault
            } else {
                MissingCandidatePolicy::Omit
            },
            disabled_values: &["none"],
        })
        .map_err(|error| reasoning_effort_error(model, error))
}

fn reasoning_effort_error(model: &ModelInfo, error: ModelParameterCandidateError) -> RuntimeError {
    match error {
        ModelParameterCandidateError::UnsupportedCandidate { candidate, .. } => {
            RuntimeError::InvalidInput(format!(
                "reasoning effort `{candidate}` is not supported by model `{}`",
                model.slug
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reasoning_effort_uses_pl_model_candidate_resolution() {
        let source = include_str!("model.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production.contains("resolve_candidate"));
        assert!(!production.contains("reasoning.variants"));
    }
}
