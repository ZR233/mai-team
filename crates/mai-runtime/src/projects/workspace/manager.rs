use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_protocol::{AgentId, AgentSummary, ProjectId, ProjectSummary};

use super::lease::ProjectWorkspaceLocks;
use super::reconcile::{WorkspaceReconcileReport, reconcile_project_workspaces};
use super::{delete_project_workspace, git_plain, git_with_token};
use crate::github::github_clone_url;
use crate::projects::workspace::paths::{
    agent_clone_path, project_paths, project_repo_cache_path, project_tmp_path,
};
use crate::{Result, RuntimeError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepoCacheHandle {
    pub(crate) project_id: ProjectId,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepoSyncReport {
    pub(crate) cache: RepoCacheHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentCloneHandle {
    pub(crate) project_id: ProjectId,
    pub(crate) agent_id: AgentId,
    pub(crate) path: PathBuf,
    pub(crate) branch: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CloneSeed {
    DefaultBranch,
    AgentBranch {
        branch: String,
    },
    ParentHead {
        parent_agent_id: AgentId,
    },
    ParentSnapshot {
        parent_agent_id: AgentId,
        include_uncommitted: bool,
    },
    PullRequest {
        pr_number: u64,
        head_ref: String,
    },
}

/// Owns host-side project workspace lifecycle for repository caches,
/// per-agent clones, cleanup, and filesystem reconciliation.
///
/// Implementations must keep GitHub tokens in host execution only, serialize
/// project-scoped mutations, and return handles that describe managed paths
/// without exposing token material.
pub(crate) trait ProjectWorkspaceManager: Send + Sync {
    fn ensure_repo_cache(
        &self,
        project: &ProjectSummary,
        token: &str,
    ) -> impl std::future::Future<Output = Result<RepoCacheHandle>> + Send;

    fn sync_repo_cache(
        &self,
        project: &ProjectSummary,
        token: &str,
    ) -> impl std::future::Future<Output = Result<RepoSyncReport>> + Send;

    fn prepare_agent_clone(
        &self,
        project: &ProjectSummary,
        agent_id: AgentId,
        seed: CloneSeed,
    ) -> impl std::future::Future<Output = Result<AgentCloneHandle>> + Send;

    fn cleanup_agent_clone(
        &self,
        project_id: ProjectId,
        agent_id: AgentId,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    fn delete_project_workspace(
        &self,
        project_id: ProjectId,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    fn reconcile(
        &self,
        live_projects: &[ProjectSummary],
        live_agents: &[AgentSummary],
    ) -> impl std::future::Future<Output = Result<WorkspaceReconcileReport>> + Send;
}

#[derive(Clone)]
pub(crate) struct LocalProjectWorkspaceManager {
    git_binary: String,
    projects_root: PathBuf,
    locks: Arc<ProjectWorkspaceLocks>,
}

impl LocalProjectWorkspaceManager {
    pub(crate) fn new(git_binary: String, projects_root: PathBuf) -> Self {
        Self {
            git_binary,
            projects_root,
            locks: Arc::new(ProjectWorkspaceLocks::default()),
        }
    }

    pub(crate) fn repo_cache_path(&self, project_id: ProjectId) -> PathBuf {
        project_repo_cache_path(&self.projects_root, project_id)
    }

    pub(crate) fn agent_clone_path(&self, project_id: ProjectId, agent_id: AgentId) -> PathBuf {
        agent_clone_path(&self.projects_root, project_id, agent_id)
    }

    fn repo_cache_handle(&self, project_id: ProjectId) -> RepoCacheHandle {
        RepoCacheHandle {
            project_id,
            path: self.repo_cache_path(project_id),
        }
    }
}

impl ProjectWorkspaceManager for LocalProjectWorkspaceManager {
    async fn ensure_repo_cache(
        &self,
        project: &ProjectSummary,
        token: &str,
    ) -> Result<RepoCacheHandle> {
        let _lease = self.locks.lock(project.id).await;
        if !self.repo_cache_path(project.id).exists() {
            sync_project_repo_cache_unlocked(&self.git_binary, &self.projects_root, project, token)
                .await?;
        }
        Ok(self.repo_cache_handle(project.id))
    }

    async fn sync_repo_cache(
        &self,
        project: &ProjectSummary,
        token: &str,
    ) -> Result<RepoSyncReport> {
        let _lease = self.locks.lock(project.id).await;
        let cache =
            sync_project_repo_cache_unlocked(&self.git_binary, &self.projects_root, project, token)
                .await?;
        Ok(RepoSyncReport { cache })
    }

    async fn prepare_agent_clone(
        &self,
        project: &ProjectSummary,
        agent_id: AgentId,
        seed: CloneSeed,
    ) -> Result<AgentCloneHandle> {
        let _lease = self.locks.lock(project.id).await;
        prepare_project_agent_clone_unlocked(
            &self.git_binary,
            &self.projects_root,
            project,
            agent_id,
            seed,
        )
        .await
    }

    async fn cleanup_agent_clone(&self, project_id: ProjectId, agent_id: AgentId) -> Result<()> {
        let _lease = self.locks.lock(project_id).await;
        cleanup_project_agent_clone_unlocked(&self.projects_root, project_id, agent_id)
    }

    async fn delete_project_workspace(&self, project_id: ProjectId) -> Result<()> {
        let _lease = self.locks.lock(project_id).await;
        delete_project_workspace(&self.projects_root, project_id)
    }

    async fn reconcile(
        &self,
        live_projects: &[ProjectSummary],
        live_agents: &[AgentSummary],
    ) -> Result<WorkspaceReconcileReport> {
        reconcile_project_workspaces(&self.projects_root, live_projects, live_agents)
    }
}

pub(crate) async fn sync_project_repo_cache(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    token: &str,
) -> Result<()> {
    let manager =
        LocalProjectWorkspaceManager::new(git_binary.to_string(), projects_root.to_path_buf());
    manager.sync_repo_cache(project, token).await?;
    Ok(())
}

async fn sync_project_repo_cache_unlocked(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    token: &str,
) -> Result<RepoCacheHandle> {
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
        return Ok(RepoCacheHandle {
            project_id: project.id,
            path: paths.repo_cache_path,
        });
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
    Ok(RepoCacheHandle {
        project_id: project.id,
        path: paths.repo_cache_path,
    })
}

#[allow(dead_code)]
pub(crate) async fn prepare_project_agent_clone(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    agent_id: AgentId,
) -> Result<PathBuf> {
    let manager =
        LocalProjectWorkspaceManager::new(git_binary.to_string(), projects_root.to_path_buf());
    Ok(manager
        .prepare_agent_clone(project, agent_id, CloneSeed::DefaultBranch)
        .await?
        .path)
}

async fn prepare_project_agent_clone_unlocked(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    agent_id: AgentId,
    seed: CloneSeed,
) -> Result<AgentCloneHandle> {
    let repo_cache_path = project_repo_cache_path(projects_root, project.id);
    if !repo_cache_path.exists() {
        return Err(RuntimeError::InvalidInput(
            "project repository cache is not ready".to_string(),
        ));
    }

    let clone_path = agent_clone_path(projects_root, project.id, agent_id);
    if clone_path.exists() {
        cleanup_project_agent_clone_unlocked(projects_root, project.id, agent_id)?;
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

    let (branch, start_point) = clone_checkout_target(project, agent_id, seed)?;
    git_plain(
        git_binary,
        &clone_path,
        ["checkout", "-B", &branch, &start_point],
    )
    .await?;
    Ok(AgentCloneHandle {
        project_id: project.id,
        agent_id,
        path: clone_path,
        branch,
    })
}

fn clone_checkout_target(
    project: &ProjectSummary,
    agent_id: AgentId,
    seed: CloneSeed,
) -> Result<(String, String)> {
    match seed {
        CloneSeed::DefaultBranch => Ok((
            format!("mai-agent/{agent_id}"),
            format!("origin/{}", project.branch),
        )),
        CloneSeed::AgentBranch { branch } => Ok((branch.clone(), format!("origin/{branch}"))),
        CloneSeed::ParentHead { parent_agent_id } => Err(RuntimeError::InvalidInput(format!(
            "clone seed from parent agent {parent_agent_id} is not implemented"
        ))),
        CloneSeed::ParentSnapshot {
            parent_agent_id,
            include_uncommitted,
        } => Err(RuntimeError::InvalidInput(format!(
            "clone seed from parent agent {parent_agent_id} with include_uncommitted={include_uncommitted} is not implemented"
        ))),
        CloneSeed::PullRequest {
            pr_number,
            head_ref,
        } => Err(RuntimeError::InvalidInput(format!(
            "clone seed for pull request #{pr_number} ({head_ref}) is not implemented"
        ))),
    }
}

#[allow(dead_code)]
pub(crate) async fn cleanup_project_agent_clone(
    projects_root: &Path,
    project_id: ProjectId,
    agent_id: AgentId,
) -> Result<()> {
    let manager = LocalProjectWorkspaceManager::new("git".to_string(), projects_root.to_path_buf());
    manager.cleanup_agent_clone(project_id, agent_id).await
}

fn cleanup_project_agent_clone_unlocked(
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
    use mai_protocol::{
        AgentStatus, AgentSummary, ProjectCloneStatus, ProjectStatus, ProjectSummary,
    };
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
    async fn local_workspace_manager_prepares_clone_handle_from_default_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, agent_id);
        std::fs::create_dir_all(project_repo_cache_path(dir.path(), project_id))
            .expect("repo cache");
        let manager = LocalProjectWorkspaceManager::new(git, dir.path().to_path_buf());

        let clone = manager
            .prepare_agent_clone(&project, agent_id, CloneSeed::DefaultBranch)
            .await
            .expect("prepare clone");

        assert_eq!(
            clone,
            AgentCloneHandle {
                project_id,
                agent_id,
                path: agent_clone_path(dir.path(), project_id, agent_id),
                branch: format!("mai-agent/{agent_id}"),
            }
        );
    }

    #[tokio::test]
    async fn local_workspace_manager_ensures_repo_cache_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, agent_id);
        let manager = LocalProjectWorkspaceManager::new(git, dir.path().to_path_buf());

        let cache = manager
            .ensure_repo_cache(&project, "secret-token")
            .await
            .expect("ensure cache");

        assert_eq!(
            cache,
            RepoCacheHandle {
                project_id,
                path: manager.repo_cache_path(project_id),
            }
        );
    }

    #[tokio::test]
    async fn local_workspace_manager_supports_named_agent_branch_seed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, agent_id);
        std::fs::create_dir_all(project_repo_cache_path(dir.path(), project_id))
            .expect("repo cache");
        let manager = LocalProjectWorkspaceManager::new(git, dir.path().to_path_buf());

        let clone = manager
            .prepare_agent_clone(
                &project,
                agent_id,
                CloneSeed::AgentBranch {
                    branch: "feature/demo".to_string(),
                },
            )
            .await
            .expect("prepare clone");

        assert_eq!(clone.branch, "feature/demo");
        assert!(read_git_log(dir.path()).contains("checkout -B feature/demo origin/feature/demo"));
    }

    #[tokio::test]
    async fn local_workspace_manager_rejects_unsupported_clone_seeds_explicitly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let parent_agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, agent_id);
        std::fs::create_dir_all(project_repo_cache_path(dir.path(), project_id))
            .expect("repo cache");
        let manager = LocalProjectWorkspaceManager::new(git, dir.path().to_path_buf());
        let seeds = [
            CloneSeed::ParentHead { parent_agent_id },
            CloneSeed::ParentSnapshot {
                parent_agent_id,
                include_uncommitted: true,
            },
            CloneSeed::PullRequest {
                pr_number: 7,
                head_ref: "refs/pull/7/head".to_string(),
            },
        ];

        let mut errors = Vec::new();
        for seed in seeds {
            let result = manager.prepare_agent_clone(&project, agent_id, seed).await;
            if let Err(err) = result {
                errors.push(err.to_string());
            }
            let _ = manager.cleanup_agent_clone(project_id, agent_id).await;
        }

        assert_eq!(
            errors,
            vec![
                format!("invalid input: clone seed from parent agent {parent_agent_id} is not implemented"),
                format!(
                    "invalid input: clone seed from parent agent {parent_agent_id} with include_uncommitted=true is not implemented"
                ),
                "invalid input: clone seed for pull request #7 (refs/pull/7/head) is not implemented".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn local_workspace_manager_reconcile_removes_orphan_agent_clones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let live_agent_id = uuid::Uuid::new_v4();
        let orphan_agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, live_agent_id);
        let live_agent = test_agent(live_agent_id, project_id);
        std::fs::create_dir_all(project_repo_cache_path(dir.path(), project_id))
            .expect("repo cache");
        let live_clone = agent_clone_path(dir.path(), project_id, live_agent_id);
        let orphan_clone = agent_clone_path(dir.path(), project_id, orphan_agent_id);
        std::fs::create_dir_all(&live_clone).expect("live clone");
        std::fs::create_dir_all(&orphan_clone).expect("orphan clone");
        let manager = LocalProjectWorkspaceManager::new(git, dir.path().to_path_buf());

        let report = manager
            .reconcile(&[project], &[live_agent])
            .await
            .expect("reconcile");

        assert_eq!(
            report,
            WorkspaceReconcileReport {
                orphan_clones_removed: vec![orphan_agent_id],
                legacy_worktree_dirs_removed: Vec::new(),
                missing_repo_caches: Vec::new(),
                missing_agent_clones: Vec::new(),
                invalid_clone_dirs: Vec::new(),
            }
        );
        assert!(live_clone.exists());
        assert!(!orphan_clone.exists());
    }

    #[tokio::test]
    async fn local_workspace_manager_reconcile_removes_legacy_worktree_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let project = test_project(project_id, agent_id);
        let mut live_agent = test_agent(agent_id, project_id);
        live_agent.status = AgentStatus::Deleted;
        let legacy_worktrees = project_paths(dir.path(), project_id)
            .project_dir
            .join("worktrees");
        std::fs::create_dir_all(legacy_worktrees.join(agent_id.to_string()))
            .expect("legacy worktree dir");
        let manager = LocalProjectWorkspaceManager::new(git, dir.path().to_path_buf());

        let report = manager
            .reconcile(&[project], &[live_agent])
            .await
            .expect("reconcile");

        assert_eq!(
            report,
            WorkspaceReconcileReport {
                orphan_clones_removed: Vec::new(),
                legacy_worktree_dirs_removed: vec![project_id],
                missing_repo_caches: Vec::new(),
                missing_agent_clones: Vec::new(),
                invalid_clone_dirs: Vec::new(),
            }
        );
        assert!(!legacy_worktrees.exists());
    }

    #[test]
    fn local_workspace_manager_implements_workspace_manager_trait() {
        fn assert_manager<T: ProjectWorkspaceManager>() {}

        assert_manager::<LocalProjectWorkspaceManager>();
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

    fn test_agent(agent_id: uuid::Uuid, project_id: uuid::Uuid) -> AgentSummary {
        AgentSummary {
            id: agent_id,
            name: "agent".to_string(),
            task_id: None,
            project_id: Some(project_id),
            parent_id: None,
            role: None,
            status: AgentStatus::Idle,
            model: "model".to_string(),
            provider_id: "provider".to_string(),
            provider_name: "provider".to_string(),
            reasoning_effort: None,
            docker_image: "image".to_string(),
            container_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_error: None,
            current_turn: None,
            token_usage: Default::default(),
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
