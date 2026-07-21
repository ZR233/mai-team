use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use mai_docker::{DockerClient, SidecarParams, project_agent_workspace_volume};
#[cfg(test)]
use mai_protocol::ProjectSummary;
use mai_protocol::{AgentId, ProjectId};
#[cfg(test)]
use pl_core::ToolCapabilityConfig;
use pl_core::{
    ExecutionBackend, ExecutionOutput, ExecutionRequest, GIT_TOKEN_ENV, GitCredential,
    GitCredentialProvider, GitCredentialRequest, GitPolicy, GitShellCommandRequest,
    GitShellCredential, GitToolKind, GitWorkspaceConfig, git_shell_command,
};
#[cfg(test)]
use serde_json::Value;
use serde_json::json;
#[cfg(test)]
use tokio::process::Command;

use crate::github::github_clone_url;
use crate::projects;
use crate::state::AgentRecord;
#[cfg(test)]
use crate::turn::tool_output::ToolExecution;
use crate::{AgentRuntime, Result, RuntimeError};

#[cfg(test)]
pub(crate) struct GitToolContext<'a> {
    pub(crate) backend: GitToolBackend<'a>,
    pub(crate) agent_id: AgentId,
    pub(crate) project: ProjectSummary,
    pub(crate) token: Option<String>,
}

#[cfg(test)]
pub(crate) enum GitToolBackend<'a> {
    Host {
        git_binary: &'a str,
        projects_root: &'a std::path::Path,
    },
}

#[cfg(test)]
pub(crate) async fn execute_git_tool(
    context: GitToolContext<'_>,
    name: &str,
    arguments: Value,
) -> Result<ToolExecution> {
    let GitToolBackend::Host { projects_root, .. } = &context.backend;
    let clone =
        projects::workspace::agent_clone_path(projects_root, context.project.id, context.agent_id);
    if !clone.exists() {
        return Err(RuntimeError::InvalidInput(
            "project git workspace is not available".to_string(),
        ));
    }
    let kind = GitToolKind::from_name(name)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("unsupported git tool `{name}`")))?;
    let output = execute_git_tool_via_registry(&context, kind, arguments).await?;
    Ok(ToolExecution::success(output))
}

#[cfg(test)]
async fn execute_git_tool_via_registry(
    context: &GitToolContext<'_>,
    kind: GitToolKind,
    arguments: Value,
) -> Result<String> {
    let config = git_workspace_config(context);
    let workspace_root = config.worktree.clone();
    let tool_set =
        pl_core::ToolSetBuilder::from_capabilities(ToolCapabilityConfig::git_workspace())
            .with_allowed_tools([kind.name()])
            .with_git_tools(
                config,
                Arc::new(ProjectGitExecutionBackend),
                Arc::new(MaiGitCredentialProvider::Static {
                    token: context.token.clone(),
                }),
            );
    let kernel = pl_core::AgentKernel::builder(
        pl_core::TurnEngineBuilder::from_provider_info(pl_model::ProviderInfo::deepseek(None))
            .map_err(runtime_error_from_pure)?,
    )
    .with_profile(pl_core::CoreAgentProfile::host_provided(
        workspace_root.clone(),
    ))
    .with_tool_set(tool_set)
    .build()
    .await;
    let tool = kernel.tool(kind.name()).ok_or_else(|| {
        RuntimeError::InvalidInput(format!("git tool `{}` was not registered", kind.name()))
    })?;
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel(8);
    let output = kernel
        .execute_tool(pl_core::AgentKernelToolRequest::new(
            tool.name(),
            arguments,
            "mai-project-git",
            kind.name(),
            event_tx,
        ))
        .await
        .map_err(runtime_error_from_pure)?;
    Ok(output.into_model_output())
}

pub(crate) struct NativeGitToolRuntime {
    pub(crate) config: GitWorkspaceConfig,
    pub(crate) backend: Arc<MaiGitExecutionBackend>,
    pub(crate) credential_provider: Arc<MaiGitCredentialProvider>,
}

