use chrono::{DateTime, Utc};
use mai_docker::{DockerClient, SidecarParams, project_review_workspace_volume};
use mai_protocol::{AgentId, ProjectId, ProjectSummary, preview};
use tokio::time::{Duration, sleep};

use crate::github::github_clone_url;
use crate::{Result, RuntimeError};

use super::{ReviewRepoAction, retention_cleanup_spec, review_repo_command_spec};

const PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS: usize = 3;
const PROJECT_REVIEW_REPO_COMMAND_RETRY_SECS: u64 = 5;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ReviewRepoCommand {
    Ensure,
    Sync,
}

pub(crate) async fn cleanup_history(
    docker: &DockerClient,
    sidecar_image: &str,
    project_id: ProjectId,
    active_reviewer: Option<AgentId>,
    cutoff: DateTime<Utc>,
) -> Result<()> {
    let volume = project_review_workspace_volume(&project_id.to_string());
    let spec = retention_cleanup_spec(project_id, active_reviewer, cutoff.timestamp());
    let output = docker
        .run_sidecar_shell_env(&SidecarParams {
            name: &spec.sidecar_name,
            image: sidecar_image,
            command: &spec.command,
            args: &[],
            cwd: Some(spec.cwd),
            env: &[],
            workspace_volume: Some(&volume),
            timeout_secs: Some(spec.timeout_secs),
        })
        .await?;
    if output.status != 0 {
        return Err(RuntimeError::InvalidInput(format!(
            "review retention cleanup failed: {}",
            preview(format!("{}\n{}", output.stderr, output.stdout).trim(), 500)
        )));
    }
    Ok(())
}

pub(crate) async fn cleanup_worktree(
    docker: &DockerClient,
    sidecar_image: &str,
    project_id: ProjectId,
    reviewer_id: AgentId,
) -> Result<()> {
    let volume = project_review_workspace_volume(&project_id.to_string());
    let spec = super::reviewer_worktree_cleanup_spec(reviewer_id);
    let output = docker
        .run_sidecar_shell_env(&SidecarParams {
            name: &spec.sidecar_name,
            image: sidecar_image,
            command: &spec.command,
            args: &[],
            cwd: Some(spec.cwd),
            env: &[],
            workspace_volume: Some(&volume),
            timeout_secs: Some(spec.timeout_secs),
        })
        .await?;
    if output.status != 0 {
        return Err(RuntimeError::InvalidInput(format!(
            "review workspace cleanup failed: {}",
            preview(format!("{}\n{}", output.stderr, output.stdout).trim(), 500)
        )));
    }
    Ok(())
}

pub(crate) async fn run_repo_command(
    docker: &DockerClient,
    sidecar_image: &str,
    project: &ProjectSummary,
    token: &str,
    command: ReviewRepoCommand,
) -> Result<()> {
    let volume = project_review_workspace_volume(&project.id.to_string());
    let repo_url = github_clone_url(&project.owner, &project.repo);
    let expected_remote = repo_url.clone();
    let branch = if project.branch.trim().is_empty() {
        "main".to_string()
    } else {
        project.branch.clone()
    };
    let action = match command {
        ReviewRepoCommand::Ensure => ReviewRepoAction::Ensure,
        ReviewRepoCommand::Sync => ReviewRepoAction::Sync,
    };
    let spec = review_repo_command_spec(action, &repo_url, &expected_remote, &branch);
    let env = [("MAI_GITHUB_REVIEW_TOKEN".to_string(), token.to_string())];
    let mut last_message = String::new();
    for attempt in 1..=PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS {
        let sidecar_name = format!("mai-review-sync-{}-{attempt}", project.id);
        let output = docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: sidecar_image,
                command: &spec.command,
                args: &[],
                cwd: Some("/workspace"),
                env: &env,
                workspace_volume: Some(&volume),
                timeout_secs: Some(900),
            })
            .await?;
        if output.status == 0 {
            return Ok(());
        }
        let combined = format!("{}\n{}", output.stderr, output.stdout);
        last_message = preview(redact_secret(combined.trim(), token).trim(), 500);
        if attempt < PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS {
            tracing::warn!(
                project_id = %project.id,
                command = spec.label,
                attempt,
                attempts = PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS,
                error = %last_message,
                "project review workspace command failed; retrying"
            );
            sleep(Duration::from_secs(PROJECT_REVIEW_REPO_COMMAND_RETRY_SECS)).await;
        }
    }
    if let Some(fallback_command_text) = spec.fallback_command {
        tracing::warn!(
            project_id = %project.id,
            command = spec.label,
            error = %last_message,
            "project review workspace sync failed; recreating review repository"
        );
        let sidecar_name = format!("mai-review-sync-{}-reclone", project.id);
        let output = docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: sidecar_image,
                command: &fallback_command_text,
                args: &[],
                cwd: Some("/workspace"),
                env: &env,
                workspace_volume: Some(&volume),
                timeout_secs: Some(900),
            })
            .await?;
        if output.status == 0 {
            return Ok(());
        }
        let combined = format!("{}\n{}", output.stderr, output.stdout);
        last_message = preview(redact_secret(combined.trim(), token).trim(), 500);
    }
    Err(RuntimeError::InvalidInput(format!(
        "project review workspace {} failed after {} attempts: {last_message}",
        spec.label, PROJECT_REVIEW_REPO_COMMAND_MAX_ATTEMPTS
    )))
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}
