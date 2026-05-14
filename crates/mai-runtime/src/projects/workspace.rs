use std::path::{Path, PathBuf};

use mai_protocol::{AgentId, ProjectId, ProjectSummary, preview};
use tokio::process::Command;

use crate::github::github_clone_url;
use crate::{Result, RuntimeError};

pub(crate) const PROJECT_REPO_DIR: &str = "repo";
pub(crate) const PROJECT_WORKTREES_DIR: &str = "worktrees";
pub(crate) const PROJECT_TMP_DIR: &str = "tmp";

pub(crate) fn project_dir(projects_root: &Path, project_id: ProjectId) -> PathBuf {
    projects_root.join(project_id.to_string())
}

pub(crate) fn project_repo_path(projects_root: &Path, project_id: ProjectId) -> PathBuf {
    project_dir(projects_root, project_id).join(PROJECT_REPO_DIR)
}

pub(crate) fn agent_worktree_path(
    projects_root: &Path,
    project_id: ProjectId,
    agent_id: AgentId,
) -> PathBuf {
    project_dir(projects_root, project_id)
        .join(PROJECT_WORKTREES_DIR)
        .join(agent_id.to_string())
}

fn project_tmp_path(projects_root: &Path, project_id: ProjectId) -> PathBuf {
    project_dir(projects_root, project_id).join(PROJECT_TMP_DIR)
}

pub(crate) async fn sync_project_repo(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    token: &str,
) -> Result<()> {
    let repo_path = project_repo_path(projects_root, project.id);
    let tmp_path = project_tmp_path(projects_root, project.id);
    std::fs::create_dir_all(&tmp_path)?;
    let repo_url = github_clone_url(&project.owner, &project.repo);
    if repo_path.join(".git").exists() {
        git_with_token(
            git_binary,
            &repo_path,
            token,
            ["remote", "set-url", "origin", &repo_url],
        )
        .await?;
        git_with_token(
            git_binary,
            &repo_path,
            token,
            ["fetch", "--prune", "origin"],
        )
        .await?;
        let origin_branch = format!("origin/{}", project.branch);
        git_with_token(
            git_binary,
            &repo_path,
            token,
            ["checkout", "-B", &project.branch, &origin_branch],
        )
        .await?;
        git_with_token(
            git_binary,
            &repo_path,
            token,
            ["reset", "--hard", &origin_branch],
        )
        .await?;
        git_plain(git_binary, &repo_path, ["clean", "-fdx"]).await?;
        git_plain(git_binary, &repo_path, ["worktree", "prune"]).await?;
        return Ok(());
    }

    let _ = std::fs::remove_dir_all(&repo_path);
    std::fs::create_dir_all(project_dir(projects_root, project.id))?;
    let parent = repo_path.parent().ok_or_else(|| {
        RuntimeError::InvalidInput("project repository path has no parent".to_string())
    })?;
    git_with_token(
        git_binary,
        parent,
        token,
        [
            "clone",
            "--branch",
            &project.branch,
            "--",
            &repo_url,
            &repo_path.to_string_lossy(),
        ],
    )
    .await?;
    Ok(())
}

pub(crate) async fn prepare_project_agent_worktree(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    agent_id: AgentId,
) -> Result<PathBuf> {
    let repo_path = project_repo_path(projects_root, project.id);
    if !repo_path.join(".git").exists() {
        return Err(RuntimeError::InvalidInput(
            "project repository workspace is not ready".to_string(),
        ));
    }
    let worktree_path = agent_worktree_path(projects_root, project.id, agent_id);
    if worktree_path.exists() {
        cleanup_project_agent_worktree(git_binary, projects_root, project.id, agent_id).await?;
    }
    std::fs::create_dir_all(worktree_path.parent().ok_or_else(|| {
        RuntimeError::InvalidInput("project worktree path has no parent".to_string())
    })?)?;
    let branch = format!("mai-agent/{agent_id}");
    git_plain(
        git_binary,
        &repo_path,
        [
            "worktree",
            "add",
            "-B",
            &branch,
            &worktree_path.to_string_lossy(),
            &project.branch,
        ],
    )
    .await?;
    Ok(worktree_path)
}

pub(crate) async fn cleanup_project_agent_worktree(
    git_binary: &str,
    projects_root: &Path,
    project_id: ProjectId,
    agent_id: AgentId,
) -> Result<()> {
    let repo_path = project_repo_path(projects_root, project_id);
    let worktree_path = agent_worktree_path(projects_root, project_id, agent_id);
    if repo_path.join(".git").exists() {
        let _ = git_plain(
            git_binary,
            &repo_path,
            [
                "worktree",
                "remove",
                "--force",
                &worktree_path.to_string_lossy(),
            ],
        )
        .await;
        let _ = git_plain(git_binary, &repo_path, ["worktree", "prune"]).await;
    }
    let _ = std::fs::remove_dir_all(worktree_path);
    Ok(())
}

pub(crate) fn delete_project_workspace(projects_root: &Path, project_id: ProjectId) -> Result<()> {
    let _ = std::fs::remove_dir_all(project_dir(projects_root, project_id));
    Ok(())
}

pub(crate) async fn git_plain<const N: usize>(
    git_binary: &str,
    cwd: &Path,
    args: [&str; N],
) -> Result<String> {
    run_git(git_binary, cwd, &args, None).await
}

pub(crate) async fn git_with_token<const N: usize>(
    git_binary: &str,
    cwd: &Path,
    token: &str,
    args: [&str; N],
) -> Result<String> {
    run_git(git_binary, cwd, &args, Some(token)).await
}

async fn run_git(
    git_binary: &str,
    cwd: &Path,
    args: &[&str],
    token: Option<&str>,
) -> Result<String> {
    let tmp;
    let askpass_path;
    if token.is_some() {
        tmp = tempfile::TempDir::new()?;
        askpass_path = tmp.path().join("askpass.sh");
        std::fs::write(
            &askpass_path,
            "#!/bin/sh\ncase \"$1\" in\n  *Username*) printf '%s\\n' x-access-token ;;\n  *Password*) printf '%s\\n' \"$MAI_GITHUB_INSTALLATION_TOKEN\" ;;\n  *) printf '\\n' ;;\nesac\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&askpass_path)?.permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&askpass_path, permissions)?;
        }
    } else {
        tmp = tempfile::TempDir::new()?;
        askpass_path = tmp.path().join("unused");
    }

    let mut command = Command::new(git_binary);
    command.current_dir(cwd).args(args);
    if let Some(token) = token {
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", &askpass_path)
            .env("MAI_GITHUB_INSTALLATION_TOKEN", token);
    }
    let output = command.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let combined = redact_secret(
        format!("{stderr}\n{stdout}").trim(),
        token.unwrap_or_default(),
    );
    Err(RuntimeError::InvalidInput(format!(
        "git {} failed: {}",
        args.first().copied().unwrap_or("command"),
        preview(combined.trim(), 500)
    )))
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_workspace_paths_use_host_project_layout() {
        let root = PathBuf::from("/data/.mai-team/projects");
        let project_id = uuid::Uuid::nil();
        let agent_id = uuid::Uuid::nil();

        assert_eq!(
            project_repo_path(&root, project_id),
            root.join(project_id.to_string()).join("repo")
        );
        assert_eq!(
            agent_worktree_path(&root, project_id, agent_id),
            root.join(project_id.to_string())
                .join("worktrees")
                .join(agent_id.to_string())
        );
    }
}