pub(crate) async fn native_git_tool_runtime(
    runtime: Arc<AgentRuntime>,
    agent: &AgentRecord,
    visible_tool: impl Fn(&str) -> bool,
) -> Result<Option<NativeGitToolRuntime>> {
    let summary = agent.summary.read().await.clone();
    let Some(project_id) = summary.project_id else {
        return Ok(None);
    };
    if !GitToolKind::all()
        .iter()
        .any(|kind| visible_tool(kind.name()))
    {
        return Ok(None);
    }
    let project = runtime.project(project_id).await?;
    let project_summary = project.summary.read().await.clone();
    let workspace_volume =
        project_agent_workspace_volume(&project_id.to_string(), &summary.id.to_string());
    let mut workspace_info = BTreeMap::new();
    workspace_info.insert("project_id".to_string(), json!(project_id));
    workspace_info.insert("workspace_volume".to_string(), json!(workspace_volume));
    let remote_url = github_clone_url(&project_summary.owner, &project_summary.repo);
    let config = GitWorkspaceConfig {
        worktree: std::path::PathBuf::from(projects::workspace::AGENT_WORKSPACE_REPO_PATH),
        git_binary: std::path::PathBuf::from("git"),
        policy: GitPolicy::new(project_summary.branch),
        default_push_branch: Some(format!("mai-agent/{}", summary.id)),
        remote_url: Some(remote_url),
        workspace_info,
    };
    let backend = Arc::new(MaiGitExecutionBackend {
        docker: runtime.deps.docker.clone(),
        sidecar_image: runtime.sidecar_image.clone(),
        workspace_volume,
        repo_path: projects::workspace::AGENT_WORKSPACE_REPO_PATH.to_string(),
        agent_id: summary.id,
    });
    let credential_provider = Arc::new(MaiGitCredentialProvider::Project {
        runtime,
        project_id,
    });
    Ok(Some(NativeGitToolRuntime {
        config,
        backend,
        credential_provider,
    }))
}

#[cfg(test)]
fn git_workspace_config(context: &GitToolContext<'_>) -> GitWorkspaceConfig {
    let mut workspace_info = BTreeMap::new();
    workspace_info.insert("project_id".to_string(), json!(context.project.id));
    let GitToolBackend::Host {
        git_binary,
        projects_root,
    } = &context.backend;
    let repo_cache =
        projects::workspace::project_repo_cache_path(projects_root, context.project.id);
    let clone =
        projects::workspace::agent_clone_path(projects_root, context.project.id, context.agent_id);
    workspace_info.insert("repo_cache".to_string(), json!(repo_cache));
    GitWorkspaceConfig {
        worktree: clone,
        git_binary: std::path::PathBuf::from(git_binary),
        policy: GitPolicy::new(context.project.branch.clone()),
        default_push_branch: Some(format!("mai-agent/{}", context.agent_id)),
        remote_url: Some(github_clone_url(
            &context.project.owner,
            &context.project.repo,
        )),
        workspace_info,
    }
}

#[derive(Clone)]
pub(crate) enum MaiGitCredentialProvider {
    #[cfg(test)]
    Static { token: Option<String> },
    Project {
        runtime: Arc<AgentRuntime>,
        project_id: ProjectId,
    },
}

impl fmt::Debug for MaiGitCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(test)]
            Self::Static { token } => f
                .debug_struct("MaiGitCredentialProvider::Static")
                .field(
                    "has_token",
                    &token.as_ref().is_some_and(|value| !value.trim().is_empty()),
                )
                .finish(),
            Self::Project { project_id, .. } => f
                .debug_struct("MaiGitCredentialProvider::Project")
                .field("project_id", project_id)
                .finish(),
        }
    }
}

impl GitCredentialProvider for MaiGitCredentialProvider {
    type Error = RuntimeError;

    fn credential(
        &self,
        _request: GitCredentialRequest,
    ) -> impl Future<Output = Result<Option<GitCredential>>> + Send {
        let provider = self.clone();
        async move {
            let token = match provider {
                #[cfg(test)]
                MaiGitCredentialProvider::Static { token } => token,
                MaiGitCredentialProvider::Project {
                    runtime,
                    project_id,
                } => runtime.project_git_token(project_id).await?,
            };
            Ok(token
                .filter(|token| !token.trim().is_empty())
                .map(GitCredential::new))
        }
    }
}

#[cfg(test)]
struct ProjectGitExecutionBackend;

#[cfg(test)]
impl fmt::Debug for ProjectGitExecutionBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProjectGitExecutionBackend::Host")
    }
}

#[cfg(test)]
impl ExecutionBackend for ProjectGitExecutionBackend {
    type Error = RuntimeError;

    async fn run(&self, request: ExecutionRequest) -> Result<ExecutionOutput> {
        run_host_git_request(request).await
    }
}

#[derive(Clone)]
pub(crate) struct MaiGitExecutionBackend {
    docker: DockerClient,
    sidecar_image: String,
    workspace_volume: String,
    repo_path: String,
    agent_id: AgentId,
}

impl fmt::Debug for MaiGitExecutionBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaiGitExecutionBackend")
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

impl ExecutionBackend for MaiGitExecutionBackend {
    type Error = RuntimeError;

