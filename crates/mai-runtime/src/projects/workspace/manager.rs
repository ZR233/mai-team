use std::path::{Path, PathBuf};

use mai_protocol::{AgentId, ProjectId, ProjectSummary};

use super::{git_plain, git_with_token};
use crate::github::github_clone_url;
use crate::projects::workspace::paths::{
    agent_clone_path, project_paths, project_repo_cache_path, project_tmp_path,
};
use crate::{Result, RuntimeError};

pub(crate) async fn sync_project_repo_cache(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    token: &str,
) -> Result<()> {
    let paths = project_paths(projects_root, project.id);
    let tmp_path = project_tmp_path(projects_root, project.id);
    let repo_url = github_clone_url(&project.owner, &project.repo);
    std::fs::create_dir_all(&paths.project_dir)?;
    std::fs::create_dir_all(&tmp_path)?;

    if paths.repo_cache_path.exists() {
        git_with_token(
            git_binary,
            &paths.repo_cache_path,
            token,
            ["remote", "set-url", "origin", &repo_url],
        )
        .await?;
        git_with_token(
            git_binary,
            &paths.repo_cache_path,
            token,
            ["fetch", "--prune", "origin"],
        )
        .await?;
        return Ok(());
    }

    let _ = std::fs::remove_dir_all(&paths.repo_cache_path);
    let repo_cache_path = paths.repo_cache_path.to_string_lossy().into_owned();
    git_with_token(
        git_binary,
        &paths.project_dir,
        token,
        ["clone", "--mirror", "--", &repo_url, &repo_cache_path],
    )
    .await?;
    Ok(())
}

pub(crate) async fn prepare_project_agent_clone(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    agent_id: AgentId,
) -> Result<PathBuf> {
    let repo_cache_path = project_repo_cache_path(projects_root, project.id);
    if !repo_cache_path.exists() {
        return Err(RuntimeError::InvalidInput(
            "project repository cache is not ready".to_string(),
        ));
    }

    let clone_path = agent_clone_path(projects_root, project.id, agent_id);
    if clone_path.exists() {
        cleanup_project_agent_clone(projects_root, project.id, agent_id).await?;
    }
    std::fs::create_dir_all(clone_path.parent().ok_or_else(|| {
        RuntimeError::InvalidInput("project clone path has no parent".to_string())
    })?)?;

    let repo_cache_arg = repo_cache_path.to_string_lossy().into_owned();
    let clone_arg = clone_path.to_string_lossy().into_owned();
    git_plain(
        git_binary,
        projects_root,
        [
            "clone",
            "--local",
            "--no-checkout",
            &repo_cache_arg,
            &clone_arg,
        ],
    )
    .await?;

    let repo_url = github_clone_url(&project.owner, &project.repo);
    git_plain(
        git_binary,
        &clone_path,
        ["remote", "set-url", "origin", &repo_url],
    )
    .await?;

    let branch = format!("mai-agent/{agent_id}");
    let origin_branch = format!("origin/{}", project.branch);
    git_plain(
        git_binary,
        &clone_path,
        ["checkout", "-B", &branch, &origin_branch],
    )
    .await?;
    Ok(clone_path)
}

pub(crate) async fn cleanup_project_agent_clone(
    projects_root: &Path,
    project_id: ProjectId,
    agent_id: AgentId,
) -> Result<()> {
    let clone_path = agent_clone_path(projects_root, project_id, agent_id);
    let _ = std::fs::remove_dir_all(clone_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::Utc;
    use mai_protocol::{ProjectCloneStatus, ProjectStatus, ProjectSummary};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::projects::workspace::paths::{
        agent_clone_path, project_paths, project_repo_cache_path,
    };

    #[tokio::test]
    async fn sync_project_repo_cache_clones_bare_mirror_with_host_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, agent_id);

        sync_project_repo_cache(&git, dir.path(), &project, "secret-token")
            .await
            .expect("sync repo cache");

        let paths = project_paths(dir.path(), project_id);
        assert!(paths.repo_cache_path.exists());
        assert!(paths.tmp_dir.exists());

        let git_log = read_git_log(dir.path());
        assert!(git_log.contains("token-present"));
        assert!(!git_log.contains("secret-token"));
        assert!(git_log.contains(&format!(
            "clone --mirror -- https://github.com/owner/repo.git {}",
            paths.repo_cache_path.display()
        )));
    }

    #[tokio::test]
    async fn prepare_project_agent_clone_uses_repo_cache_and_agent_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, agent_id);
        std::fs::create_dir_all(project_repo_cache_path(dir.path(), project_id))
            .expect("repo cache");

        let clone_path = prepare_project_agent_clone(&git, dir.path(), &project, agent_id)
            .await
            .expect("prepare clone");

        assert_eq!(
            clone_path,
            agent_clone_path(dir.path(), project_id, agent_id)
        );
        assert!(clone_path.join(".git").exists());

        let git_log = read_git_log(dir.path());
        assert!(git_log.contains(&format!(
            "clone --local --no-checkout {} {}",
            project_repo_cache_path(dir.path(), project_id).display(),
            clone_path.display()
        )));
        assert!(git_log.contains("remote set-url origin https://github.com/owner/repo.git"));
        assert!(git_log.contains(&format!("checkout -B mai-agent/{agent_id} origin/main")));
    }

    #[tokio::test]
    async fn cleanup_project_agent_clone_removes_only_agent_clone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_id = uuid::Uuid::new_v4();
        let removed_agent_id = uuid::Uuid::new_v4();
        let kept_agent_id = uuid::Uuid::new_v4();
        let removed = agent_clone_path(dir.path(), project_id, removed_agent_id);
        let kept = agent_clone_path(dir.path(), project_id, kept_agent_id);
        std::fs::create_dir_all(&removed).expect("removed clone");
        std::fs::create_dir_all(&kept).expect("kept clone");

        cleanup_project_agent_clone(dir.path(), project_id, removed_agent_id)
            .await
            .expect("cleanup clone");

        assert!(!removed.exists());
        assert!(kept.exists());
    }

    fn test_project(project_id: uuid::Uuid, agent_id: uuid::Uuid) -> ProjectSummary {
        ProjectSummary {
            id: project_id,
            name: "owner/repo".to_string(),
            status: ProjectStatus::Creating,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account-1".to_string()),
            repository_id: 1,
            installation_id: 2,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "image".to_string(),
            clone_status: ProjectCloneStatus::Pending,
            maintainer_agent_id: agent_id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
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
echo "$*" >> "$LOG"
if [ -n "$MAI_GITHUB_INSTALLATION_TOKEN" ]; then
  echo "token-present" >> "$LOG"
fi
case "$1" in
  clone)
    last=""
    for arg in "$@"; do
      last="$arg"
    done
    mkdir -p "$last"
    case "$*" in
      *"--no-checkout"*) mkdir -p "$last/.git" ;;
    esac
    ;;
esac
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
