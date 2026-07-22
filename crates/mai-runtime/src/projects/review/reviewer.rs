use std::future::Future;
use std::sync::Arc;

use mai_protocol::{
    AgentId, AgentModelPreference, AgentRole, AgentSummary, CreateAgentRequest, ProjectId,
    ProjectSummary, TurnId,
};

use crate::agents::ContainerSource;
use crate::projects::review::context::ProjectReviewContext;
use crate::projects::review::target::{
    ProjectReviewRequest, ProjectReviewTargetOps, ResolvedProjectReviewTarget,
    resolve_project_review_target,
};
use crate::projects::workspace::{ProjectRepositoryReviewTarget, ProjectRepositoryRevision};
use crate::{Result, RuntimeError};
use uuid::Uuid;

const REVIEWER_SKILL_MENTION: &str = "reviewer-agent-review-pr";

/// Provides the agent and project operations needed by the project review loop
/// to create and run a short-lived reviewer agent.
pub(crate) trait ProjectReviewerAgentOps: ProjectReviewTargetOps + Send + Sync {
    fn agent_summary(&self, agent_id: AgentId)
    -> impl Future<Output = Result<AgentSummary>> + Send;

    fn reviewer_model(&self) -> impl Future<Output = Result<AgentModelPreference>> + Send;

    fn project_reviewer_agents(
        &self,
        project_id: ProjectId,
    ) -> impl Future<Output = Vec<AgentSummary>> + Send;

    fn sync_project_repository_for_review(
        &self,
        project_id: ProjectId,
        target: ResolvedProjectReviewTarget,
    ) -> impl Future<Output = Result<ProjectRepositoryRevision>> + Send;

    fn create_project_review_context(
        &self,
        project_id: ProjectId,
        run_id: Uuid,
        target: ResolvedProjectReviewTarget,
        project_revision: ProjectRepositoryRevision,
    ) -> impl Future<Output = Result<Arc<ProjectReviewContext>>> + Send;

    fn create_agent_with_container_source(
        &self,
        request: CreateAgentRequest,
        source: ContainerSource,
        task_id: Option<mai_protocol::TaskId>,
        project_id: Option<ProjectId>,
        role: Option<AgentRole>,
    ) -> impl Future<Output = Result<AgentSummary>> + Send;

    fn attach_project_review_context(
        &self,
        agent_id: AgentId,
        context: Arc<ProjectReviewContext>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn delete_project_review_context(
        &self,
        project_id: ProjectId,
        context: Arc<ProjectReviewContext>,
    ) -> impl Future<Output = Result<()>> + Send;

    fn delete_agent(&self, agent_id: AgentId) -> impl Future<Output = Result<()>> + Send;

    fn start_agent_turn(
        &self,
        agent_id: AgentId,
        message: String,
        skill_mentions: Vec<String>,
    ) -> impl Future<Output = Result<TurnId>> + Send;

    fn last_turn_response(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<String>>> + Send;
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedProjectReviewer {
    pub(crate) agent: AgentSummary,
    pub(crate) target: ResolvedProjectReviewTarget,
    pub(crate) project_revision: ProjectRepositoryRevision,
}

pub(crate) async fn prepare_project_reviewer(
    ops: &impl ProjectReviewerAgentOps,
    project_id: ProjectId,
    run_id: Uuid,
    request: ProjectReviewRequest,
) -> Result<PreparedProjectReviewer> {
    let existing_reviewer_id = ops
        .project_reviewer_agents(project_id)
        .await
        .first()
        .map(|reviewer| reviewer.id);
    ensure_project_reviewer_slot_available(existing_reviewer_id)?;
    let target = resolve_project_review_target(ops, project_id, request).await?;
    let project_revision = ops
        .sync_project_repository_for_review(project_id, target.clone())
        .await?;
    let project_summary = ops.project_summary(project_id).await?;
    let maintainer_summary = ops
        .agent_summary(project_summary.maintainer_agent_id)
        .await?;
    let model = ops.reviewer_model().await?;
    let context = ops
        .create_project_review_context(project_id, run_id, target.clone(), project_revision.clone())
        .await?;
    let agent = match ops
        .create_agent_with_container_source(
            CreateAgentRequest {
                name: Some(format!("{} Auto Reviewer", project_summary.name)),
                provider_id: Some(model.provider_id),
                model: Some(model.model),
                reasoning_effort: model.reasoning_effort,
                docker_image: Some(maintainer_summary.docker_image.clone()),
                parent_id: Some(project_summary.maintainer_agent_id),
                system_prompt: Some(super::project_reviewer_system_prompt(&context)),
            },
            ContainerSource::ProjectReviewWorkspace {
                target: ProjectRepositoryReviewTarget {
                    pr: target.pr,
                    head_sha: target.head_sha.clone(),
                },
                revision: project_revision.clone(),
                repository_view: context.repository_view.clone(),
            },
            maintainer_summary.task_id,
            Some(project_id),
            Some(AgentRole::Reviewer),
        )
        .await
    {
        Ok(agent) => agent,
        Err(error) => {
            if let Err(cleanup_error) = ops
                .delete_project_review_context(project_id, Arc::clone(&context))
                .await
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "reviewer creation failed: {error}; context cleanup failed: {cleanup_error}"
                )));
            }
            return Err(error);
        }
    };
    if let Err(error) = ops
        .attach_project_review_context(agent.id, Arc::clone(&context))
        .await
    {
        let delete_error = ops.delete_agent(agent.id).await.err();
        let cleanup_error = ops
            .delete_project_review_context(project_id, context)
            .await
            .err();
        if delete_error.is_some() || cleanup_error.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "review context attachment failed: {error}; reviewer cleanup: {}; context cleanup: {}",
                delete_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "ok".to_string()),
                cleanup_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "ok".to_string())
            )));
        }
        return Err(error);
    }
    Ok(PreparedProjectReviewer {
        agent,
        target,
        project_revision,
    })
}

