use std::sync::Arc;

use mai_docker::{SidecarParams, project_agent_workspace_volume};
use mai_protocol::{GitProvider, ProjectId, ProjectSummary};
use serde_json::Value;
use uuid::Uuid;

use crate::github::GitAccountToken;
use crate::state::AgentRecord;
use crate::turn::tool_output::ToolExecution;
use crate::{
    AgentRuntime, Result, RuntimeError, github, normalized_text, projects, redact_secret,
    shell_quote, turn,
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
        let workspace_volume =
            project_agent_workspace_volume(&project_id.to_string(), &summary.id.to_string());
        let mut env = vec![("GH_TOKEN".to_string(), token.clone())];
        let sidecar_name = format!("mai-tool-gh-api-{}-{}", summary.id, Uuid::new_v4());
        let body = projects::review::project_review_github_api_body_with_model_footer(
            &method,
            &path,
            request.body.clone(),
            summary.role.as_ref(),
            &summary.model,
        )?;
        let command = if let Some(body) = &body {
            let body = serde_json::to_string(body).map_err(|err| {
                RuntimeError::InvalidInput(format!("invalid GitHub API JSON body: {err}"))
            })?;
            env.push(("MAI_GH_API_BODY".to_string(), body));
            format!(
                "printf '%s' \"$MAI_GH_API_BODY\" | gh api --method {} --input - {}",
                shell_quote(&method),
                shell_quote(&path)
            )
        } else {
            format!(
                "gh api --method {} {}",
                shell_quote(&method),
                shell_quote(&path)
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
                "gh api sidecar failed: {message}"
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
