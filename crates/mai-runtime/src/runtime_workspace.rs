use super::*;

impl AgentRuntime {
    pub(super) async fn set_project_clone_result(
        &self,
        project_id: ProjectId,
        status: ProjectStatus,
        clone_status: ProjectCloneStatus,
        last_error: Option<String>,
    ) -> Result<ProjectSummary> {
        let project = self.project(project_id).await?;
        let updated = {
            let mut summary = project.summary.write().await;
            summary.status = status;
            summary.clone_status = clone_status;
            summary.last_error = last_error;
            summary.updated_at = now();
            summary.clone()
        };
        self.deps.store.save_project(&updated).await?;
        self.events
            .publish(ServiceEventKind::ProjectUpdated {
                project: updated.clone(),
            })
            .await;
        Ok(updated)
    }

    pub(super) async fn start_project_workspace(
        self: &Arc<Self>,
        project_id: ProjectId,
        maintainer_agent_id: AgentId,
    ) -> Result<()> {
        self.set_project_clone_result(
            project_id,
            ProjectStatus::Creating,
            ProjectCloneStatus::Cloning,
            None,
        )
        .await?;

        if let Err(err) = self
            .clone_project_repository(project_id, maintainer_agent_id)
            .await
        {
            self.shutdown_project_mcp_manager(project_id).await;
            let _ = self.delete_project_sidecar(project_id).await;
            self.set_project_clone_result(
                project_id,
                ProjectStatus::Failed,
                ProjectCloneStatus::Failed,
                Some(err.to_string()),
            )
            .await?;
            tracing::warn!(project_id = %project_id, "project workspace clone failed: {err}");
            return Ok(());
        }

        self.set_project_clone_result(
            project_id,
            ProjectStatus::Ready,
            ProjectCloneStatus::Ready,
            None,
        )
        .await?;

        let maintainer = self.agent(maintainer_agent_id).await?;
        let source = self
            .agent_container_source_for_project(
                maintainer_agent_id,
                Some(project_id),
                agents::ContainerSource::FreshImage,
            )
            .await?;
        agents::ensure_agent_container_with_source(self.as_ref(), &maintainer, &source).await?;
        self.start_project_review_loop_if_ready(project_id).await?;
        Ok(())
    }

    pub(super) async fn start_enabled_project_review_workers(self: &Arc<Self>) {
        projects::review::worker::start_enabled_project_review_workers(Arc::clone(self)).await
    }

    pub(super) async fn reconcile_project_review_singletons(self: &Arc<Self>) {
        projects::review::worker::reconcile_project_review_singletons(
            Arc::clone(self),
            PROJECT_REVIEW_RUN_LIST_LIMIT,
        )
        .await
    }

    pub(super) async fn start_project_review_loop_if_ready(
        self: &Arc<Self>,
        project_id: ProjectId,
    ) -> Result<()> {
        projects::review::worker::start_project_review_loop_if_ready(Arc::clone(self), project_id)
            .await
    }

    pub(super) async fn stop_project_review_loop(self: &Arc<Self>, project_id: ProjectId) {
        projects::review::worker::stop_project_review_loop(
            Arc::clone(self),
            project_id,
            PROJECT_REVIEW_RUN_LIST_LIMIT,
        )
        .await
    }

    pub(super) async fn run_project_review_once(
        self: &Arc<Self>,
        project_id: ProjectId,
        cancellation_token: CancellationToken,
        request: projects::review::target::ProjectReviewRequest,
    ) -> Result<ProjectReviewCycleResult> {
        let project = self.project(project_id).await?;
        let _review_cycle_guard = project.review_cycle_lock.lock().await;
        projects::review::cycle::run_project_review_once(
            self,
            project_id,
            cancellation_token,
            request,
        )
        .await
    }

    pub(super) async fn ensure_project_repository_ready(
        &self,
        project_id: ProjectId,
    ) -> Result<()> {
        self.sync_project_repository(
            project_id,
            projects::workspace::ProjectRepositorySyncTarget::DefaultBranch,
        )
        .await
        .map(|_| ())
    }

    pub(super) async fn restore_project_repositories(&self) {
        let projects = self.list_projects().await;
        for project in projects {
            if project.status != ProjectStatus::Ready
                || project.clone_status != ProjectCloneStatus::Ready
            {
                continue;
            }
            let result = async {
                self.sync_project_repository(
                    project.id,
                    projects::workspace::ProjectRepositorySyncTarget::DefaultBranch,
                )
                .await?;
                self.refresh_project_skill_cache_from_project_repository(project.id)
                    .await
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(
                    project_id = %project.id,
                    "failed to restore project repository during startup: {error}"
                );
            }
        }
    }