fn ensure_project_reviewer_slot_available(existing_reviewer_id: Option<AgentId>) -> Result<()> {
    if let Some(existing_reviewer_id) = existing_reviewer_id {
        return Err(RuntimeError::InvalidInput(format!(
            "project already owns reviewer agent `{existing_reviewer_id}`"
        )));
    }
    Ok(())
}

pub(crate) async fn project_reviewer_initial_message(
    ops: &impl ProjectReviewerAgentOps,
    project_id: ProjectId,
    reviewer_id: AgentId,
    target: ResolvedProjectReviewTarget,
    project_revision: ProjectRepositoryRevision,
) -> Result<String> {
    let summary = ops.project_summary(project_id).await?;
    Ok(project_reviewer_initial_message_from_summary(
        &summary,
        reviewer_id,
        &target,
        &project_revision,
    ))
}

pub(crate) async fn start_reviewer_turn(
    ops: &impl ProjectReviewerAgentOps,
    reviewer_id: AgentId,
    message: String,
) -> Result<TurnId> {
    ops.start_agent_turn(
        reviewer_id,
        message,
        vec![REVIEWER_SKILL_MENTION.to_string()],
    )
    .await
}

pub(crate) async fn last_turn_response(
    ops: &impl ProjectReviewerAgentOps,
    reviewer_id: AgentId,
) -> Result<String> {
    ops.last_turn_response(reviewer_id).await?.ok_or_else(|| {
        RuntimeError::InvalidInput("reviewer did not return a final response".to_string())
    })
}

