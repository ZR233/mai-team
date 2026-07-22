use std::sync::Arc;

use chrono::{DateTime, Utc};
use mai_docker::{SidecarParams, project_agent_workspace_volume};
use mai_protocol::{
    GitProvider, ProjectId, ProjectReviewDecision, ProjectReviewFailure,
    ProjectReviewFailureCategory, ProjectReviewSubmissionIntent, ProjectReviewSubmissionReceipt,
    ProjectSummary, now,
};
use pl_core::shell_quote_word;
use serde_json::Value;
use uuid::Uuid;

use crate::github::GitAccountToken;
use crate::state::AgentRecord;
use crate::turn::tool_output::ToolExecution;
use crate::{
    AgentRuntime, Result, RuntimeError, github, normalized_text, projects, redact_secret, turn,
};

const INVALID_INLINE_POSITION_CODE: &str = "invalid_inline_position";

impl AgentRuntime {
    pub(crate) async fn project_git_token(&self, project_id: ProjectId) -> Result<Option<String>> {
        Ok(self
            .project_git_token_details(project_id)
            .await?
            .map(|token| token.token))
    }

    pub(crate) async fn project_git_token_details(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<GitAccountToken>> {
        let project = self.project(project_id).await?;
        let summary = project.summary.read().await.clone();
        let Some(account_id) = summary.git_account_id else {
            return Ok(None);
        };
        Ok(Some(
            self.deps.git_accounts.token_details(&account_id).await?,
        ))
    }

    pub(crate) async fn project_git_token_for_agent(
        &self,
        agent: &AgentRecord,
    ) -> Result<Option<String>> {
        let Some(project_id) = agent.summary.read().await.project_id else {
            return Ok(None);
        };
        self.project_git_token(project_id).await
    }

    pub(crate) async fn execute_project_github_api_request(
        &self,
        agent: &AgentRecord,
        request: &turn::product_tools::GithubApiRequest,
    ) -> Result<ToolExecution> {
        let method = github::normalize_github_api_method(&request.method)?;
        let path = github::normalize_github_api_get_path(&request.path)?;
        let token = self
            .project_git_token_for_agent(agent)
            .await?
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "project git account token is not configured".to_string(),
                )
            })?;
        let summary = agent.summary.read().await.clone();
        let project_id = summary.project_id.ok_or_else(|| {
            RuntimeError::InvalidInput("agent is not attached to a project".to_string())
        })?;
        let project = self.project(project_id).await?;
        let project_summary = project.summary.read().await.clone();
        projects::review::validate_project_reviewer_github_api_target(
            &method,
            &path,
            summary.role.as_ref(),
            &project_summary.owner,
            &project_summary.repo,
        )?;
        let review_context = if summary.role == Some(mai_protocol::AgentRole::Reviewer) {
            Some(agent.review_context.read().await.clone().ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "reviewer GitHub API request is missing its review context snapshot"
                        .to_string(),
                )
            })?)
        } else {
            None
        };
        let workspace_volume =
            project_agent_workspace_volume(&project_id.to_string(), &summary.id.to_string());
        let mut env = vec![("GH_TOKEN".to_string(), token.clone())];
        let sidecar_name = format!("mai-tool-gh-api-{}-{}", summary.id, Uuid::new_v4());
        let mut body = projects::review::project_review_github_api_body_with_model_footer(
            &method,
            &path,
            request.body.clone(),
            summary.role.as_ref(),
            &summary.model,
        )?;
        let mut submission_intent = None;
        if let Some(submitted_pr) = projects::review::project_review_submission_pr(&method, &path) {
            let context = review_context.as_ref().ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "only a prepared reviewer may submit a project review".to_string(),
                )
            })?;
            if submitted_pr != context.target.pr {
                return Err(RuntimeError::InvalidInput(format!(
                    "reviewer may submit only its prepared PR #{}",
                    context.target.pr
                )));
            }
            let detail_path = format!(
                "/repos/{}/{}/pulls/{}",
                github::github_path_segment(&project_summary.owner),
                github::github_path_segment(&project_summary.repo),
                context.target.pr
            );
            let detail = github::project_github_api_get_json(
                &self.deps.github_http,
                &self.github_api_base_url,
                Some(token.clone()),
                &detail_path,
            )
            .await?;
            let current_head = detail
                .get("head")
                .and_then(|head| head.get("sha"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|sha| !sha.is_empty())
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "GitHub pull request #{} response is missing head SHA",
                        context.target.pr
                    ))
                })?;
            if current_head != context.target.head_sha {
                context.mark_target_stale();
                return Err(RuntimeError::InvalidInput(format!(
                    "{} for PR #{}: prepared {}, current {}",
                    projects::review::REVIEW_TARGET_HEAD_CHANGED,
                    context.target.pr,
                    context.target.head_sha,
                    current_head
                )));
            }
            let body_object = body
                .as_mut()
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "review submission body must be a JSON object".to_string(),
                    )
                })?;
            body_object.insert(
                "commit_id".to_string(),
                Value::String(context.target.head_sha.clone()),
            );
            append_review_job_marker(body_object, context.run_id)?;
            let intent =
                review_submission_intent(context.run_id, &context.target.head_sha, body_object)?;
            let existing = self
                .deps
                .store
                .load_project_review_job(project_id, context.run_id)
                .await?
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "project review job {} is missing before GitHub submission",
                        context.run_id
                    ))
                })?;
            if let Some(receipt) = existing.submission_receipt.clone() {
                return Ok(ToolExecution::json(
                    serde_json::to_value(receipt).map_err(|error| {
                        RuntimeError::InvalidInput(format!(
                            "failed to serialize GitHub review receipt: {error}"
                        ))
                    })?,
                )?);
            }
            let had_intent = existing.submission_intent.is_some();
            let body_only_fallback_allowed = review_body_only_fallback_is_allowed(
                existing.submission_intent.as_ref(),
                existing.failure.as_ref(),
                &intent,
            );
            let job = self
                .deps
                .store
                .record_project_review_submission_intent(intent.clone())
                .await?;
            if let Some(receipt) = job.submission_receipt {
                return Ok(ToolExecution::json(
                    serde_json::to_value(receipt).map_err(|error| {
                        RuntimeError::InvalidInput(format!(
                            "failed to serialize GitHub review receipt: {error}"
                        ))
                    })?,
                )?);
            }
            if had_intent
                && let Some(receipt) = self
                    .reconcile_project_review_submission(&token, &project_summary, &intent)
                    .await?
            {
                return Ok(ToolExecution::json(
                    serde_json::to_value(receipt).map_err(|error| {
                        RuntimeError::InvalidInput(format!(
                            "failed to serialize GitHub review receipt: {error}"
                        ))
                    })?,
                )?);
            }
            if had_intent && !body_only_fallback_allowed {
                self.deps
                    .store
                    .mark_project_review_submission_reconciling(intent.job_id, now())
                    .await?;
                return Err(RuntimeError::InvalidInput(format!(
                    "GitHub review submission for job {} remains uncertain; reconciliation found no receipt yet",
                    context.run_id
                )));
            }
            submission_intent = Some(intent);
            tracing::info!(
                project_id = %project_id,
                run_id = %context.run_id,
                pr = context.target.pr,
                base_sha = %context.project_revision.base_sha,
                head_sha = %context.target.head_sha,
                "verified project review target before GitHub submission"
            );
        }
        if method == "GET" {
            let value = self
                .github_get_cache
                .get(
                    &self.deps.github_http,
                    &self.github_api_base_url,
                    &token,
                    &path,
                )
                .await?;
            return Ok(ToolExecution::json(select_github_fields(
                value,
                &request.fields,
            ))?);
        }
        let command = if let Some(body) = &body {
            let body = serde_json::to_string(body).map_err(|err| {
                RuntimeError::InvalidInput(format!("invalid GitHub API JSON body: {err}"))
            })?;
            env.push(("MAI_GH_API_BODY".to_string(), body));
            format!(
                "printf '%s' \"$MAI_GH_API_BODY\" | gh api --method {} --input - {}",
                shell_quote_word(&method),
                shell_quote_word(&path)
            )
        } else {
            format!(
                "gh api --method {} {}",
                shell_quote_word(&method),
                shell_quote_word(&path)
            )
        };
        let output = self
            .deps
            .docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: Some(projects::workspace::AGENT_WORKSPACE_REPO_PATH),
                env: &env,
                workspace_volume: Some(&workspace_volume),
                mounts: &[],
                timeout_secs: Some(120),
            })
            .await?;
        if output.status != 0 {
            let message = redact_secret(&format!("{}{}", output.stdout, output.stderr), &token);
            if let Some(intent) = submission_intent.as_ref() {
                if github_inline_position_is_invalid(&message) && intent.comment_count > 0 {
                    self.deps
                        .store
                        .record_project_review_submission_failure(
                            intent.job_id,
                            invalid_inline_position_failure(message.clone()),
                            now(),
                        )
                        .await?;
                } else if github::github_error_message_is_retryable(&message) {
                    self.deps
                        .store
                        .mark_project_review_submission_reconciling(intent.job_id, now())
                        .await?;
                }
            }
            return Err(RuntimeError::InvalidInput(format!(
                "GitHub API {method} {path} failed: {message}"
            )));
        }
        if let Some(intent) = submission_intent {
            let receipt = match review_submission_receipt_from_response(&intent, &output.stdout) {
                Ok(receipt) => receipt,
                Err(error) => {
                    self.deps
                        .store
                        .mark_project_review_submission_reconciling(intent.job_id, now())
                        .await?;
                    return Err(error);
                }
            };
            self.deps
                .store
                .record_project_review_submission_receipt(intent.job_id, receipt)
                .await?;
        }
        Ok(ToolExecution::success(output.stdout))
    }

    pub(crate) async fn reconcile_project_review_submission(
        &self,
        token: &str,
        project: &ProjectSummary,
        intent: &ProjectReviewSubmissionIntent,
    ) -> Result<Option<ProjectReviewSubmissionReceipt>> {
        let job = self
            .deps
            .store
            .load_project_review_job(project.id, intent.job_id)
            .await?
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "project review job {} is missing during reconciliation",
                    intent.job_id
                ))
            })?;
        let marker = review_job_marker(intent.job_id);
        for page in 1..=10 {
            let path = format!(
                "/repos/{}/{}/pulls/{}/reviews?per_page=100&page={page}",
                github::github_path_segment(&project.owner),
                github::github_path_segment(&project.repo),
                job.pr
            );
            let reviews = github::project_github_api_get_json(
                &self.deps.github_http,
                &self.github_api_base_url,
                Some(token.to_string()),
                &path,
            )
            .await?;
            let Some(reviews) = reviews.as_array() else {
                return Err(RuntimeError::InvalidInput(
                    "GitHub reviews response is not an array".to_string(),
                ));
            };
            let receipt = reviews.iter().find_map(|review| {
                let body_matches = review
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| body.contains(&marker));
                let head_matches = review
                    .get("commit_id")
                    .and_then(Value::as_str)
                    .is_some_and(|head| head == intent.head_sha);
                (body_matches && head_matches)
                    .then(|| review_submission_receipt_from_value(intent, review))
            });
            if let Some(receipt) = receipt {
                let receipt = receipt?;
                self.deps
                    .store
                    .record_project_review_submission_receipt(intent.job_id, receipt.clone())
                    .await?;
                return Ok(Some(receipt));
            }
            if reviews.len() < 100 {
                break;
            }
        }
        Ok(None)
    }
}

