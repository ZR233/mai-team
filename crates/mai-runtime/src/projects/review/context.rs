use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;

use super::target::ResolvedProjectReviewTarget;
use crate::projects::workspace::ProjectRepositoryRevision;

pub(crate) const PROJECT_REVIEW_CONTEXT_CACHE_DIR: &str = "project-review-contexts";
pub(crate) const PROJECT_REPOSITORY_CONTAINER_PATH: &str = "/project/repo";
pub(crate) const PROJECT_REVIEW_SNAPSHOT_ROOT: &str = "review-contexts";

/// review manifest 中记录的默认分支约束文件。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewConstraintSource {
    pub(crate) path: String,
    pub(crate) content_hash: String,
}

/// review 准备阶段固化、供每轮推理复用的仓库事实。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReviewManifestSnapshot {
    pub(crate) changed_files: Vec<String>,
    pub(crate) changed_files_total: usize,
    pub(crate) changed_files_hash: String,
    pub(crate) constraint_sources: Vec<ReviewConstraintSource>,
    pub(crate) skill_sources: Vec<String>,
    pub(crate) github: ReviewGithubSummary,
}

/// selector 已确认后、reviewer 启动前固化的 GitHub 只读摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewGithubSummary {
    pub(crate) reviews: ReviewCollectionSummary,
    pub(crate) checks: CheckCollectionSummary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewCollectionSummary {
    pub(crate) available: bool,
    pub(crate) total: usize,
    pub(crate) states: std::collections::BTreeMap<String, usize>,
    pub(crate) recent: Vec<ReviewSummaryItem>,
    pub(crate) content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewSummaryItem {
    pub(crate) author: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) submitted_at: Option<String>,
    pub(crate) commit_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckCollectionSummary {
    pub(crate) available: bool,
    pub(crate) total: usize,
    pub(crate) states: std::collections::BTreeMap<String, usize>,
    pub(crate) conclusions: std::collections::BTreeMap<String, usize>,
    pub(crate) recent: Vec<CheckSummaryItem>,
    pub(crate) content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckSummaryItem {
    pub(crate) name: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
}

/// Reviewer 可见的默认分支只读快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRepositoryView {
    pub(crate) volume: String,
    pub(crate) volume_subpath: String,
    pub(crate) container_path: String,
    pub(crate) base_sha: String,
}

/// 创建一次 reviewer 固定上下文所需的不可变数据。
pub(crate) struct ProjectReviewContextInit {
    pub(crate) run_id: Uuid,
    pub(crate) target: ResolvedProjectReviewTarget,
    pub(crate) project_revision: ProjectRepositoryRevision,
    pub(crate) repository_view: ProjectRepositoryView,
    pub(crate) manifest: ReviewManifestSnapshot,
    pub(crate) root: PathBuf,
    pub(crate) skills_cache_dir: PathBuf,
    pub(crate) workspace_instructions: String,
}

impl ProjectRepositoryView {
    pub(crate) fn for_run(volume: String, run_id: Uuid, base_sha: String) -> Self {
        Self {
            volume,
            volume_subpath: format!("{PROJECT_REVIEW_SNAPSHOT_ROOT}/{run_id}/repo"),
            container_path: PROJECT_REPOSITORY_CONTAINER_PATH.to_string(),
            base_sha,
        }
    }

    pub(crate) fn workspace_path(&self) -> String {
        format!("/workspace/{}", self.volume_subpath)
    }
}

/// 单次 reviewer 固定使用的默认分支上下文快照。
///
/// 技能目录和项目记忆都在 reviewer turn 前从同一个 detached worktree 提取；
/// `Drop` 只能清理 host cache，volume 中的 worktree 必须由 agent 生命周期异步释放。
#[derive(Debug)]
pub(crate) struct ProjectReviewContext {
    pub(crate) run_id: Uuid,
    pub(crate) target: ResolvedProjectReviewTarget,
    pub(crate) project_revision: ProjectRepositoryRevision,
    pub(crate) repository_view: ProjectRepositoryView,
    pub(crate) manifest: ReviewManifestSnapshot,
    pub(crate) skills_cache_dir: PathBuf,
    pub(crate) workspace_instructions: String,
    target_stale: AtomicBool,
    repository_released: AtomicBool,
    root: PathBuf,
}

impl ProjectReviewContext {
    pub(crate) fn new(init: ProjectReviewContextInit) -> Self {
        Self {
            run_id: init.run_id,
            target: init.target,
            project_revision: init.project_revision,
            repository_view: init.repository_view,
            manifest: init.manifest,
            skills_cache_dir: init.skills_cache_dir,
            workspace_instructions: init.workspace_instructions,
            target_stale: AtomicBool::new(false),
            repository_released: AtomicBool::new(false),
            root: init.root,
        }
    }

    pub(crate) fn mark_target_stale(&self) {
        self.target_stale.store(true, Ordering::Release);
    }

    pub(crate) fn target_is_stale(&self) -> bool {
        self.target_stale.load(Ordering::Acquire)
    }

    pub(crate) fn repository_is_released(&self) -> bool {
        self.repository_released.load(Ordering::Acquire)
    }

    pub(crate) fn mark_repository_released(&self) {
        self.repository_released.store(true, Ordering::Release);
    }

    pub(crate) fn remove_host_cache(&self) -> std::io::Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for ProjectReviewContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_context_owns_and_removes_its_snapshot_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("review-context");
        let skills = root.join("skills");
        std::fs::create_dir_all(&skills).expect("create snapshot");
        std::fs::write(root.join("AGENTS.md"), "default branch memory").expect("write snapshot");
        let context = ProjectReviewContext::new(ProjectReviewContextInit {
            run_id: Uuid::new_v4(),
            target: ResolvedProjectReviewTarget {
                pr: 42,
                head_sha: "head".to_string(),
            },
            project_revision: ProjectRepositoryRevision {
                branch: "main".to_string(),
                base_sha: "base".to_string(),
            },
            repository_view: ProjectRepositoryView::for_run(
                "project-volume".to_string(),
                Uuid::nil(),
                "base".to_string(),
            ),
            manifest: ReviewManifestSnapshot::default(),
            root: root.clone(),
            skills_cache_dir: skills,
            workspace_instructions: "default branch memory".to_string(),
        });

        assert_eq!(context.workspace_instructions, "default branch memory");
        assert!(!context.target_is_stale());
        assert!(!context.repository_is_released());
        context.mark_target_stale();
        assert!(context.target_is_stale());
        context.mark_repository_released();
        assert!(context.repository_is_released());
        drop(context);
        assert!(!root.exists());
    }
}
