use std::path::Path;

use mai_protocol::{AgentId, ProjectSummary};
use serde_json::{Value, json};

use crate::github::github_clone_url;
use crate::projects;
use crate::projects::workspace::policy::GitPolicy;
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
    let policy = GitPolicy::new(context.project.branch.clone());
    let output = match name {
        mai_tools::TOOL_GIT_STATUS => {
            git_plain(
                context.git_binary,
                &clone,
                ["status", "--short", "--branch"],
            )
            .await?
        }
        mai_tools::TOOL_GIT_DIFF => {
            git_diff(context.git_binary, &clone, &arguments, &policy).await?
        }
        mai_tools::TOOL_GIT_BRANCH => {
            git_branch(context.git_binary, &clone, &arguments, &policy).await?
        }
        mai_tools::TOOL_GIT_FETCH => {
            let token = required_token(context.token.as_deref())?;
            git_fetch(context.git_binary, &clone, token, &arguments, &policy).await?
        }
        mai_tools::TOOL_GIT_COMMIT => git_commit(context.git_binary, &clone, &arguments).await?,
        mai_tools::TOOL_GIT_PUSH => {
            let token = required_token(context.token.as_deref())?;
            git_push(
                context.git_binary,
                &clone,
                token,
                &arguments,
                &policy,
                context.agent_id,
            )
            .await?
        }
        mai_tools::TOOL_GIT_WORKTREE_INFO | mai_tools::TOOL_GIT_WORKSPACE_INFO => {
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
            git_sync_default_branch(
                context.git_binary,
                context.projects_root,
                &clone,
                &context.project,
                context.agent_id,
                token,
                &arguments,
                &policy,
            )
            .await?
        }
        _ => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported git tool `{name}`"
            )));
        }
    };
    Ok(ToolExecution::new(true, output, false))
}