    pub(super) async fn sync_project_repository(
        &self,
        project_id: ProjectId,
        target: projects::workspace::ProjectRepositorySyncTarget,
    ) -> Result<projects::workspace::ProjectRepositoryRevision> {
        let project = self.project(project_id).await?;
        let _repo_sync_guard = project.repo_sync_lock.lock().await;
        let summary = project.summary.read().await.clone();
        let token = self.project_git_token(project_id).await?.ok_or_else(|| {
            RuntimeError::InvalidInput("project git account token is not configured".to_string())
        })?;
        self.ensure_project_cache_volume(project_id).await?;
        self.sync_project_repository_volume(&summary, &token, &target)
            .await
    }

    pub(super) async fn ensure_agent_workspace_volume(
        &self,
        agent_id: AgentId,
        volume: &str,
        role_override: Option<AgentRole>,
    ) -> Result<()> {
        let agent = self.agent(agent_id).await?;
        let summary = agent.summary.read().await.clone();
        let Some(project_id) = summary.project_id else {
            return Ok(());
        };
        let role = role_override.or(summary.role).unwrap_or_default();
        self.deps
            .docker
            .ensure_volume(
                volume,
                &[
                    ("mai.team.managed", "true".to_string()),
                    ("mai.team.kind", "agent-workspace".to_string()),
                    ("mai.team.project", project_id.to_string()),
                    ("mai.team.agent", agent_id.to_string()),
                    ("mai.team.role", role.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    pub(super) async fn ensure_project_cache_volume(&self, project_id: ProjectId) -> Result<()> {
        let volume = project_cache_volume(&project_id.to_string());
        self.deps
            .docker
            .ensure_volume(
                &volume,
                &[
                    ("mai.team.managed", "true".to_string()),
                    ("mai.team.kind", "project-cache".to_string()),
                    ("mai.team.project", project_id.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    pub(super) async fn sync_agent_workspace_volume_repo(
        &self,
        project: &ProjectSummary,
        agent_id: AgentId,
        token: &str,
    ) -> Result<()> {
        let project_record = self.project(project.id).await?;
        let _repo_sync_guard = project_record.repo_sync_lock.lock().await;
        self.sync_project_repository_volume(
            project,
            token,
            &projects::workspace::ProjectRepositorySyncTarget::DefaultBranch,
        )
        .await?;
        self.sync_agent_workspace_volume_from_cache_repo(project, agent_id, token)
            .await
    }

    pub(super) async fn sync_agent_workspace_volume_from_cache_repo(
        &self,
        project: &ProjectSummary,
        agent_id: AgentId,
        token: &str,
    ) -> Result<()> {
        let volume = project_agent_workspace_volume(&project.id.to_string(), &agent_id.to_string());
        let cache_volume = project_cache_volume(&project.id.to_string());
        self.ensure_agent_workspace_volume(agent_id, &volume, None)
            .await?;
        let repo_url = github::github_clone_url(&project.owner, &project.repo);
        let branch = format!("mai-agent/{agent_id}");
        let origin_branch = format!("origin/{}", project.branch);
        let command = format!(
            "set -eu\n{}{}\
             rm -rf /workspace/repo\n\
             mkdir -p /workspace /workspace/.mai/install-log /workspace/.mai/tool-state /workspace/tmp\n\
             git_with_retry clone --no-checkout -- /cache/repo.git /workspace/repo\n\
             cd /workspace/repo\n\
             git_with_retry fetch /cache/repo.git '+refs/pull/*/head:refs/remotes/origin/pr/*'\n\
             git remote set-url origin {}\n\
             git checkout -B {} {}",
            git_shell_credential_prelude(),
            git_shell_retry_function(),
            shell_quote_word(&repo_url),
            shell_quote_word(&branch),
            shell_quote_word(&origin_branch),
        );
        let env = [(GIT_TOKEN_ENV.to_string(), token.to_string())];
        let sidecar_name = format!("mai-tool-git-clone-{agent_id}");
        let cache_mounts = [(cache_volume.as_str(), "/cache")];
        let output = self
            .deps
            .docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: None,
                env: &env,
                workspace_volume: Some(&volume),
                mounts: &cache_mounts,
                timeout_secs: Some(600),
            })
            .await?;
        if output.status != 0 {
            let message = redact_secret(&format!("{}{}", output.stdout, output.stderr), token);
            return Err(RuntimeError::InvalidInput(format!(
                "project agent workspace volume clone failed: {message}"
            )));
        }
        Ok(())
    }

    pub(super) async fn sync_project_review_workspace_volume_from_repository(
        &self,
        project: &ProjectSummary,
        agent_id: AgentId,
        target: &projects::workspace::ProjectRepositoryReviewTarget,
        revision: &projects::workspace::ProjectRepositoryRevision,
        token: &str,
    ) -> Result<()> {
        let volume = project_agent_workspace_volume(&project.id.to_string(), &agent_id.to_string());
        let repository_volume = project_cache_volume(&project.id.to_string());
        self.ensure_agent_workspace_volume(agent_id, &volume, Some(AgentRole::Reviewer))
            .await?;
        let repo_url = github::github_clone_url(&project.owner, &project.repo);
        let base_ref = format!("refs/heads/{}", revision.branch);
        let remote_base_ref = format!("refs/remotes/origin/{}", revision.branch);
        let pull_ref = format!("refs/pull/{}/head", target.pr);
        let remote_pull_ref = format!("refs/remotes/origin/pr/{}", target.pr);
        let command = format!(
            r#"set -eu
{credential}{retry}rm -rf /workspace/repo
mkdir -p /workspace /workspace/.mai/install-log /workspace/.mai/tool-state /workspace/tmp
git_with_retry clone --no-checkout -- /project/repo.git /workspace/repo
cd /workspace/repo
git fetch /project/repo.git {base_fetch} {pull_fetch}
expected_origin={repo_url}
git remote set-url origin "$expected_origin"
expected_base={expected_base}
expected_head={expected_head}
actual_base=$(git rev-parse {remote_base_commit})
actual_head=$(git rev-parse {remote_pull_commit})
if [ "$actual_base" != "$expected_base" ]; then
  echo "reviewer base ref mismatch: expected $expected_base, found $actual_base" >&2
  exit 1
fi
if [ "$actual_head" != "$expected_head" ]; then
  echo "reviewer PR head mismatch: expected $expected_head, found $actual_head" >&2
  exit 1
fi
git checkout --detach "$expected_head"
git reset --hard "$expected_head"
git clean -fdx
test "$(git rev-parse HEAD)" = "$expected_head"
test "$(git remote get-url origin)" = "$expected_origin"
test -z "$(git status --porcelain)"
"#,
            credential = git_shell_credential_prelude(),
            retry = git_shell_retry_function(),
            base_fetch = shell_quote_word(&format!("+{base_ref}:{remote_base_ref}")),
            pull_fetch = shell_quote_word(&format!("+{pull_ref}:{remote_pull_ref}")),
            repo_url = shell_quote_word(&repo_url),
            remote_base_commit = shell_quote_word(&format!("{remote_base_ref}^{{commit}}")),
            remote_pull_commit = shell_quote_word(&format!("{remote_pull_ref}^{{commit}}")),
            expected_base = shell_quote_word(&revision.base_sha),
            expected_head = shell_quote_word(&target.head_sha),
        );
        let env = [(GIT_TOKEN_ENV.to_string(), token.to_string())];
        let sidecar_name = format!("mai-tool-review-clone-{agent_id}");
        let mounts = [(repository_volume.as_str(), "/project")];
        let output = self
            .deps
            .docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: None,
                env: &env,
                workspace_volume: Some(&volume),
                mounts: &mounts,
                timeout_secs: Some(600),
            })
            .await?;
        if output.status != 0 {
            let message = redact_secret(&format!("{}{}", output.stdout, output.stderr), token);
            return Err(RuntimeError::InvalidInput(format!(
                "project reviewer workspace preparation failed: {message}"
            )));
        }
        Ok(())
    }

    pub(super) async fn sync_project_repository_volume(
        &self,
        project: &ProjectSummary,
        token: &str,
        target: &projects::workspace::ProjectRepositorySyncTarget,
    ) -> Result<projects::workspace::ProjectRepositoryRevision> {
        let volume = project_cache_volume(&project.id.to_string());
        self.ensure_project_cache_volume(project.id).await?;
        let repo_url = github::github_clone_url(&project.owner, &project.repo);
        let branch_ref = format!("refs/heads/{}", project.branch);
        let default_fetch = format!("+{branch_ref}:{branch_ref}");
        let review_fetch = target
            .review()
            .map(|target| {
                let pull_ref = format!("refs/pull/{}/head", target.pr);
                format!(
                    "git_with_retry -C /workspace/repo.git fetch origin {}\n",
                    shell_quote_word(&format!("+{pull_ref}:{pull_ref}"))
                )
            })
            .unwrap_or_default();
        let review_resolution = target
            .review()
            .map(|target| {
                let pull_ref = format!("refs/pull/{}/head^{{commit}}", target.pr);
                let (_, head_marker) = projects::workspace::repository::sync_result_markers();
                format!(
                    "expected_head={}\n\
                     head_sha=$(git -C /workspace/repo.git rev-parse {})\n\
                     if [ \"$head_sha\" != \"$expected_head\" ]; then\n\
                       echo \"project PR ref mismatch: expected $expected_head, found $head_sha\" >&2\n\
                       exit 1\n\
                     fi\n\
                     printf '{}%s\\n' \"$head_sha\"\n",
                    shell_quote_word(&target.head_sha),
                    shell_quote_word(&pull_ref),
                    head_marker,
                )
            })
            .unwrap_or_default();
        let (base_marker, _) = projects::workspace::repository::sync_result_markers();
        let command = format!(
            r#"set -eu
{credential}{retry}mkdir -p /workspace
if [ -d /workspace/repo.git ] && [ "$(git -C /workspace/repo.git rev-parse --is-bare-repository 2>/dev/null || true)" = true ]; then
  git -C /workspace/repo.git remote set-url origin {repo_url}
else
  rm -rf /workspace/repo /workspace/repo.git
  git_with_retry clone --mirror -- {repo_url} /workspace/repo.git
fi
git_with_retry -C /workspace/repo.git fetch --prune origin {default_fetch}
{review_fetch}base_sha=$(git -C /workspace/repo.git rev-parse {base_commit})
{review_resolution}common_dir=$(git -C /workspace/repo rev-parse --git-common-dir 2>/dev/null || true)
if [ "$common_dir" = /workspace/repo.git ] && (
  git -C /workspace/repo reset --hard &&
  git -C /workspace/repo clean -fdx &&
  git -C /workspace/repo checkout --detach "$base_sha"
); then
  :
else
  rm -rf /workspace/repo
  git --git-dir=/workspace/repo.git worktree prune
  git --git-dir=/workspace/repo.git worktree add --detach /workspace/repo "$base_sha"
fi
git -C /workspace/repo reset --hard "$base_sha"
git -C /workspace/repo clean -fdx
test "$(git -C /workspace/repo rev-parse HEAD)" = "$base_sha"
test -z "$(git -C /workspace/repo status --porcelain)"
printf '{base_marker}%s\n' "$base_sha"
"#,
            credential = git_shell_credential_prelude(),
            retry = git_shell_retry_function(),
            repo_url = shell_quote_word(&repo_url),
            default_fetch = shell_quote_word(&default_fetch),
            base_commit = shell_quote_word(&format!("{branch_ref}^{{commit}}")),
        );
        let env = [(GIT_TOKEN_ENV.to_string(), token.to_string())];
        let sidecar_name = format!("mai-tool-git-cache-{}-{}", project.id, Uuid::new_v4());
        let output = self
            .deps
            .docker
            .run_sidecar_shell_env(&SidecarParams {
                name: &sidecar_name,
                image: &self.sidecar_image,
                command: &command,
                args: &[],
                cwd: None,
                env: &env,
                workspace_volume: Some(&volume),
                mounts: &[],
                timeout_secs: Some(600),
            })
            .await?;
        if output.status != 0 {
            let message = redact_secret(&format!("{}{}", output.stdout, output.stderr), token);
            return Err(RuntimeError::InvalidInput(format!(
                "project repository volume sync failed: {message}"
            )));
        }
        projects::workspace::repository::parse_sync_result(
            &project.branch,
            &output.stdout,
            target.review().map(|target| target.head_sha.as_str()),
        )
    }

    pub(super) async fn set_project_review_state(
        &self,
        project_id: ProjectId,
        status: ProjectReviewStatus,
        update: ReviewStateUpdate,
    ) -> Result<ProjectSummary> {
        projects::review::state::set_project_review_state(self, project_id, status, update).await
    }

    pub(super) async fn delete_project_workspace(&self, project_id: ProjectId) -> Result<()> {
        self.workspace_manager
            .delete_project_workspace(project_id)
            .await?;
        self.delete_project_agent_workspace_volumes(project_id)
            .await?;
        let cache_volume = project_cache_volume(&project_id.to_string());
        self.deps.docker.delete_volume(&cache_volume).await?;
        Ok(())
    }

    pub(super) async fn delete_project_agent_workspace_volumes(
        &self,
        project_id: ProjectId,
    ) -> Result<()> {
        let project_id_label = project_id.to_string();
        let volumes = self.deps.docker.list_managed_volumes().await?;
        for volume in volumes {
            if volume.kind.as_deref() == Some("agent-workspace")
                && volume.project_id.as_deref() == Some(project_id_label.as_str())
            {
                self.deps.docker.delete_volume(&volume.name).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn agent_resource_broker(
        &self,
        agent: &AgentRecord,
        agent_id: AgentId,
        cancellation_token: &CancellationToken,
    ) -> Result<AgentResourceBroker> {
        agents::agent_resource_broker(self, agent, agent_id, cancellation_token).await
    }
}