fn project_reviewer_initial_message_from_summary(
    summary: &ProjectSummary,
    reviewer_id: AgentId,
    target: &ResolvedProjectReviewTarget,
    project_revision: &ProjectRepositoryRevision,
) -> String {
    let extra = summary
        .reviewer_extra_prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("None");
    format!(
        "Run one automatic pull request review for project `{}`.\n\nRepository: {}/{}\nDefault branch: {}\nBase SHA: {}\nDefault-branch constraint and pattern view: /project/repo (read-only, fixed at the base SHA)\nTarget pull request: review PR #{} only\nPR head SHA: {}\nPR-head review workspace: /workspace/repo (writable, fixed at the head SHA)\nReviewer agent: {}\n\nStart by reading project constraints and existing patterns from `/project/repo`. Read the changed implementation and PR-only files from `/workspace/repo`. A constraint file changed by the PR is a proposal to review and does not override the default-branch version for this run. Never modify or run Git commands in `/project/repo`; run all diff, HEAD verification, build, and test commands in `/workspace/repo`.\n\nThe service already fetched both revisions and verified that `/workspace/repo` is clean and checked out at the exact PR head. Do not select another pull request and do not checkout another revision. Compare against `origin/{}`.\n\nExtra reviewer instructions:\n{}\n\nUse the $reviewer-agent-review-pr skill. At the end of the turn, return only one JSON object matching this schema exactly:\n{{\"outcome\":\"review_submitted|failed\",\"review_event\":\"approve|request_changes|comment\"|null,\"pr\":123|null,\"summary\":\"short result\",\"error\":null|\"failure reason\"}}",
        summary.name,
        summary.owner,
        summary.repo,
        project_revision.branch,
        project_revision.base_sha,
        target.pr,
        target.head_sha,
        reviewer_id,
        project_revision.branch,
        extra
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mai_protocol::{
        AgentModelPreference, AgentState, ProjectCloneStatus, ProjectReviewOutcome,
        ProjectReviewStatus, ProjectStatus, ProjectSummary, TokenUsage, now,
    };
    use serde_json::{Value, json};
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::{
        ContainerSource, ProjectRepositoryRevision, ProjectReviewContext, ProjectReviewRequest,
        ProjectReviewerAgentOps, ResolvedProjectReviewTarget,
        ensure_project_reviewer_slot_available, prepare_project_reviewer,
        project_reviewer_initial_message_from_summary,
    };
    use crate::projects::review::context::ProjectRepositoryView;
    use crate::{Result, RuntimeError};

    #[derive(Clone, Copy)]
    enum PreparationFailure {
        Create,
        Attach,
    }

    #[derive(Clone)]
    struct FakeReviewerOps {
        project: ProjectSummary,
        maintainer: mai_protocol::AgentSummary,
        reviewer: mai_protocol::AgentSummary,
        context: Arc<ProjectReviewContext>,
        failure: PreparationFailure,
        operations: Arc<Mutex<Vec<&'static str>>>,
        _temp: Arc<tempfile::TempDir>,
    }

    impl crate::projects::review::target::ProjectReviewTargetOps for FakeReviewerOps {
        async fn project_summary(
            &self,
            _project_id: mai_protocol::ProjectId,
        ) -> Result<ProjectSummary> {
            Ok(self.project.clone())
        }

        async fn github_api_get_json(
            &self,
            _project_id: mai_protocol::ProjectId,
            _path: String,
        ) -> Result<Value> {
            Ok(json!({ "head": { "sha": "head-sha" } }))
        }
    }

    impl ProjectReviewerAgentOps for FakeReviewerOps {
        async fn agent_summary(
            &self,
            _agent_id: mai_protocol::AgentId,
        ) -> Result<mai_protocol::AgentSummary> {
            Ok(self.maintainer.clone())
        }

        async fn reviewer_model(&self) -> Result<AgentModelPreference> {
            Ok(AgentModelPreference {
                provider_id: "provider".to_string(),
                model: "model".to_string(),
                reasoning_effort: None,
            })
        }

        async fn project_reviewer_agents(
            &self,
            _project_id: mai_protocol::ProjectId,
        ) -> Vec<mai_protocol::AgentSummary> {
            Vec::new()
        }

        async fn sync_project_repository_for_review(
            &self,
            _project_id: mai_protocol::ProjectId,
            _target: ResolvedProjectReviewTarget,
        ) -> Result<ProjectRepositoryRevision> {
            Ok(ProjectRepositoryRevision {
                branch: "main".to_string(),
                base_sha: "base-sha".to_string(),
            })
        }

        async fn create_project_review_context(
            &self,
            _project_id: mai_protocol::ProjectId,
            _run_id: Uuid,
            _target: ResolvedProjectReviewTarget,
            _project_revision: ProjectRepositoryRevision,
        ) -> Result<Arc<ProjectReviewContext>> {
            self.operations.lock().await.push("create_context");
            Ok(Arc::clone(&self.context))
        }

        async fn create_agent_with_container_source(
            &self,
            request: mai_protocol::CreateAgentRequest,
            source: ContainerSource,
            _task_id: Option<mai_protocol::TaskId>,
            _project_id: Option<mai_protocol::ProjectId>,
            _role: Option<mai_protocol::AgentRole>,
        ) -> Result<mai_protocol::AgentSummary> {
            self.operations.lock().await.push("create_agent");
            let prompt = request.system_prompt.expect("reviewer system prompt");
            assert!(prompt.contains("`/project/repo`"));
            assert!(prompt.contains("`/workspace/repo`"));
            assert!(prompt.contains("base snapshot `base-sha`"));
            assert!(prompt.contains("PR #24 at head `head-sha`"));
            assert!(prompt.contains("`write_session_note` using `expectedRevision: 0`"));
            assert!(prompt.contains("recording only immutable metadata"));
            assert!(prompt.contains("Do not add progress checkboxes"));
            assert!(prompt.contains("immediately append its complete record"));
            assert!(prompt.contains("`apply_session_note_patch`"));
            assert!(prompt.contains("never overwrite the note"));
            assert!(prompt.contains("changes or removes an existing line"));
            assert!(prompt.contains("entire ledger with paginated `read_session_note` calls"));
            assert!(prompt.contains("start at line 1 with at most 500 lines"));
            assert!(prompt.contains("keep the returned revision fixed as `expectedRevision`"));
            assert!(prompt.contains("every non-empty `nextStartLine`"));
            assert!(prompt.contains("restart reading from line 1"));
            assert!(prompt.contains("do not use its optional cursor"));
            assert!(prompt.contains("one logical final pull request review"));
            assert!(prompt.contains("never fall back to a temporary file"));
            assert!(!prompt.contains("POSIX `sh`"));
            assert!(!prompt.contains("`bash -lc`"));
            assert!(!prompt.contains("/tmp/mai-review-findings.md"));
            assert!(matches!(
                source,
                ContainerSource::ProjectReviewWorkspace { repository_view, .. }
                    if repository_view == self.context.repository_view
            ));
            if matches!(self.failure, PreparationFailure::Create) {
                return Err(RuntimeError::InvalidInput("create failed".to_string()));
            }
            Ok(self.reviewer.clone())
        }

        async fn attach_project_review_context(
            &self,
            _agent_id: mai_protocol::AgentId,
            _context: Arc<ProjectReviewContext>,
        ) -> Result<()> {
            self.operations.lock().await.push("attach_context");
            if matches!(self.failure, PreparationFailure::Attach) {
                return Err(RuntimeError::InvalidInput("attach failed".to_string()));
            }
            Ok(())
        }

        async fn delete_project_review_context(
            &self,
            _project_id: mai_protocol::ProjectId,
            _context: Arc<ProjectReviewContext>,
        ) -> Result<()> {
            self.operations.lock().await.push("delete_context");
            Ok(())
        }

        async fn delete_agent(&self, _agent_id: mai_protocol::AgentId) -> Result<()> {
            self.operations.lock().await.push("delete_agent");
            Ok(())
        }

        async fn start_agent_turn(
            &self,
            _agent_id: mai_protocol::AgentId,
            _message: String,
            _skill_mentions: Vec<String>,
        ) -> Result<mai_protocol::TurnId> {
            Err(RuntimeError::InvalidInput("unused".to_string()))
        }

        async fn last_turn_response(
            &self,
            _agent_id: mai_protocol::AgentId,
        ) -> Result<Option<String>> {
            Ok(None)
        }
    }

    #[test]
    fn existing_reviewer_blocks_second_project_reviewer() {
        let reviewer_id = Uuid::new_v4();

        let error = ensure_project_reviewer_slot_available(Some(reviewer_id))
            .expect_err("duplicate reviewer must be rejected");

        assert_eq!(
            error.to_string(),
            format!("invalid input: project already owns reviewer agent `{reviewer_id}`")
        );
    }

    #[test]
    fn target_pr_message_delegates_selection_to_system_selector() {
        let reviewer_id = Uuid::new_v4();
        let message = project_reviewer_initial_message_from_summary(
            &test_project_summary(),
            reviewer_id,
            &ResolvedProjectReviewTarget {
                pr: 24,
                head_sha: "head-sha".to_string(),
            },
            &ProjectRepositoryRevision {
                branch: "main".to_string(),
                base_sha: "base-sha".to_string(),
            },
        );

        assert!(message.contains("review PR #24 only"));
        assert!(message.contains("Base SHA: base-sha"));
        assert!(message.contains("PR head SHA: head-sha"));
        assert!(message.contains("/project/repo (read-only"));
        assert!(message.contains("/workspace/repo (writable"));
        assert!(message.contains("does not override"));
        assert!(message.contains("checked out at the exact PR head"));
        assert!(!message.contains("select-pr"));
        assert!(!message.contains("eligibility"));
        assert!(!message.contains("filtering"));
    }

    #[tokio::test]
    async fn agent_creation_failure_releases_prepared_review_context() {
        let ops = fake_reviewer_ops(PreparationFailure::Create);

        let error = prepare_project_reviewer(
            &ops,
            ops.project.id,
            Uuid::new_v4(),
            ProjectReviewRequest {
                pr: 24,
                head_sha_hint: None,
            },
        )
        .await
        .expect_err("creation must fail");

        assert!(error.to_string().contains("create failed"));
        assert_eq!(
            *ops.operations.lock().await,
            vec!["create_context", "create_agent", "delete_context"]
        );
    }

    #[tokio::test]
    async fn context_attach_failure_deletes_reviewer_and_context() {
        let ops = fake_reviewer_ops(PreparationFailure::Attach);

        let error = prepare_project_reviewer(
            &ops,
            ops.project.id,
            Uuid::new_v4(),
            ProjectReviewRequest {
                pr: 24,
                head_sha_hint: None,
            },
        )
        .await
        .expect_err("attach must fail");

        assert!(error.to_string().contains("attach failed"));
        assert_eq!(
            *ops.operations.lock().await,
            vec![
                "create_context",
                "create_agent",
                "attach_context",
                "delete_agent",
                "delete_context",
            ]
        );
    }

    fn fake_reviewer_ops(failure: PreparationFailure) -> FakeReviewerOps {
        let temp = Arc::new(tempfile::tempdir().expect("tempdir"));
        let project = test_project_summary();
        let context_root = temp.path().join("context");
        let skills = context_root.join("skills");
        std::fs::create_dir_all(&skills).expect("skills");
        let context = Arc::new(ProjectReviewContext::new(
            crate::projects::review::context::ProjectReviewContextInit {
                run_id: Uuid::new_v4(),
                target: ResolvedProjectReviewTarget {
                    pr: 24,
                    head_sha: "head-sha".to_string(),
                },
                project_revision: ProjectRepositoryRevision {
                    branch: "main".to_string(),
                    base_sha: "base-sha".to_string(),
                },
                repository_view: ProjectRepositoryView::for_run(
                    "project-volume".to_string(),
                    Uuid::new_v4(),
                    "base-sha".to_string(),
                ),
                manifest: crate::projects::review::context::ReviewManifestSnapshot::default(),
                root: context_root,
                skills_cache_dir: skills,
                workspace_instructions: "base constraints".to_string(),
            },
        ));
        FakeReviewerOps {
            maintainer: test_agent_summary(
                project.id,
                project.maintainer_agent_id,
                mai_protocol::AgentRole::Executor,
            ),
            reviewer: test_agent_summary(
                project.id,
                Uuid::new_v4(),
                mai_protocol::AgentRole::Reviewer,
            ),
            project,
            context,
            failure,
            operations: Arc::new(Mutex::new(Vec::new())),
            _temp: temp,
        }
    }

    fn test_agent_summary(
        project_id: mai_protocol::ProjectId,
        id: mai_protocol::AgentId,
        role: mai_protocol::AgentRole,
    ) -> mai_protocol::AgentSummary {
        let timestamp = now();
        mai_protocol::AgentSummary {
            id,
            parent_id: None,
            task_id: None,
            project_id: Some(project_id),
            role: Some(role),
            name: role.to_string(),
            state: AgentState::default(),
            container_id: None,
            docker_image: "reviewer:latest".to_string(),
            provider_id: "provider".to_string(),
            provider_name: "Provider".to_string(),
            model: "model".to_string(),
            reasoning_effort: None,
            created_at: timestamp,
            updated_at: timestamp,
            token_usage: TokenUsage::default(),
        }
    }

    fn test_project_summary() -> ProjectSummary {
        ProjectSummary {
            id: Uuid::new_v4(),
            name: "owner/repo".to_string(),
            status: ProjectStatus::Ready,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_full_name: "owner/repo".to_string(),
            git_account_id: Some("account-1".to_string()),
            repository_id: 42,
            installation_id: 0,
            installation_account: "owner".to_string(),
            branch: "main".to_string(),
            docker_image: "mai-sidecar:local".to_string(),
            clone_status: ProjectCloneStatus::Ready,
            maintainer_agent_id: Uuid::new_v4(),
            created_at: now(),
            updated_at: now(),
            last_error: None,
            auto_review_enabled: true,
            reviewer_extra_prompt: None,
            review_status: ProjectReviewStatus::Idle,
            current_reviewer_agent_id: None,
            last_review_started_at: None,
            last_review_finished_at: None,
            next_review_at: None,
            last_review_outcome: Some(ProjectReviewOutcome::NoEligiblePr),
            review_last_error: None,
        }
    }
}