impl projects::review::eligibility::ProjectReviewEligibilityOps for Arc<AgentRuntime> {
    async fn project_summary(&self, project_id: ProjectId) -> Result<ProjectSummary> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        Ok(project.summary.read().await.clone())
    }

    async fn project_review_identity(
        &self,
        project_id: ProjectId,
    ) -> Result<projects::review::selector::ProjectReviewIdentity> {
        let project = AgentRuntime::project(self.as_ref(), project_id).await?;
        let summary = project.summary.read().await.clone();
        let account_id = summary.git_account_id.ok_or_else(|| {
            RuntimeError::InvalidInput("project git account is not configured".to_string())
        })?;
        let account = self.deps.git_accounts.summary(&account_id).await?;
        let login = match account.provider {
            GitProvider::Github => match normalized_text(account.login) {
                Some(login) => login,
                None => {
                    let value = github::project_github_api_get_json(
                        &self.deps.github_http,
                        &self.github_api_base_url,
                        self.project_git_token(project_id).await?,
                        "/user",
                    )
                    .await?;
                    value
                        .get("login")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| {
                            RuntimeError::InvalidInput(
                                "GitHub /user response missing login".to_string(),
                            )
                        })?
                }
            },
            GitProvider::GithubAppRelay => {
                let settings = self.deps.github_backend.github_app_settings().await?;
                normalized_text(settings.app_slug)
                    .map(|slug| format!("{slug}[bot]"))
                    .or_else(|| normalized_text(account.login))
                    .or_else(|| normalized_text(account.installation_account))
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "GitHub App reviewer identity is not configured".to_string(),
                        )
                    })?
            }
        };
        Ok(projects::review::selector::ProjectReviewIdentity { login })
    }

    async fn github_api_get_json(&self, project_id: ProjectId, path: String) -> Result<Value> {
        github::project_github_api_get_json(
            &self.deps.github_http,
            &self.github_api_base_url,
            self.project_git_token(project_id).await?,
            &path,
        )
        .await
    }
}