async fn git_diff(
    git_binary: &str,
    cwd: &Path,
    arguments: &Value,
    policy: &GitPolicy,
) -> Result<String> {
    let staged = arguments
        .get("staged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = optional_arg(arguments, "path")?;
    if let Some(path) = path.as_deref() {
        policy.validate_path(path)?;
    }
    match (staged, path.as_deref()) {
        (true, Some(path)) => git_plain(git_binary, cwd, ["diff", "--staged", "--", path]).await,
        (true, None) => git_plain(git_binary, cwd, ["diff", "--staged"]).await,
        (false, Some(path)) => git_plain(git_binary, cwd, ["diff", "--", path]).await,
        (false, None) => git_plain(git_binary, cwd, ["diff"]).await,
    }
}

async fn git_branch(
    git_binary: &str,
    cwd: &Path,
    arguments: &Value,
    policy: &GitPolicy,
) -> Result<String> {
    let action = optional_arg(arguments, "action")?.unwrap_or_else(|| "list".to_string());
    match action.as_str() {
        "list" => git_plain(git_binary, cwd, ["branch", "--list", "--all"]).await,
        "switch" => {
            let name = required_arg(arguments, "name")?;
            policy.validate_branch(&name)?;
            git_plain(git_binary, cwd, ["switch", &name]).await
        }
        "create" => {
            let name = required_arg(arguments, "name")?;
            policy.validate_branch(&name)?;
            if let Some(start_point) = optional_arg(arguments, "start_point")? {
                policy.validate_branch(&start_point)?;
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

async fn git_fetch(
    git_binary: &str,
    cwd: &Path,
    token: &str,
    arguments: &Value,
    policy: &GitPolicy,
) -> Result<String> {
    let remote = optional_arg(arguments, "remote")?.unwrap_or_else(|| "origin".to_string());
    policy.validate_remote(&remote)?;
    let prune = arguments
        .get("prune")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let refspec = optional_arg(arguments, "refspec")?;
    policy.validate_fetch_refspec(refspec.as_deref(), &policy.default_branch)?;
    if let Some(refspec) = refspec {
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
        git_plain(git_binary, cwd, ["commit", "--no-verify", "-am", &message]).await
    } else {
        git_plain(git_binary, cwd, ["commit", "--no-verify", "-m", &message]).await
    }
}

async fn git_push(
    git_binary: &str,
    cwd: &Path,
    token: &str,
    arguments: &Value,
    policy: &GitPolicy,
    agent_id: AgentId,
) -> Result<String> {
    let remote = optional_arg(arguments, "remote")?.unwrap_or_else(|| "origin".to_string());
    policy.validate_remote(&remote)?;
    let branch = optional_arg(arguments, "branch")?;
    if let Some(branch) = branch.as_deref() {
        policy.validate_branch(branch)?;
    }
    let set_upstream = arguments
        .get("set_upstream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let branch = branch.unwrap_or_else(|| format!("{}{agent_id}", policy.agent_branch_prefix));
    let destination = format!("HEAD:refs/heads/{branch}");
    if set_upstream {
        git_with_token(
            git_binary,
            cwd,
            token,
            ["push", "--no-verify", "-u", &remote, &destination],
        )
        .await
    } else {
        git_with_token(
            git_binary,
            cwd,
            token,
            ["push", "--no-verify", &remote, &destination],
        )
        .await
    }
}

async fn git_sync_default_branch(
    git_binary: &str,
    projects_root: &Path,
    clone: &Path,
    project: &ProjectSummary,
    agent_id: AgentId,
    token: &str,
    arguments: &Value,
    policy: &GitPolicy,
) -> Result<String> {
    let force = arguments
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let preserve_changes = arguments
        .get("preserve_changes")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if force && preserve_changes {
        return Err(RuntimeError::InvalidInput(
            "force and preserve_changes cannot both be true".to_string(),
        ));
    }

    let status = git_plain(git_binary, clone, ["status", "--porcelain"]).await?;
    let dirty = !status.trim().is_empty();
    if dirty && !force && !preserve_changes {
        return Err(RuntimeError::InvalidInput(
            "project git workspace has uncommitted changes; pass force=true to discard them or preserve_changes=true to stash them before sync".to_string(),
        ));
    }
    if dirty && preserve_changes {
        git_plain(
            git_binary,
            clone,
            ["stash", "push", "-u", "-m", "mai sync default branch"],
        )
        .await?;
    }

    projects::workspace::sync_project_repo_cache(git_binary, projects_root, project, token).await?;
    let repo_url = github_clone_url(&project.owner, &project.repo);
    git_plain(
        git_binary,
        clone,
        ["remote", "set-url", "origin", &repo_url],
    )
    .await?;
    git_with_token(git_binary, clone, token, ["fetch", "--prune", "origin"]).await?;
    let branch = format!("{}{agent_id}", policy.agent_branch_prefix);
    let origin_branch = format!("origin/{}", project.branch);
    git_plain(
        git_binary,
        clone,
        ["checkout", "-B", &branch, &origin_branch],
    )
    .await?;
    git_plain(git_binary, clone, ["reset", "--hard", &origin_branch]).await?;
    if force {
        git_plain(git_binary, clone, ["clean", "-fdx"]).await?;
    }
    if dirty && preserve_changes {
        git_plain(git_binary, clone, ["stash", "pop"]).await?;
    }

    Ok(json!({
        "clone": clone,
        "worktree": clone,
        "preserved_changes": dirty && preserve_changes,
        "forced": force,
    })
    .to_string())
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

    #[tokio::test]
    async fn git_workspace_info_matches_clone_payload() {
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
            mai_tools::TOOL_GIT_WORKSPACE_INFO,
            json!({}),
        )
        .await
        .expect("execute workspace info");

        let payload: Value = serde_json::from_str(&execution.output).expect("json payload");
        assert_eq!(payload["clone"], json!(clone_path));
    }

    #[tokio::test]
    async fn git_diff_rejects_unsafe_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        let err = execute_git_tool(
            GitToolContext {
                git_binary: &git,
                projects_root: dir.path(),
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            mai_tools::TOOL_GIT_DIFF,
            json!({ "path": "../secret" }),
        )
        .await
        .expect_err("unsafe path rejected");

        assert!(err.to_string().contains("unsafe git path"));
        assert_eq!(read_git_log(dir.path()), "");
    }

    #[tokio::test]
    async fn git_branch_rejects_unsafe_branch_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        let err = execute_git_tool(
            GitToolContext {
                git_binary: &git,
                projects_root: dir.path(),
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            mai_tools::TOOL_GIT_BRANCH,
            json!({ "action": "create", "name": "../escape" }),
        )
        .await
        .expect_err("unsafe branch rejected");

        assert!(err.to_string().contains("unsafe git branch"));
        assert_eq!(read_git_log(dir.path()), "");
    }

    #[tokio::test]
    async fn git_fetch_rejects_non_origin_remote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        let err = execute_git_tool(
            GitToolContext {
                git_binary: &git,
                projects_root: dir.path(),
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            mai_tools::TOOL_GIT_FETCH,
            json!({ "remote": "upstream" }),
        )
        .await
        .expect_err("non-origin remote rejected");

        assert!(err.to_string().contains("unsupported git remote"));
        assert_eq!(read_git_log(dir.path()), "");
    }

    #[tokio::test]
    async fn git_commit_and_push_disable_hooks() {
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
            mai_tools::TOOL_GIT_COMMIT,
            json!({ "message": "save work" }),
        )
        .await
        .expect("commit");
        execute_git_tool(
            GitToolContext {
                git_binary: &git,
                projects_root: dir.path(),
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            mai_tools::TOOL_GIT_PUSH,
            json!({}),
        )
        .await
        .expect("push");

        let git_log = read_git_log(dir.path());
        assert!(git_log.contains("commit --no-verify -m save work"));
        assert!(git_log.contains(&format!(
            "push --no-verify origin HEAD:refs/heads/mai-agent/{agent_id}"
        )));
    }

    #[tokio::test]
    async fn git_sync_default_branch_refuses_dirty_clone_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path_with_status(dir.path(), " M README.md\n");
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");
        std::fs::create_dir_all(workspace::project_repo_cache_path(dir.path(), project_id))
            .expect("repo cache");

        let err = execute_git_tool(
            GitToolContext {
                git_binary: &git,
                projects_root: dir.path(),
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            mai_tools::TOOL_GIT_SYNC_DEFAULT_BRANCH,
            json!({}),
        )
        .await
        .expect_err("dirty sync rejected");

        assert!(err.to_string().contains("uncommitted changes"));
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
        fake_git_path_with_status(root, "")
    }

    fn fake_git_path_with_status(root: &Path, status_output: &str) -> String {
        let path = root.join("fake-git.sh");
        let log_path = git_log_path(root);
        let script = format!(
            r#"#!/bin/sh
LOG={}
if [ "$1" = "status" ] && [ "$2" = "--porcelain" ]; then
  printf '{}'
  exit 0
fi
printf '%s|%s\n' "$PWD" "$*" >> "$LOG"
exit 0
"#,
            shell_quote(&log_path),
            status_output.replace('\'', "'\\''")
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
