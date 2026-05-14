use std::path::Path;

use mai_protocol::{AgentId, ProjectSummary};
use serde_json::{Value, json};

use crate::projects;
use crate::turn::tools::ToolExecution;
use crate::{Result, RuntimeError};

pub(crate) struct GitToolContext<'a> {
    pub(crate) git_binary: &'a str,
    pub(crate) projects_root: &'a std::path::Path,
    pub(crate) agent_id: AgentId,
    pub(crate) project: ProjectSummary,
    pub(crate) token: Option<String>,
}

pub(crate) async fn execute_git_tool(
    context: GitToolContext<'_>,
    name: &str,
    arguments: Value,
) -> Result<ToolExecution> {
    let clone = projects::workspace::agent_clone_path(
        context.projects_root,
        context.project.id,
        context.agent_id,
    );
    if !clone.exists() {
        return Err(RuntimeError::InvalidInput(
            "project git workspace is not available".to_string(),
        ));
    }
    let output = match name {
        mai_tools::TOOL_GIT_STATUS => {
            git_plain(
                context.git_binary,
                &clone,
                ["status", "--short", "--branch"],
            )
            .await?
        }
        mai_tools::TOOL_GIT_DIFF => git_diff(context.git_binary, &clone, &arguments).await?,
        mai_tools::TOOL_GIT_BRANCH => git_branch(context.git_binary, &clone, &arguments).await?,
        mai_tools::TOOL_GIT_FETCH => {
            let token = required_token(context.token.as_deref())?;
            git_fetch(context.git_binary, &clone, token, &arguments).await?
        }
        mai_tools::TOOL_GIT_COMMIT => git_commit(context.git_binary, &clone, &arguments).await?,
        mai_tools::TOOL_GIT_PUSH => {
            let token = required_token(context.token.as_deref())?;
            git_push(context.git_binary, &clone, token, &arguments).await?
        }
        mai_tools::TOOL_GIT_WORKTREE_INFO => {
            let repo_cache = projects::workspace::project_repo_cache_path(
                context.projects_root,
                context.project.id,
            );
            json!({
                "project_id": context.project.id,
                "repo_cache": repo_cache,
                "clone": clone,
                "worktree": clone,
            })
            .to_string()
        }
        mai_tools::TOOL_GIT_SYNC_DEFAULT_BRANCH => {
            let token = required_token(context.token.as_deref())?;
            projects::workspace::sync_project_repo_cache(
                context.git_binary,
                context.projects_root,
                &context.project,
                token,
            )
            .await?;
            let refreshed = projects::workspace::prepare_project_agent_clone(
                context.git_binary,
                context.projects_root,
                &context.project,
                context.agent_id,
            )
            .await?;
            json!({ "clone": refreshed, "worktree": refreshed }).to_string()
        }
        _ => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported git tool `{name}`"
            )));
        }
    };
    Ok(ToolExecution::new(true, output, false))
}

async fn git_diff(git_binary: &str, cwd: &Path, arguments: &Value) -> Result<String> {
    let staged = arguments
        .get("staged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = optional_arg(arguments, "path")?;
    match (staged, path.as_deref()) {
        (true, Some(path)) => git_plain(git_binary, cwd, ["diff", "--staged", "--", path]).await,
        (true, None) => git_plain(git_binary, cwd, ["diff", "--staged"]).await,
        (false, Some(path)) => git_plain(git_binary, cwd, ["diff", "--", path]).await,
        (false, None) => git_plain(git_binary, cwd, ["diff"]).await,
    }
}

async fn git_branch(git_binary: &str, cwd: &Path, arguments: &Value) -> Result<String> {
    let action = optional_arg(arguments, "action")?.unwrap_or_else(|| "list".to_string());
    match action.as_str() {
        "list" => git_plain(git_binary, cwd, ["branch", "--list", "--all"]).await,
        "switch" => {
            let name = required_arg(arguments, "name")?;
            git_plain(git_binary, cwd, ["switch", &name]).await
        }
        "create" => {
            let name = required_arg(arguments, "name")?;
            if let Some(start_point) = optional_arg(arguments, "start_point")? {
                git_plain(git_binary, cwd, ["switch", "-c", &name, &start_point]).await
            } else {
                git_plain(git_binary, cwd, ["switch", "-c", &name]).await
            }
        }
        other => Err(RuntimeError::InvalidInput(format!(
            "unsupported git branch action `{other}`"
        ))),
    }
}

async fn git_fetch(git_binary: &str, cwd: &Path, token: &str, arguments: &Value) -> Result<String> {
    let remote = optional_arg(arguments, "remote")?.unwrap_or_else(|| "origin".to_string());
    let prune = arguments
        .get("prune")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if let Some(refspec) = optional_arg(arguments, "refspec")? {
        if prune {
            git_with_token(
                git_binary,
                cwd,
                token,
                ["fetch", "--prune", &remote, &refspec],
            )
            .await
        } else {
            git_with_token(git_binary, cwd, token, ["fetch", &remote, &refspec]).await
        }
    } else if prune {
        git_with_token(git_binary, cwd, token, ["fetch", "--prune", &remote]).await
    } else {
        git_with_token(git_binary, cwd, token, ["fetch", &remote]).await
    }
}

async fn git_commit(git_binary: &str, cwd: &Path, arguments: &Value) -> Result<String> {
    let message = required_arg(arguments, "message")?;
    if arguments
        .get("all")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        git_plain(git_binary, cwd, ["commit", "-am", &message]).await
    } else {
        git_plain(git_binary, cwd, ["commit", "-m", &message]).await
    }
}