fn append_review_job_marker(body: &mut serde_json::Map<String, Value>, job_id: Uuid) -> Result<()> {
    let review_body = body
        .get_mut("body")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "review submission body must contain a text body".to_string(),
            )
        })?
        .to_string();
    let marker = review_job_marker(job_id);
    if !review_body.contains(&marker) {
        body.insert(
            "body".to_string(),
            Value::String(format!("{}\n\n{marker}", review_body.trim_end())),
        );
    }
    Ok(())
}

fn review_job_marker(job_id: Uuid) -> String {
    format!("<!-- mai-review-job:{job_id} -->")
}

fn review_body_only_fallback_is_allowed(
    previous: Option<&ProjectReviewSubmissionIntent>,
    failure: Option<&ProjectReviewFailure>,
    next: &ProjectReviewSubmissionIntent,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    previous.job_id == next.job_id
        && previous.head_sha == next.head_sha
        && previous.event == next.event
        && previous.body_hash == next.body_hash
        && previous.comment_count > 0
        && next.comment_count == 0
        && failure
            .and_then(|failure| failure.code.as_deref())
            .is_some_and(|code| code == INVALID_INLINE_POSITION_CODE)
}

fn github_inline_position_is_invalid(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("line could not be resolved")
}

fn invalid_inline_position_failure(message: String) -> ProjectReviewFailure {
    ProjectReviewFailure {
        category: ProjectReviewFailureCategory::Github,
        code: Some(INVALID_INLINE_POSITION_CODE.to_string()),
        http_status: Some(422),
        message,
        retry: pl_protocol::RetryDisposition::Permanent,
    }
}

