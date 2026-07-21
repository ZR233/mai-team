use serde::Serialize;

use crate::projects::review::context::ProjectReviewContext;
use crate::skills::SkillInjections;
use crate::{Result, RuntimeError};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewManifest<'a> {
    pull_request: u64,
    base_sha: &'a str,
    head_sha: &'a str,
    default_branch: &'a str,
    project_repository: RepositoryView<'a>,
    review_repository: RepositoryView<'a>,
    changed_files: &'a [String],
    changed_files_total: usize,
    changed_files_hash: &'a str,
    constraint_sources: &'a [crate::projects::review::context::ReviewConstraintSource],
    discovered_skill_sources: &'a [String],
    activated_skills: Vec<&'a str>,
    github: &'a crate::projects::review::context::ReviewGithubSummary,
    preflight: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryView<'a> {
    path: &'a str,
    revision: &'a str,
    access: &'static str,
    purpose: &'static str,
}

/// 将 review 准备阶段已经解析的事实固化为 PL pinned context。
pub(super) fn section(
    context: &ProjectReviewContext,
    skills: &SkillInjections,
) -> Result<pl_core::PinnedContextSection> {
    let manifest = ReviewManifest {
        pull_request: context.target.pr,
        base_sha: &context.project_revision.base_sha,
        head_sha: &context.target.head_sha,
        default_branch: &context.project_revision.branch,
        project_repository: RepositoryView {
            path: &context.repository_view.container_path,
            revision: &context.project_revision.base_sha,
            access: "read_only",
            purpose: "project constraints, guidelines, skills, memory, and existing patterns",
        },
        review_repository: RepositoryView {
            path: "/workspace/repo",
            revision: &context.target.head_sha,
            access: "read_write",
            purpose: "PR-head code, diffs, tests, and temporary review files",
        },
        changed_files: &context.manifest.changed_files,
        changed_files_total: context.manifest.changed_files_total,
        changed_files_hash: &context.manifest.changed_files_hash,
        constraint_sources: &context.manifest.constraint_sources,
        discovered_skill_sources: &context.manifest.skill_sources,
        activated_skills: skills
            .items
            .iter()
            .map(|skill| skill.metadata.name.as_str())
            .collect(),
        github: &context.manifest.github,
        preflight: "GitHub review/check eligibility passed before this reviewer was prepared",
    };
    let content = serde_json::to_string_pretty(&manifest).map_err(|error| {
        RuntimeError::InvalidInput(format!("failed to serialize review manifest: {error}"))
    })?;
    pl_core::context_section(
        pl_core::REVIEW_MANIFEST_SECTION_ID,
        1,
        "Review Manifest",
        content,
    )
    .map_err(RuntimeError::Model)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use crate::projects::review::context::{
        ProjectRepositoryView, ProjectReviewContextInit, ReviewManifestSnapshot,
    };
    use crate::projects::review::target::ResolvedProjectReviewTarget;
    use crate::projects::workspace::ProjectRepositoryRevision;

    #[test]
    fn manifest_keeps_repository_roles_and_revisions_explicit() {
        let root = tempfile::tempdir().expect("tempdir");
        let context = ProjectReviewContext::new(ProjectReviewContextInit {
            run_id: Uuid::new_v4(),
            target: ResolvedProjectReviewTarget {
                pr: 1631,
                head_sha: "head".to_string(),
            },
            project_revision: ProjectRepositoryRevision {
                branch: "main".to_string(),
                base_sha: "base".to_string(),
            },
            repository_view: ProjectRepositoryView::for_run(
                "project-volume".to_string(),
                Uuid::new_v4(),
                "base".to_string(),
            ),
            manifest: ReviewManifestSnapshot {
                changed_files: vec!["M\tsrc/lib.rs".to_string()],
                changed_files_total: 1,
                changed_files_hash: "files-hash".to_string(),
                ..ReviewManifestSnapshot::default()
            },
            root: root.path().to_path_buf(),
            skills_cache_dir: PathBuf::new(),
            workspace_instructions: String::new(),
        });

        let section = section(&context, &SkillInjections::default()).expect("manifest");

        assert_eq!(section.id.as_str(), pl_core::REVIEW_MANIFEST_SECTION_ID);
        assert!(section.content.contains("/project/repo"));
        assert!(section.content.contains("/workspace/repo"));
        assert!(section.content.contains("1631"));
        assert!(section.content.contains("files-hash"));
    }
}