async fn git_push(git_binary: &str, cwd: &Path, token: &str, arguments: &Value) -> Result<String> {
    let remote = optional_arg(arguments, "remote")?.unwrap_or_else(|| "origin".to_string());
    let branch = optional_arg(arguments, "branch")?;
    let set_upstream = arguments
        .get("set_upstream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match (set_upstream, branch.as_deref()) {
        (true, Some(branch)) => {
            git_with_token(git_binary, cwd, token, ["push", "-u", &remote, branch]).await
        }
        (false, Some(branch)) => {
            git_with_token(git_binary, cwd, token, ["push", &remote, branch]).await
        }
        (true, None) => {
            git_with_token(git_binary, cwd, token, ["push", "-u", &remote, "HEAD"]).await
        }
        (false, None) => git_with_token(git_binary, cwd, token, ["push"]).await,
    }
}

async fn git_plain<const N: usize>(
    git_binary: &str,
    cwd: &Path,
    args: [&str; N],
) -> Result<String> {
    projects::workspace::git_plain(git_binary, cwd, args).await
}

async fn git_with_token<const N: usize>(
    git_binary: &str,
    cwd: &Path,
    token: &str,
    args: [&str; N],
) -> Result<String> {
    projects::workspace::git_with_token(git_binary, cwd, token, args)
        .await
        .map(|output| output.replace(token, "[redacted]"))
}

fn required_token(token: Option<&str>) -> Result<&str> {
    token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidInput("project git account token is not configured".to_string())
        })
}

fn required_arg(arguments: &Value, name: &str) -> Result<String> {
    optional_arg(arguments, name)?
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing string field `{name}`")))
}

fn optional_arg(arguments: &Value, name: &str) -> Result<Option<String>> {
    Ok(arguments
        .get(name)
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                RuntimeError::InvalidInput(format!("field `{name}` must be a string"))
            })
        })
        .transpose()?
        .filter(|value| !value.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::Utc;
    use mai_protocol::{ProjectCloneStatus, ProjectStatus, ProjectSummary};
    use pretty_assertions::assert_eq;
    use serde_json::Value;

    use super::*;
    use crate::projects::workspace;

    #[tokio::test]
    async fn git_status_runs_inside_agent_clone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        execute_git_tool(
            GitToolContext {
                git_binary: &git,
                projects_root: dir.path(),
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            mai_tools::TOOL_GIT_STATUS,
            json!({}),
        )
        .await
        .expect("execute git status");

        assert_eq!(
            read_git_log(dir.path()),
            format!("{}|status --short --branch\n", clone_path.to_string_lossy())
        );
    }

    #[tokio::test]
    async fn git_worktree_info_returns_clone_oriented_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        let execution = execute_git_tool(
            GitToolContext {
                git_binary: &git,
                projects_root: dir.path(),
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            mai_tools::TOOL_GIT_WORKTREE_INFO,
            json!({}),
        )
        .await
        .expect("execute info");

        let payload: Value = serde_json::from_str(&execution.output).expect("json payload");
        assert_eq!(payload["project_id"], json!(project_id));
        assert_eq!(
            payload["repo_cache"],
            json!(workspace::paths::project_repo_cache_path(
                dir.path(),
                project_id
            ))
        );
        assert_eq!(payload["clone"], json!(clone_path));
    }

    fn test_project(project_id: uuid::Uuid, agent_id: uuid::Uuid) -> ProjectSummary {
        let timestamp = Utc::now();
        ProjectSummary {
            id: project_id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account-1".to_string()),
            repository_id: 1,
            installation_id: 2,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "image".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: agent_id,
            created_at: timestamp,
            updated_at: timestamp,
            last_error: None,
            auto_review_enabled: false,
            reviewer_extra_prompt: None,
            review_status: Default::default(),
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: None,
            review_last_error: None,
        }
    }

    fn fake_git_path(root: &Path) -> String {
        let path = root.join("fake-git.sh");
        let log_path = git_log_path(root);
        let script = format!(
            r#"#!/bin/sh
LOG={}
printf '%s|%s\n' "$PWD" "$*" >> "$LOG"
exit 0
"#,
            shell_quote(&log_path)
        );
        std::fs::write(&path, script).expect("fake git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod");
        }
        path.to_string_lossy().to_string()
    }

    fn read_git_log(root: &Path) -> String {
        std::fs::read_to_string(git_log_path(root)).unwrap_or_default()
    }

    fn git_log_path(root: &Path) -> PathBuf {
        root.join("git.log")
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
}
