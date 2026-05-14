use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use mai_protocol::{ArtifactInfo, TaskId};
use mai_runtime::AgentRuntime;
use mai_store::ConfigStore;

pub(crate) struct ArtifactService {
    store: Arc<ConfigStore>,
    runtime: Arc<AgentRuntime>,
}

impl ArtifactService {
    pub(crate) fn new(store: Arc<ConfigStore>, runtime: Arc<AgentRuntime>) -> Self {
        Self { store, runtime }
    }

    pub(crate) fn list_artifacts(&self, task_id: &TaskId) -> Result<Vec<ArtifactInfo>> {
        self.store.load_artifacts(task_id).map_err(Into::into)
    }

    pub(crate) async fn download_artifact(&self, artifact_id: &str) -> Result<DownloadFile> {
        let artifacts = self.store.load_all_artifacts()?;
        let artifact = artifacts
            .into_iter()
            .find(|a| a.id == artifact_id)
            .ok_or_else(|| anyhow::anyhow!("Artifact not found"))?;

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
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_DISPOSITION, filename)
            .body(Body::from(self.bytes))
            .expect("download response")
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
