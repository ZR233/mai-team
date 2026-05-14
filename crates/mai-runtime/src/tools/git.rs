use std::path::PathBuf;

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
    let worktree = projects::workspace::agent_worktree_path(
        context.projects_root,
        context.project.id,
        context.agent_id,
    );
    if !worktree.exists() {
        return Err(RuntimeError::InvalidInput(
            "project git worktree is not available".to_string(),
        ));
    }
    let output = match name {
        mai_tools::TOOL_GIT_STATUS => {
            git_plain(context.git_binary, &worktree, ["status", "--short", "--branch"]).await?
        }
        mai_tools::TOOL_GIT_DIFF => git_diff(context.git_binary, &worktree, &arguments).await?,
        mai_tools::TOOL_GIT_BRANCH => git_branch(context.git_binary, &worktree, &arguments).await?,
        mai_tools::TOOL_GIT_FETCH => {
            let token = required_token(context.token.as_deref())?;
            git_fetch(context.git_binary, &worktree, token, &arguments).await?
        }
        mai_tools::TOOL_GIT_COMMIT => git_commit(context.git_binary, &worktree, &arguments).await?,
        mai_tools::TOOL_GIT_PUSH => {
            let token = required_token(context.token.as_deref())?;
            git_push(context.git_binary, &worktree, token, &arguments).await?
        }
        mai_tools::TOOL_GIT_WORKTREE_INFO => {
            let repo = projects::workspace::project_repo_path(context.projects_root, context.project.id);
            json!({
                "project_id": context.project.id,
                "repo": repo,
                "worktree": worktree,
            })
            .to_string()
        }
        mai_tools::TOOL_GIT_SYNC_DEFAULT_BRANCH => {
            let token = required_token(context.token.as_deref())?;
            projects::workspace::sync_project_repo(
                context.git_binary,
                context.projects_root,
                &context.project,
                token,
            )
            .await?;
            let refreshed = projects::workspace::prepare_project_agent_worktree(
                context.git_binary,
                context.projects_root,
                &context.project,
                context.agent_id,
            )
            .await?;
            json!({ "worktree": refreshed }).to_string()
        }
        _ => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported git tool `{name}`"
            )));
        }
    };
    Ok(ToolExecution::new(true, output, false))
}

async fn git_diff(git_binary: &str, cwd: &PathBuf, arguments: &Value) -> Result<String> {
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

async fn git_branch(git_binary: &str, cwd: &PathBuf, arguments: &Value) -> Result<String> {
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

async fn git_fetch(git_binary: &str, cwd: &PathBuf, token: &str, arguments: &Value) -> Result<String> {
    let remote = optional_arg(arguments, "remote")?.unwrap_or_else(|| "origin".to_string());
    let prune = arguments.get("prune").and_then(Value::as_bool).unwrap_or(true);
    if let Some(refspec) = optional_arg(arguments, "refspec")? {
        if prune {
            git_with_token(git_binary, cwd, token, ["fetch", "--prune", &remote, &refspec]).await
        } else {
            git_with_token(git_binary, cwd, token, ["fetch", &remote, &refspec]).await
        }
    } else if prune {
        git_with_token(git_binary, cwd, token, ["fetch", "--prune", &remote]).await
    } else {
        git_with_token(git_binary, cwd, token, ["fetch", &remote]).await
    }
}

async fn git_commit(git_binary: &str, cwd: &PathBuf, arguments: &Value) -> Result<String> {
    let message = required_arg(arguments, "message")?;
    if arguments.get("all").and_then(Value::as_bool).unwrap_or(false) {
        git_plain(git_binary, cwd, ["commit", "-am", &message]).await
    } else {
        git_plain(git_binary, cwd, ["commit", "-m", &message]).await
    }
}

async fn git_push(git_binary: &str, cwd: &PathBuf, token: &str, arguments: &Value) -> Result<String> {
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
        (false, Some(branch)) => git_with_token(git_binary, cwd, token, ["push", &remote, branch]).await,
        (true, None) => git_with_token(git_binary, cwd, token, ["push", "-u", &remote, "HEAD"]).await,
        (false, None) => git_with_token(git_binary, cwd, token, ["push"]).await,
    }
}

async fn git_plain<const N: usize>(git_binary: &str, cwd: &PathBuf, args: [&str; N]) -> Result<String> {
    projects::workspace::git_plain(git_binary, cwd, args).await
}

async fn git_with_token<const N: usize>(
    git_binary: &str,
    cwd: &PathBuf,
    token: &str,
    args: [&str; N],
) -> Result<String> {
    projects::workspace::git_with_token(git_binary, cwd, token, args)
        .await
        .map(|output| output.replace(token, "[redacted]"))
}

fn required_token(token: Option<&str>) -> Result<&str> {
    token.filter(|token| !token.trim().is_empty()).ok_or_else(|| {
        RuntimeError::InvalidInput("project git account token is not configured".to_string())
    })
}

fn required_arg(arguments: &Value, name: &str) -> Result<String> {
    optional_arg(arguments, name)?.ok_or_else(|| {
        RuntimeError::InvalidInput(format!("missing string field `{name}`"))
    })
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