    async fn run(&self, request: ExecutionRequest) -> Result<ExecutionOutput> {
        run_sidecar_git_output(
            &self.docker,
            &self.sidecar_image,
            &self.workspace_volume,
            &self.repo_path,
            self.agent_id,
            request.env.get(GIT_TOKEN_ENV).map(String::as_str),
            &request.args,
        )
        .await
    }
}

#[cfg(test)]
async fn run_host_git_request(request: ExecutionRequest) -> Result<ExecutionOutput> {
    let mut command = Command::new(&request.program);
    command.current_dir(&request.cwd).args(&request.args);
    apply_host_git_safety_environment(&mut command, &request.cwd);
    command.envs(&request.env);
    let output = match request.timeout {
        Some(timeout) => tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| RuntimeError::InvalidInput("git command timed out".to_string()))?,
        None => command.output().await,
    }?;
    Ok(ExecutionOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

#[cfg(test)]
fn apply_host_git_safety_environment(command: &mut Command, cwd: &std::path::Path) {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "safe.directory")
        .env("GIT_CONFIG_VALUE_1", cwd)
        .env("GIT_CONFIG_KEY_2", "credential.helper")
        .env("GIT_CONFIG_VALUE_2", "")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove(GIT_TOKEN_ENV);
}

#[cfg(test)]
fn runtime_error_from_pure(error: pl_core::PureError) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}

