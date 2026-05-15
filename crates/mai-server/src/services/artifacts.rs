use std::sync::Arc;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use mai_protocol::{ArtifactInfo, TaskId};
use mai_runtime::AgentRuntime;
use mai_store::ConfigStore;

#[derive(Debug)]
pub(crate) enum ArtifactError {
    NotFound(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtifactError::NotFound(msg) => write!(f, "{msg}"),
            ArtifactError::Other(err) => write!(f, "{err}"),
        }
    }
}

impl From<anyhow::Error> for ArtifactError {
    fn from(err: anyhow::Error) -> Self {
        ArtifactError::Other(err)
    }
}

impl From<std::io::Error> for ArtifactError {
    fn from(err: std::io::Error) -> Self {
        ArtifactError::Other(err.into())
    }
}

impl From<mai_store::StoreError> for ArtifactError {
    fn from(err: mai_store::StoreError) -> Self {
        ArtifactError::Other(err.into())
    }
}

pub(crate) struct ArtifactService {
    store: Arc<ConfigStore>,
    runtime: Arc<AgentRuntime>,
}

impl ArtifactService {
    pub(crate) fn new(store: Arc<ConfigStore>, runtime: Arc<AgentRuntime>) -> Self {
        Self { store, runtime }
    }

    pub(crate) fn list_artifacts(&self, task_id: &TaskId) -> anyhow::Result<Vec<ArtifactInfo>> {
        self.store.load_artifacts(task_id).map_err(Into::into)
    }

    pub(crate) async fn download_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<DownloadFile, ArtifactError> {
        let artifact = self
            .store
            .load_artifact_by_id(artifact_id)?
            .ok_or_else(|| {
                ArtifactError::NotFound(format!("artifact not found: {artifact_id}"))
            })?;

        let file_path = self.runtime.artifact_file_path(&artifact);
        let bytes = tokio::fs::read(&file_path).await?;
        Ok(DownloadFile {
            bytes,
            filename: artifact.name,
        })
    }
}

pub(crate) struct DownloadFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) filename: String,
}

fn content_disposition_filename(name: &str) -> String {
    let escaped = name
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\r' | '\n' => '_',
            ch if ch.is_control() || !ch.is_ascii() => '_',
            ch => ch,
        })
        .collect::<String>();
    format!("attachment; filename=\"{escaped}\"")
}

impl DownloadFile {
    pub(crate) fn into_response(self) -> Response {
        let filename = content_disposition_filename(&self.filename);
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_DISPOSITION, filename),
            ],
            Body::from(self.bytes),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_escapes_special_characters() {
        assert_eq!(
            content_disposition_filename("report.pdf"),
            r#"attachment; filename="report.pdf""#
        );
        assert_eq!(
            content_disposition_filename(r#"file"name.txt"#),
            r#"attachment; filename="file_name.txt""#
        );
        assert_eq!(
            content_disposition_filename("file\\path.dat"),
            r#"attachment; filename="file_path.dat""#
        );
    }

    #[test]
    fn content_disposition_replaces_control_and_non_ascii() {
        assert_eq!(
            content_disposition_filename("file\r\n.csv"),
            r#"attachment; filename="file__.csv""#
        );
        assert_eq!(
            content_disposition_filename("文件.zip"),
            r#"attachment; filename="__.zip""#
        );
    }

    #[test]
    fn download_file_response_has_correct_headers() {
        let file = DownloadFile {
            bytes: vec![1, 2, 3],
            filename: "data.bin".to_string(),
        };
        let resp = file.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers();
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            headers.get(header::CONTENT_DISPOSITION).unwrap(),
            r#"attachment; filename="data.bin""#
        );
    }
}