fn review_submission_intent(
    job_id: Uuid,
    head_sha: &str,
    body: &serde_json::Map<String, Value>,
) -> Result<ProjectReviewSubmissionIntent> {
    let event = match body.get("event").and_then(Value::as_str) {
        Some("APPROVE") => ProjectReviewDecision::Approve,
        Some("REQUEST_CHANGES") => ProjectReviewDecision::RequestChanges,
        Some("COMMENT") => ProjectReviewDecision::Comment,
        Some(event) => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported review event `{event}`"
            )));
        }
        None => {
            return Err(RuntimeError::InvalidInput(
                "review submission is missing event".to_string(),
            ));
        }
    };
    let review_body = body.get("body").and_then(Value::as_str).unwrap_or_default();
    let comment_count = body
        .get("comments")
        .and_then(Value::as_array)
        .map_or(0, |comments| comments.len() as u64);
    Ok(ProjectReviewSubmissionIntent {
        job_id,
        head_sha: head_sha.to_string(),
        event,
        body_hash: pl_core::canonical_content_hash(review_body.as_bytes()),
        comment_count,
        created_at: now(),
    })
}

fn review_submission_receipt_from_response(
    intent: &ProjectReviewSubmissionIntent,
    response: &str,
) -> Result<ProjectReviewSubmissionReceipt> {
    let value = serde_json::from_str::<Value>(response).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "GitHub review submission returned invalid JSON: {error}"
        ))
    })?;
    review_submission_receipt_from_value(intent, &value)
}

