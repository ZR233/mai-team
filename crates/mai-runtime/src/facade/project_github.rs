use std::sync::Arc;

use mai_docker::{SidecarParams, project_agent_workspace_volume};
use mai_protocol::{GitProvider, ProjectId, ProjectSummary};
use pl_core::shell_quote_word;
use serde_json::Value;
use uuid::Uuid;

use crate::github::GitAccountToken;
use crate::state::AgentRecord;
use crate::turn::tool_output::ToolExecution;
use crate::{
    AgentRuntime, Result, RuntimeError, github, normalized_text, projects, redact_secret, turn,
};

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
            return Err(RuntimeError::InvalidInput(format!(
                "GitHub API {method} {path} failed: {message}"
            )));
        }
        Ok(ToolExecution::success(output.stdout))
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
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::select_github_fields;

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
}
