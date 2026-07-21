use std::future::Future;

use mai_protocol::{ProjectId, ProjectSummary};
use serde_json::Value;

use crate::github::github_path_segment;
use crate::{Result, RuntimeError};

/// 尚未解析到 GitHub 当前 revision 的 review 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReviewRequest {
    pub(crate) pr: u64,
    pub(crate) head_sha_hint: Option<String>,
}

/// review preparation 必须检出的精确 PR revision。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedProjectReviewTarget {
    pub(crate) pr: u64,
    pub(crate) head_sha: String,
}

/// 提供解析 review 目标所需的项目和 GitHub 只读数据。
///
/// 实现不得创建 reviewer、修改仓库或提交 review；解析结果只描述调用时
/// GitHub 上的当前 PR head，后续 preparation 仍需校验本地 ref。
pub(crate) trait ProjectReviewTargetOps: Send + Sync {
    fn project_summary(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Result<ProjectSummary>> + Send;

    fn github_api_get_json(
        &self,
        project_id: ProjectId,
        path: String,
    ) -> impl Future<Output = Result<Value>> + Send;
}

pub(crate) async fn resolve_project_review_target(
    ops: &impl ProjectReviewTargetOps,
    project_id: ProjectId,
    request: ProjectReviewRequest,
) -> Result<ResolvedProjectReviewTarget> {
    if request.pr == 0 {
        return Err(RuntimeError::InvalidInput(
            "project review target PR must be greater than zero".to_string(),
        ));
    }
    let project = ops.project_summary(project_id).await?;
    let path = format!(
        "/repos/{}/{}/pulls/{}",
        github_path_segment(&project.owner),
        github_path_segment(&project.repo),
        request.pr
    );
    let detail = ops.github_api_get_json(project_id, path).await?;
    let head_sha = detail
        .get("head")
        .and_then(|head| head.get("sha"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "GitHub pull request #{} response is missing head SHA",
                request.pr
            ))
        })?
        .to_string();
    if request
        .head_sha_hint
        .as_deref()
        .is_some_and(|hint| hint != head_sha)
    {
        tracing::info!(
            project_id = %project_id,
            pr = request.pr,
            head_sha_hint = request.head_sha_hint.as_deref().unwrap_or_default(),
            resolved_head_sha = %head_sha,
            "project review signal head changed before preparation; using current GitHub head"
        );
    }
    Ok(ResolvedProjectReviewTarget {
        pr: request.pr,
        head_sha,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use mai_protocol::{ProjectCloneStatus, ProjectReviewStatus, ProjectStatus, ProjectSummary};
    use pretty_assertions::assert_eq;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;

    #[derive(Clone)]
    struct FakeTargetOps {
        project: ProjectSummary,
        requested_paths: Arc<Mutex<Vec<String>>>,
        response: Value,
    }

    impl ProjectReviewTargetOps for FakeTargetOps {
        async fn project_summary(&self, _project_id: ProjectId) -> Result<ProjectSummary> {
            Ok(self.project.clone())
        }

        async fn github_api_get_json(&self, _project_id: ProjectId, path: String) -> Result<Value> {
            self.requested_paths.lock().await.push(path);
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn stale_hint_resolves_to_current_github_head() {
        let project_id = Uuid::new_v4();
        let ops = FakeTargetOps {
            project: project(project_id),
            requested_paths: Arc::new(Mutex::new(Vec::new())),
            response: serde_json::json!({ "head": { "sha": "current-head" } }),
        };

        let target = resolve_project_review_target(
            &ops,
            project_id,
            ProjectReviewRequest {
                pr: 42,
                head_sha_hint: Some("stale-head".to_string()),
            },
        )
        .await
        .expect("resolve target");

        assert_eq!(
            target,
            ResolvedProjectReviewTarget {
                pr: 42,
                head_sha: "current-head".to_string(),
            }
        );
        assert_eq!(
            *ops.requested_paths.lock().await,
            vec!["/repos/owner/repo/pulls/42".to_string()]
        );
    }

    fn project(id: ProjectId) -> ProjectSummary {
        let now = Utc::now();
        ProjectSummary {
            id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account".to_string()),
            repository_id: 1,
            installation_id: 1,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "sidecar".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Idle,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        }
    }
}