fn review_submission_receipt_from_value(
    intent: &ProjectReviewSubmissionIntent,
    value: &Value,
) -> Result<ProjectReviewSubmissionReceipt> {
    let github_review_id = value.get("id").and_then(Value::as_u64).ok_or_else(|| {
        RuntimeError::InvalidInput(
            "GitHub review submission response is missing review id".to_string(),
        )
    })?;
    let head_sha = value
        .get("commit_id")
        .and_then(Value::as_str)
        .unwrap_or(&intent.head_sha)
        .to_string();
    if head_sha != intent.head_sha {
        return Err(RuntimeError::InvalidInput(format!(
            "GitHub review receipt head `{head_sha}` does not match intended head `{}`",
            intent.head_sha
        )));
    }
    let submitted_at = value
        .get("submitted_at")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(now);
    Ok(ProjectReviewSubmissionReceipt {
        github_review_id,
        event: intent.event.clone(),
        head_sha,
        html_url: value
            .get("html_url")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        submitted_at,
    })
}

fn select_github_fields(value: Value, requested: &[String]) -> Value {
    if requested.is_empty() {
        return value;
    }
    const ALWAYS_RETAINED: [&str; 9] = [
        "message",
        "documentation_url",
        "status",
        "url",
        "total_count",
        "incomplete_results",
        "has_next_page",
        "next",
        "last",
    ];
    let requested = requested
        .iter()
        .map(|field| field.trim())
        .filter(|field| !field.is_empty())
        .take(32)
        .collect::<std::collections::BTreeSet<_>>();
    let select_object = |map: serde_json::Map<String, Value>| {
        Value::Object(
            map.into_iter()
                .filter(|(key, _)| {
                    requested.contains(key.as_str()) || ALWAYS_RETAINED.contains(&key.as_str())
                })
                .collect(),
        )
    };
    match value {
        Value::Object(map) => select_object(map),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| match item {
                    Value::Object(map) => select_object(map),
                    Value::Array(_)
                    | Value::String(_)
                    | Value::Number(_)
                    | Value::Bool(_)
                    | Value::Null => item,
                })
                .collect(),
        ),
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => value,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use mai_protocol::{
        ProjectReviewDecision, ProjectReviewFailure, ProjectReviewFailureCategory,
        ProjectReviewSubmissionIntent,
    };
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        INVALID_INLINE_POSITION_CODE, github_inline_position_is_invalid,
        review_body_only_fallback_is_allowed, select_github_fields,
    };

    #[test]
    fn github_field_projection_keeps_requested_and_protocol_metadata() {
        let projected = select_github_fields(
            json!({
                "total_count": 2,
                "check_runs": [{"name": "test", "status": "completed"}],
                "large_unused": [1, 2, 3],
            }),
            &["check_runs".to_string()],
        );

        assert_eq!(
            projected,
            json!({
                "total_count": 2,
                "check_runs": [{"name": "test", "status": "completed"}],
            })
        );
    }

    #[test]
    fn unresolved_submission_never_allows_a_second_full_post() {
        let previous = submission_intent(2);
        let repeated = previous.clone();

        assert!(!review_body_only_fallback_is_allowed(
            Some(&previous),
            None,
            &repeated
        ));
    }

    #[test]
    fn body_only_retry_requires_persisted_invalid_inline_failure() {
        let previous = submission_intent(2);
        let mut body_only = previous.clone();
        body_only.comment_count = 0;
        let failure = ProjectReviewFailure {
            category: ProjectReviewFailureCategory::Github,
            code: Some(INVALID_INLINE_POSITION_CODE.to_string()),
            http_status: Some(422),
            message: "Line could not be resolved".to_string(),
            retry: pl_protocol::RetryDisposition::Permanent,
        };

        assert!(!review_body_only_fallback_is_allowed(
            Some(&previous),
            None,
            &body_only
        ));
        assert!(review_body_only_fallback_is_allowed(
            Some(&previous),
            Some(&failure),
            &body_only
        ));
        assert!(github_inline_position_is_invalid(
            "HTTP 422: Line could not be resolved"
        ));
    }

    fn submission_intent(comment_count: u64) -> ProjectReviewSubmissionIntent {
        ProjectReviewSubmissionIntent {
            job_id: Uuid::new_v4(),
            head_sha: "head".to_string(),
            event: ProjectReviewDecision::RequestChanges,
            body_hash: "hash".to_string(),
            comment_count,
            created_at: Utc::now(),
        }
    }
}
