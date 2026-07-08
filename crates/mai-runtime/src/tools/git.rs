use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use mai_docker::{DockerClient, SidecarParams, project_agent_workspace_volume};
use mai_protocol::{AgentId, ProjectId, ProjectSummary};
use pl_core::{
    ExecutionBackend, ExecutionOutput, ExecutionRequest, GIT_TOKEN_ENV, GitCredential,
    GitCredentialProvider, GitCredentialRequest, GitPolicy, GitTool, GitToolKind,
    GitWorkspaceConfig, PureError, Tool, ToolContext, ToolInput,
};
use serde_json::{Value, json};
#[cfg(test)]
use tokio::process::Command;

use crate::github::github_clone_url;
use crate::projects;
use crate::state::AgentRecord;
use crate::turn::tool_output::ToolExecution;
use crate::{AgentRuntime, Result, RuntimeError};

pub(crate) struct GitToolContext<'a> {
    pub(crate) backend: GitToolBackend<'a>,
    pub(crate) agent_id: AgentId,
    pub(crate) project: ProjectSummary,
    pub(crate) token: Option<String>,
}

pub(crate) enum GitToolBackend<'a> {
    Sidecar {
        docker: &'a DockerClient,
        sidecar_image: &'a str,
        workspace_volume: String,
        repo_path: &'a str,
    },
    #[cfg(test)]
    Host {
        git_binary: &'a str,
        projects_root: &'a std::path::Path,
    },
}

pub(crate) async fn execute_git_tool(
    context: GitToolContext<'_>,
    name: &str,
    arguments: Value,
) -> Result<ToolExecution> {
    #[cfg(test)]
    if let GitToolBackend::Host { projects_root, .. } = &context.backend {
        let clone = projects::workspace::agent_clone_path(
            projects_root,
            context.project.id,
            context.agent_id,
        );
        if !clone.exists() {
            return Err(RuntimeError::InvalidInput(
                "project git workspace is not available".to_string(),
            ));
        }
    }
    let kind = GitToolKind::from_name(name)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("unsupported git tool `{name}`")))?;
    let output = execute_pl_core_git_tool(&context, kind, arguments).await?;
    Ok(ToolExecution::new(true, output, false))
}

async fn execute_pl_core_git_tool(
    context: &GitToolContext<'_>,
    kind: GitToolKind,
    arguments: Value,
) -> Result<String> {
    let config = git_workspace_config(context);
    let tool = GitTool::new(
        kind,
        config.clone(),
        Arc::new(BorrowedGitExecutionBackend::new(
            &context.backend,
            context.agent_id,
        )),
        Arc::new(MaiGitCredentialProvider::Static {
            token: context.token.clone(),
        }),
    );
    let output = tool
        .execute(
            ToolInput {
                arguments,
                session_id: "mai-project-git".to_string(),
                tool_id: kind.name().to_string(),
                revision_base: 0,
            },
            pl_tool_context(config.worktree),
        )
        .await
        .map_err(runtime_error_from_pure)?;
    Ok(output.description)
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

fn git_workspace_config(context: &GitToolContext<'_>) -> GitWorkspaceConfig {
    let mut workspace_info = BTreeMap::new();
    workspace_info.insert("project_id".to_string(), json!(context.project.id));
    match &context.backend {
        GitToolBackend::Sidecar {
            workspace_volume,
            repo_path,
            ..
        } => {
            workspace_info.insert("workspace_volume".to_string(), json!(workspace_volume));
            let remote_url = github_clone_url(&context.project.owner, &context.project.repo);
            GitWorkspaceConfig {
                worktree: std::path::PathBuf::from(repo_path),
                git_binary: std::path::PathBuf::from("git"),
                policy: GitPolicy::new(context.project.branch.clone()),
                default_push_branch: Some(format!("mai-agent/{}", context.agent_id)),
                remote_url: Some(remote_url),
                workspace_info,
            }
        }
        #[cfg(test)]
        GitToolBackend::Host {
            git_binary,
            projects_root,
        } => {
            let repo_cache =
                projects::workspace::project_repo_cache_path(projects_root, context.project.id);
            let clone = projects::workspace::agent_clone_path(
                projects_root,
                context.project.id,
                context.agent_id,
            );
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
    }
}

fn pl_tool_context(workspace_root: std::path::PathBuf) -> ToolContext {
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel(8);
    ToolContext {
        event_tx,
        options: pl_core::TurnOptions::default(),
        workspace_access: pl_core::WorkspaceAccess::WorkspaceOnly,
        mode: pl_core::CompileMode::Auto,
        workspace_root,
        workspace_instructions: None,
        instruction_snapshot: None,
        provider_call_id: None,
        active_subagent: None,
        agent_supervisor: pl_core::AgentSupervisor::default(),
        agent_tool_registrar: None,
        lsp_runtime: None,
        parent_session: Arc::new(pl_core::CoreSession::new()),
    }
}

#[derive(Clone)]
pub(crate) enum MaiGitCredentialProvider {
    Static {
        token: Option<String>,
    },
    Project {
        runtime: Arc<AgentRuntime>,
        project_id: ProjectId,
    },
}

impl fmt::Debug for MaiGitCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
    fn credential(
        &self,
        _request: GitCredentialRequest,
    ) -> impl Future<Output = std::result::Result<Option<GitCredential>, PureError>> + Send {
        let provider = self.clone();
        async move {
            let token = match provider {
                MaiGitCredentialProvider::Static { token } => token,
                MaiGitCredentialProvider::Project {
                    runtime,
                    project_id,
                } => runtime
                    .project_git_token(project_id)
                    .await
                    .map_err(pure_error_from_runtime)?,
            };
            Ok(token
                .filter(|token| !token.trim().is_empty())
                .map(GitCredential::new))
        }
    }
}

struct BorrowedGitExecutionBackend<'a> {
    backend: &'a GitToolBackend<'a>,
    agent_id: AgentId,
}

