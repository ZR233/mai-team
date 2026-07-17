use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;

use super::target::ResolvedProjectReviewTarget;
use crate::projects::workspace::ProjectRepositoryRevision;

pub(crate) const PROJECT_REVIEW_CONTEXT_CACHE_DIR: &str = "project-review-contexts";

/// 单次 reviewer 固定使用的默认分支上下文快照。
///
/// 技能目录和项目记忆都在 reviewer turn 前从项目 canonical repository 提取；
/// `Drop` 是异常路径的最终清理保障，正常删除仍由 agent 生命周期显式释放。
#[derive(Debug)]
pub(crate) struct ProjectReviewContext {
    pub(crate) run_id: Uuid,
    pub(crate) target: ResolvedProjectReviewTarget,
    pub(crate) project_revision: ProjectRepositoryRevision,
    pub(crate) skills_cache_dir: PathBuf,
    pub(crate) workspace_instructions: String,
    target_stale: AtomicBool,
    root: PathBuf,
}

impl ProjectReviewContext {
    pub(crate) fn new(
        run_id: Uuid,
        target: ResolvedProjectReviewTarget,
        project_revision: ProjectRepositoryRevision,
        root: PathBuf,
        skills_cache_dir: PathBuf,
        workspace_instructions: String,
    ) -> Self {
        Self {
            run_id,
            target,
            project_revision,
            skills_cache_dir,
            workspace_instructions,
            target_stale: AtomicBool::new(false),
            root,
        }
    }

    pub(crate) fn mark_target_stale(&self) {
        self.target_stale.store(true, Ordering::Release);
    }

    pub(crate) fn target_is_stale(&self) -> bool {
        self.target_stale.load(Ordering::Acquire)
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
        let context = ProjectReviewContext::new(
            Uuid::new_v4(),
            ResolvedProjectReviewTarget {
                pr: 42,
                head_sha: "head".to_string(),
            },
            ProjectRepositoryRevision {
                branch: "main".to_string(),
                base_sha: "base".to_string(),
            },
            root.clone(),
            skills,
            "default branch memory".to_string(),
        );

        assert_eq!(context.workspace_instructions, "default branch memory");
        assert!(!context.target_is_stale());
        context.mark_target_stale();
        assert!(context.target_is_stale());
        drop(context);
        assert!(!root.exists());
    }
}
