use std::time::Duration;

use crate::args::validate_image;
use crate::error::{DockerError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRefreshOutcome {
    NotRequired {
        image: String,
    },
    UpToDate {
        image: String,
        image_id: String,
        elapsed: Duration,
    },
    Updated {
        image: String,
        previous_image_id: Option<String>,
        image_id: String,
        elapsed: Duration,
    },
    CachedFallback {
        image: String,
        image_id: String,
        elapsed: Duration,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum ImageRefreshFailure {
    NotAvailable(String),
    CommandFailed(String),
    Cancelled,
}

impl ImageRefreshFailure {
    pub(crate) fn from_docker_error(error: DockerError) -> Self {
        match error {
            DockerError::NotAvailable(message) => Self::NotAvailable(message),
            DockerError::CommandFailed(message) => Self::CommandFailed(message),
            DockerError::Cancelled => Self::Cancelled,
            other => Self::CommandFailed(other.to_string()),
        }
    }

    pub(crate) fn into_docker_error(self) -> DockerError {
        match self {
            Self::NotAvailable(message) => DockerError::NotAvailable(message),
            Self::CommandFailed(message) => DockerError::CommandFailed(message),
            Self::Cancelled => DockerError::Cancelled,
        }
    }
}

pub(crate) fn floating_latest_reference(image: &str) -> Result<Option<String>> {
    validate_image(image)?;
    if image.contains('@') {
        return Ok(None);
    }
    let last_component = image.rsplit('/').next().unwrap_or(image);
    match last_component.rsplit_once(':') {
        Some((_, "latest")) => Ok(Some(image.to_string())),
        Some((_, _)) => Ok(None),
        None => Ok(Some(format!("{image}:latest"))),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::floating_latest_reference;

    #[test]
    fn recognizes_explicit_and_implicit_latest_references() {
        assert_eq!(
            floating_latest_reference("ghcr.io/example/reviewer:latest").unwrap(),
            Some("ghcr.io/example/reviewer:latest".to_string())
        );
        assert_eq!(
            floating_latest_reference("localhost:5000/example/reviewer").unwrap(),
            Some("localhost:5000/example/reviewer:latest".to_string())
        );
    }

    #[test]
    fn skips_fixed_tags_and_digest_pinned_references() {
        assert_eq!(
            floating_latest_reference("ghcr.io/example/reviewer:v1").unwrap(),
            None
        );
        assert_eq!(
            floating_latest_reference("ghcr.io/example/reviewer:latest@sha256:abc").unwrap(),
            None
        );
    }
}