impl<'a> BorrowedGitExecutionBackend<'a> {
    fn new(backend: &'a GitToolBackend<'a>, agent_id: AgentId) -> Self {
        Self { backend, agent_id }
    }
}

impl fmt::Debug for BorrowedGitExecutionBackend<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.backend {
            GitToolBackend::Sidecar { .. } => f.write_str("BorrowedGitExecutionBackend::Sidecar"),
            #[cfg(test)]
            GitToolBackend::Host { .. } => f.write_str("BorrowedGitExecutionBackend::Host"),
        }
    }
}

impl ExecutionBackend for BorrowedGitExecutionBackend<'_> {
    fn run(
        &self,
        request: ExecutionRequest,
    ) -> impl Future<Output = std::result::Result<ExecutionOutput, PureError>> + Send {
        async move {
            match self.backend {
                GitToolBackend::Sidecar {
                    docker,
                    sidecar_image,
                    workspace_volume,
                    repo_path,
                } => run_sidecar_git_output(
                    docker,
                    sidecar_image,
                    workspace_volume,
                    repo_path,
                    self.agent_id,
                    request.env.get(GIT_TOKEN_ENV).map(String::as_str),
                    &request.args,
                )
                .await
                .map_err(pure_error_from_runtime),
                #[cfg(test)]
                GitToolBackend::Host { .. } => run_host_git_request(request).await,
            }
        }
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
    async fn run(
        &self,
        request: ExecutionRequest,
    ) -> std::result::Result<ExecutionOutput, PureError> {
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
        .map_err(pure_error_from_runtime)
    }
}

#[cfg(test)]
async fn run_host_git_request(
    request: ExecutionRequest,
) -> std::result::Result<ExecutionOutput, PureError> {
    let mut command = Command::new(&request.program);
    command.current_dir(&request.cwd).args(&request.args);
    apply_host_git_safety_environment(&mut command, &request.cwd);
    command.envs(&request.env);
    let output = match request.timeout {
        Some(timeout) => tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| pure_tool_error("git", "git command timed out"))?,
        None => command.output().await,
    }
    .map_err(|error| pure_tool_error("git", format!("failed to run git: {error}")))?;
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
        .env_remove("MAI_GITHUB_INSTALLATION_TOKEN");
}

fn runtime_error_from_pure(error: PureError) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}

fn pure_error_from_runtime(error: RuntimeError) -> PureError {
    pure_tool_error("git", error)
}

fn pure_tool_error(tool: &str, error: impl fmt::Display) -> PureError {
    PureError::ToolExecutionFailed {
        tool: tool.to_string(),
        error: error.to_string(),
    }
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
    let command = sidecar_git_command(repo_path, token.is_some(), &args);
    let env = token
        .map(|token| {
            vec![(
                "MAI_GITHUB_INSTALLATION_TOKEN".to_string(),
                token.to_string(),
            )]
        })
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

fn sidecar_git_command(repo_path: &str, with_token: bool, args: &[&str]) -> String {
    let mut command_parts = vec![
        "git".to_string(),
        "-c".to_string(),
        shell_quote("core.hooksPath=/dev/null"),
        "-c".to_string(),
        shell_quote(&format!("safe.directory={repo_path}")),
        "-c".to_string(),
        shell_quote("credential.helper="),
    ];
    if with_token {
        let git_command = command_parts
            .iter()
            .chain(
                args.iter()
                    .map(|arg| shell_quote(arg))
                    .collect::<Vec<_>>()
                    .iter(),
            )
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        return sidecar_git_command_with_askpass(&git_command);
    }
    command_parts.extend(args.iter().map(|arg| shell_quote(arg)));
    command_parts.join(" ")
}

fn sidecar_git_command_with_askpass(git_command: &str) -> String {
    format!(
        "askpass=$(mktemp) && cat > \"$askpass\" <<'MAI_GIT_ASKPASS'\n#!/bin/sh\n{}\nMAI_GIT_ASKPASS\nchmod 700 \"$askpass\" && GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=\"$askpass\" {git_command}; status=$?; rm -f \"$askpass\"; exit $status",
        git_askpass_script()
    )
}

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

fn git_askpass_script() -> String {
    "case \"$1\" in\n  *Username*) printf '%s\\n' x-access-token ;;\n  *Password*) printf '%s\\n' \"$MAI_GITHUB_INSTALLATION_TOKEN\" ;;\n  *) printf '\\n' ;;\nesac"
        .to_string()
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
    fn git_askpass_script_uses_installation_token_for_password_prompt() {
        let script = git_askpass_script();

        assert!(script.contains("x-access-token"));
        assert!(script.contains("$MAI_GITHUB_INSTALLATION_TOKEN"));
    }

    #[test]
    fn sidecar_git_command_uses_askpass_instead_of_literal_extraheader() {
        let command = sidecar_git_command("/workspace/repo", true, &["fetch", "origin"]);

        assert!(command.contains("GIT_ASKPASS="));
        assert!(command.contains("x-access-token"));
        assert!(command.contains("$MAI_GITHUB_INSTALLATION_TOKEN"));
        assert!(!command.contains("extraheader"));
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
