use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde::Deserialize;

use crate::args::validate_image;
use crate::error::{DockerError, Result};

/// 受管快照镜像的收据事实；用于服务重启后按 operation 找回 commit 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotImageFacts {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

impl SnapshotImageFacts {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }
}

/// 解析 `docker image inspect` 输出中的第一个镜像。
pub(crate) fn snapshot_image_facts_from_inspect(json: &str) -> Result<Option<SnapshotImageFacts>> {
    let inspected = serde_json::from_str::<Vec<InspectImage>>(json)?;
    Ok(inspected.into_iter().next().map(SnapshotImageFacts::from))
}

#[derive(Debug, Deserialize)]
struct InspectImage {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Config")]
    config: Option<InspectImageConfig>,
}

#[derive(Debug, Deserialize)]
struct InspectImageConfig {
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

impl From<InspectImage> for SnapshotImageFacts {
    fn from(value: InspectImage) -> Self {
        Self {
            id: value.id,
            repo_tags: value
                .repo_tags
                .unwrap_or_default()
                .into_iter()
                .filter(|tag| !tag.is_empty() && tag != "<none>:<none>")
                .collect(),
            labels: value
                .config
                .and_then(|config| config.labels)
                .unwrap_or_default()
                .into_iter()
                .collect(),
        }
    }
}

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