async fn run_sidecar_git_output(
    docker: &DockerClient,
    sidecar_image: &str,
    workspace_volume: &str,
    repo_path: &str,
    agent_id: AgentId,
    token: Option<&str>,
    args: &[String],
) -> Result<ExecutionOutput> {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let credential = match token {
        Some(_) => GitShellCredential::EnvToken,
        None => GitShellCredential::Disabled,
    };
    let command = git_shell_command(GitShellCommandRequest {
        safe_directory: repo_path,
        args: &args,
        credential,
    });
    let env = token
        .map(|token| vec![(GIT_TOKEN_ENV.to_string(), token.to_string())])
        .unwrap_or_default();
    let sidecar_name = format!("mai-tool-git-{agent_id}-{}", uuid::Uuid::new_v4());
    let output = docker
        .run_sidecar_shell_env(&SidecarParams {
            name: &sidecar_name,
            image: sidecar_image,
            command: &command,
            args: &[],
            cwd: Some(repo_path),
            env: &env,
            workspace_volume: Some(workspace_volume),
            mounts: &[],
            timeout_secs: Some(600),
        })
        .await?;
    Ok(ExecutionOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
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

    #[test]
    fn project_git_tool_uses_pl_core_tool_set_registry() {
        let source = include_str!("git.rs");
        let start = source
            .find("pub(crate) async fn execute_git_tool")
            .expect("execute_git_tool");
        let end = source
            .find("pub(crate) struct NativeGitToolRuntime")
            .expect("native git runtime");
        let execute_path = &source[start..end];

        assert!(
            execute_path.contains("ToolSetBuilder::from_capabilities"),
            "project git tools must be registered through pl-core ToolSetBuilder"
        );
        assert!(
            execute_path.contains("ToolCapabilityConfig::git_workspace()"),
            "project git tools must reuse the pl-core git workspace capability preset"
        );
        assert!(
            !execute_path.contains("ToolCapabilityConfig {"),
            "project git tools must not hand-write a shared tool capability matrix"
        );
        assert!(
            execute_path.contains(".with_tool_set("),
            "project git tools must register their shared tool set through AgentKernelBuilder"
        );
        assert!(
            execute_path.contains(".execute_tool("),
            "project git tools must execute through AgentKernel::execute_tool"
        );
        assert!(
            !execute_path.contains("GitTool::new"),
            "project git tools must not bypass the pl-core tool registry"
        );
        for forbidden in [
            format!("{}{}", "Tool", "Context {"),
            format!("{}{}", "Tool", "Input {"),
            format!("{}{}", ".register", "(kernel.core_mut"),
            "output.description".to_string(),
        ] {
            assert!(
                !execute_path.contains(&forbidden),
                "project git tools must not assemble `{forbidden}` locally"
            );
        }
        assert!(
            !source.contains(&format!("{}{}", "execute_pl_core", "_git_tool")),
            "project git tools should not keep a direct GitTool execution helper"
        );
        assert!(
            !source.contains(&format!("{}{}", "GitToolBackend::", "Sidecar")),
            "sidecar git execution must use MaiGitExecutionBackend through pl-core ToolSetBuilder"
        );
    }

    #[test]
    fn git_backends_delegate_tool_error_shape_to_pl_core() {
        let source = include_str!("git.rs");
        let credential_impl = source_snippet(
            source,
            "impl GitCredentialProvider for MaiGitCredentialProvider",
            "#[cfg(test)]\nstruct ProjectGitExecutionBackend",
        );
        let host_backend_impl = source_snippet(
            source,
            "impl ExecutionBackend for ProjectGitExecutionBackend",
            "#[derive(Clone)]\npub(crate) struct MaiGitExecutionBackend",
        );
        let sidecar_backend_impl = source_snippet(
            source,
            "impl ExecutionBackend for MaiGitExecutionBackend",
            "#[cfg(test)]\nasync fn run_host_git_request",
        );

        for snippet in [credential_impl, host_backend_impl, sidecar_backend_impl] {
            assert!(
                !snippet.contains(&format!("{}{}", "ToolExecution", "Failed")),
                "git adapter 不应手动构造 pl-core 工具错误"
            );
            assert!(
                !snippet.contains(&format!("{}{}", "Pure", "Error")),
                "git adapter 不应依赖 pl 协议错误类型"
            );
            assert!(
                !snippet.contains("pure_error_from_runtime"),
                "git adapter 不应把 RuntimeError 包装回 pl 协议错误"
            );
        }
    }

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
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            pl_core::TOOL_GIT_STATUS,
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
    async fn git_workspace_info_returns_clone_oriented_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        let execution = execute_git_tool(
            GitToolContext {
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            pl_core::TOOL_GIT_WORKSPACE_INFO,
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
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            pl_core::TOOL_GIT_WORKSPACE_INFO,
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
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            pl_core::TOOL_GIT_DIFF,
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
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            pl_core::TOOL_GIT_BRANCH,
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
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            pl_core::TOOL_GIT_FETCH,
            json!({ "remote": "upstream" }),
        )
        .await
        .expect_err("non-origin remote rejected");

        assert!(err.to_string().contains("unsupported git remote"));
        assert_eq!(read_git_log(dir.path()), "");
    }

    #[tokio::test]
    async fn git_fetch_allows_pull_request_refspec_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path(dir.path());
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        execute_git_tool(
            GitToolContext {
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            pl_core::TOOL_GIT_FETCH,
            json!({ "refspec": "pull/679/head:refs/pull/679/head" }),
        )
        .await
        .expect("fetch pull request refspec");

        assert_eq!(
            read_git_log(dir.path()),
            format!(
                "{}|fetch --prune origin pull/679/head:refs/pull/679/head\n",
                clone_path.to_string_lossy()
            )
        );
    }

    #[test]
    fn sidecar_git_command_delegates_to_pl_core_shell_helper() {
        let source = include_str!("git.rs");
        let production =
            source_snippet(source, "async fn run_sidecar_git_output", "\n#[cfg(test)]");

        assert!(production.contains("git_shell_command"));
        assert!(production.contains("GitShellCommandRequest"));
        assert!(!production.contains("fn git_askpass_script"));
        assert!(!production.contains("sidecar_git_command_with_askpass"));
        assert!(!production.contains("MAI_GITHUB_INSTALLATION_TOKEN"));
        assert!(!production.contains("shell_words::quote"));
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
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: None,
            },
            pl_core::TOOL_GIT_COMMIT,
            json!({ "message": "save work" }),
        )
        .await
        .expect("commit");
        execute_git_tool(
            GitToolContext {
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            pl_core::TOOL_GIT_PUSH,
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
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            pl_core::TOOL_GIT_SYNC_DEFAULT_BRANCH,
            json!({}),
        )
        .await
        .expect_err("dirty sync rejected");

        assert!(err.to_string().contains("uncommitted changes"));
    }

    #[tokio::test]
    async fn git_sync_default_branch_uses_pl_core_camel_case_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = fake_git_path_with_status(dir.path(), " M README.md\n");
        let project_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let clone_path = workspace::agent_clone_path(dir.path(), project_id, agent_id);
        std::fs::create_dir_all(&clone_path).expect("clone");

        let execution = execute_git_tool(
            GitToolContext {
                backend: GitToolBackend::Host {
                    git_binary: &git,
                    projects_root: dir.path(),
                },
                agent_id,
                project: test_project(project_id, agent_id),
                token: Some("secret-token".to_string()),
            },
            pl_core::TOOL_GIT_SYNC_DEFAULT_BRANCH,
            json!({ "preserveChanges": true }),
        )
        .await
        .expect("sync default branch");

        let payload: Value = serde_json::from_str(&execution.output).expect("sync payload");
        assert_eq!(payload["preservedChanges"], json!(true));
        let git_log = read_git_log(dir.path());
        assert!(git_log.contains("stash push -u -m pl-core sync default branch"));
        assert!(git_log.contains("stash pop"));
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

    fn source_snippet<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let start = source.find(start).expect("snippet start");
        let end = source[start..]
            .find(end)
            .map(|offset| start + offset)
            .expect("snippet end");
        &source[start..end]
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
