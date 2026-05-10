use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

const MANAGED_LABEL: &str = "mai.team.managed=true";
const AGENT_LABEL_KEY: &str = "mai.team.agent";
const PROJECT_LABEL_KEY: &str = "mai.team.project";
const SIDECAR_LABEL_KEY: &str = "mai.team.sidecar";
const SIDECAR_KIND_LABEL_KEY: &str = "mai.team.sidecar.kind";
const PROJECT_SIDECAR_KIND: &str = "project";

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker is not available: {0}")]
    NotAvailable(String),
    #[error("docker command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("utf8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid docker image: {0}")]
    InvalidImage(String),
    #[error("docker command cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, DockerError>;

#[derive(Debug, Clone)]
pub struct DockerClient {
    binary: String,
    image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerHandle {
    pub id: String,
    pub name: String,
    pub image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub agent_id: Option<String>,
    pub project_id: Option<String>,
    pub sidecar: bool,
    pub sidecar_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct SidecarParams<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub command: &'a str,
    pub args: &'a [String],
    pub cwd: Option<&'a str>,
    pub env: &'a [(String, String)],
    pub workspace_volume: Option<&'a str>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerCreateOptions {
    pub memory: Option<String>,
    pub cpus: Option<String>,
    pub pids_limit: Option<u32>,
    pub cap_drop_all: bool,
    pub no_new_privileges: bool,
    pub network: Option<String>,
}

impl DockerClient {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            binary: "docker".to_string(),
            image: image.into(),
        }
    }

    pub fn new_with_binary(image: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            image: image.into(),
        }
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn workspace_volume_for_agent(agent_id: &str) -> String {
        agent_workspace_volume(agent_id)
    }

    pub fn workspace_volume_for_project(project_id: &str) -> String {
        project_workspace_volume(project_id)
    }

    pub fn workspace_volume_for_project_review(project_id: &str) -> String {
        project_review_workspace_volume(project_id)
    }

    pub async fn check_available(&self) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(["version", "--format", "{{.Server.Version}}"])
            .output()
            .await
            .map_err(|err| DockerError::NotAvailable(err.to_string()))?;
        if !output.status.success() {
            return Err(DockerError::NotAvailable(stderr_or_stdout(&output)));
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    pub async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>> {
        let output = Command::new(&self.binary)
            .args(["ps", "-aq", "--filter", &format!("label={MANAGED_LABEL}")])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }

        let ids = String::from_utf8(output.stdout)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let inspect = Command::new(&self.binary)
            .arg("inspect")
            .args(&ids)
            .output()
            .await?;
        if !inspect.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&inspect)));
        }

        managed_containers_from_inspect(&String::from_utf8(inspect.stdout)?)
    }

    pub async fn cleanup_orphaned_agent_containers(
        &self,
        active_agent_ids: &HashSet<String>,
    ) -> Result<Vec<String>> {
        self.cleanup_orphaned_managed_containers(active_agent_ids, &HashSet::new())
            .await
    }

    pub async fn cleanup_orphaned_managed_containers(
        &self,
        active_agent_ids: &HashSet<String>,
        active_project_ids: &HashSet<String>,
    ) -> Result<Vec<String>> {
        let containers = self.list_managed_containers().await?;
        let ids = orphaned_container_ids(&containers, active_agent_ids, active_project_ids);

        for id in &ids {
            self.delete_container(id).await?;
        }

        Ok(ids)
    }

    pub async fn cleanup_stale_containers(&self) -> Result<Vec<String>> {
        self.cleanup_orphaned_agent_containers(&HashSet::new())
            .await
    }

    pub async fn ensure_agent_container(
        &self,
        agent_id: &str,
        preferred_container_id: Option<&str>,
    ) -> Result<ContainerHandle> {
        self.ensure_agent_container_from_image(agent_id, preferred_container_id, &self.image)
            .await
    }

    pub async fn ensure_agent_container_from_image(
        &self,
        agent_id: &str,
        preferred_container_id: Option<&str>,
        image: &str,
    ) -> Result<ContainerHandle> {
        let image = validate_image(image)?;
        if let Some(container) = self
            .reusable_agent_container(agent_id, preferred_container_id)
            .await?
        {
            return self.prepare_existing_container(container).await;
        }

        self.create_agent_container_from_image(agent_id, image)
            .await
    }

    pub async fn create_agent_container(&self, agent_id: &str) -> Result<ContainerHandle> {
        self.create_agent_container_from_image(agent_id, &self.image)
            .await
    }

    pub async fn create_agent_container_from_parent(
        &self,
        agent_id: &str,
        parent_container_id: &str,
    ) -> Result<ContainerHandle> {
        self.create_agent_container_from_parent_with_workspace(agent_id, parent_container_id, None)
            .await
    }

    pub async fn create_agent_container_from_parent_with_workspace(
        &self,
        agent_id: &str,
        parent_container_id: &str,
        workspace_volume: Option<&str>,
    ) -> Result<ContainerHandle> {
        let image = snapshot_image_name(agent_id);
        let commit = Command::new(&self.binary)
            .args(["commit", parent_container_id, &image])
            .output()
            .await?;
        if !commit.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&commit)));
        }

        let result = self
            .create_agent_container_from_image_with_workspace(agent_id, &image, workspace_volume)
            .await;
        if let Err(err) = self.delete_image(&image).await {
            tracing::warn!(image = %image, "failed to remove temporary snapshot image: {err}");
        }
        result
    }

    async fn create_agent_container_from_image(
        &self,
        agent_id: &str,
        image: &str,
    ) -> Result<ContainerHandle> {
        self.create_agent_container_from_image_with_workspace(agent_id, image, None)
            .await
    }

    async fn create_agent_container_from_image_with_workspace(
        &self,
        agent_id: &str,
        image: &str,
        workspace_volume: Option<&str>,
    ) -> Result<ContainerHandle> {
        let image = validate_image(image)?;
        let name = agent_container_name(agent_id);
        let label = agent_label(agent_id);
        let default_workspace_volume;
        let workspace_volume = match workspace_volume {
            Some(value) => value,
            None => {
                default_workspace_volume = agent_workspace_volume(agent_id);
                &default_workspace_volume
            }
        };
        let args = create_agent_container_args(&name, &label, image, &workspace_volume);
        let create = Command::new(&self.binary)
            .args(args.iter().map(String::as_str))
            .output()
            .await?;
        if !create.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&create)));
        }
        let id = String::from_utf8(create.stdout)?.trim().to_string();

        if let Err(err) = self.start_container(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(err);
        }

        if let Err(err) = self.ensure_workspace(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(err);
        }

        Ok(ContainerHandle {
            id,
            name,
            image: image.to_string(),
        })
    }

    pub async fn ensure_project_sidecar_container(
        &self,
        project_id: &str,
        preferred_container_id: Option<&str>,
        image: &str,
        workspace_volume: &str,
        options: &ContainerCreateOptions,
    ) -> Result<ContainerHandle> {
        let image = validate_image(image)?;
        if let Some(container) = self
            .reusable_project_sidecar_container(project_id, preferred_container_id)
            .await?
        {
            return self.prepare_existing_container(container).await;
        }

        self.create_project_sidecar_container(project_id, image, workspace_volume, options)
            .await
    }

    async fn create_project_sidecar_container(
        &self,
        project_id: &str,
        image: &str,
        workspace_volume: &str,
        options: &ContainerCreateOptions,
    ) -> Result<ContainerHandle> {
        let image = validate_image(image)?;
        let name = project_sidecar_container_name(project_id);
        let args = create_project_sidecar_container_args(
            &name,
            project_id,
            image,
            workspace_volume,
            options,
        );
        let create = Command::new(&self.binary)
            .args(args.iter().map(String::as_str))
            .output()
            .await?;
        if !create.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&create)));
        }
        let id = String::from_utf8(create.stdout)?.trim().to_string();

        if let Err(err) = self.start_container(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(err);
        }

        if let Err(err) = self.ensure_workspace(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(err);
        }

        Ok(ContainerHandle {
            id,
            name,
            image: image.to_string(),
        })
    }

    pub async fn delete_agent_containers(
        &self,
        agent_id: &str,
        preferred_container_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let containers = self.list_managed_containers().await?;
        let ids = agent_container_delete_ids(&containers, agent_id, preferred_container_id);
        for id in &ids {
            self.delete_container(id).await?;
        }
        Ok(ids)
    }

    pub async fn delete_project_sidecar_containers(
        &self,
        project_id: &str,
        preferred_container_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let containers = self.list_managed_containers().await?;
        let ids =
            project_sidecar_container_delete_ids(&containers, project_id, preferred_container_id);
        for id in &ids {
            self.delete_container(id).await?;
        }
        Ok(ids)
    }

    pub async fn delete_container(&self, container_id: &str) -> Result<()> {
        let output = Command::new(&self.binary)
            .args(["rm", "-f", container_id])
            .output()
            .await?;
        if !output.status.success() {
            let message = stderr_or_stdout(&output);
            if is_missing_container_error(&message) {
                return Ok(());
            }
            return Err(DockerError::CommandFailed(message));
        }
        Ok(())
    }

    async fn delete_image(&self, image: &str) -> Result<()> {
        let output = Command::new(&self.binary)
            .args(["rmi", "-f", image])
            .output()
            .await?;
        if !output.status.success() {
            let message = stderr_or_stdout(&output);
            if is_missing_image_error(&message) {
                return Ok(());
            }
            return Err(DockerError::CommandFailed(message));
        }
        Ok(())
    }

    pub async fn delete_volume(&self, volume: &str) -> Result<()> {
        let output = Command::new(&self.binary)
            .args(["volume", "rm", "-f", volume])
            .output()
            .await?;
        if !output.status.success() {
            let message = stderr_or_stdout(&output);
            if message.to_ascii_lowercase().contains("no such volume") {
                return Ok(());
            }
            return Err(DockerError::CommandFailed(message));
        }
        Ok(())
    }

    async fn reusable_agent_container(
        &self,
        agent_id: &str,
        preferred_container_id: Option<&str>,
    ) -> Result<Option<ManagedContainer>> {
        let containers = self.list_managed_containers().await?;
        Ok(find_reusable_agent_container(&containers, agent_id, preferred_container_id).cloned())
    }

    async fn reusable_project_sidecar_container(
        &self,
        project_id: &str,
        preferred_container_id: Option<&str>,
    ) -> Result<Option<ManagedContainer>> {
        let containers = self.list_managed_containers().await?;
        Ok(
            find_reusable_project_sidecar_container(
                &containers,
                project_id,
                preferred_container_id,
            )
            .cloned(),
        )
    }

    async fn prepare_existing_container(
        &self,
        container: ManagedContainer,
    ) -> Result<ContainerHandle> {
        if container.state != "running" {
            self.start_container(&container.id).await?;
        }
        self.ensure_workspace(&container.id).await?;
        Ok(ContainerHandle {
            id: container.id,
            name: container.name,
            image: container.image,
        })
    }

    async fn start_container(&self, container_id: &str) -> Result<()> {
        let output = Command::new(&self.binary)
            .args(["start", container_id])
            .output()
            .await?;
        if !output.status.success() {
            let message = stderr_or_stdout(&output);
            if message.to_ascii_lowercase().contains("already running") {
                return Ok(());
            }
            return Err(DockerError::CommandFailed(message));
        }
        Ok(())
    }

    async fn ensure_workspace(&self, container_id: &str) -> Result<()> {
        let mkdir = self
            .exec_shell(container_id, "mkdir -p /workspace", Some("/"), Some(10))
            .await?;
        if mkdir.status != 0 {
            return Err(DockerError::CommandFailed(format!(
                "failed to initialize /workspace: {}",
                mkdir.stderr
            )));
        }
        Ok(())
    }

    pub async fn exec_shell(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        self.exec_shell_env(container_id, command, cwd, timeout_secs, &[])
            .await
    }

    pub async fn exec_shell_env(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
        env: &[(String, String)],
    ) -> Result<ExecOutput> {
        self.exec_shell_env_with_cancel(
            container_id,
            command,
            cwd,
            timeout_secs,
            env,
            &CancellationToken::new(),
        )
        .await
    }

    pub async fn exec_shell_with_cancel(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecOutput> {
        self.exec_shell_env_with_cancel(
            container_id,
            command,
            cwd,
            timeout_secs,
            &[],
            cancellation_token,
        )
        .await
    }

    pub async fn exec_shell_env_with_cancel(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
        env: &[(String, String)],
        cancellation_token: &CancellationToken,
    ) -> Result<ExecOutput> {
        let shell_command = match timeout_secs {
            Some(seconds) if seconds > 0 => {
                format!(
                    "timeout --preserve-status {seconds}s /bin/sh -lc {}",
                    shell_quote(command)
                )
            }
            _ => command.to_string(),
        };
        let mut cmd = Command::new(&self.binary);
        cmd.arg("exec");
        if let Some(cwd) = cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.args([container_id, "/bin/sh", "-lc", &shell_command]);

        cmd.kill_on_drop(true);
        let output = cmd.output();
        tokio::pin!(output);
        let output = tokio::select! {
            output = &mut output => output?,
            _ = cancellation_token.cancelled() => {
                return Err(DockerError::Cancelled);
            }
        };
        Ok(ExecOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }

    pub async fn run_sidecar_shell_env(&self, params: &SidecarParams<'_>) -> Result<ExecOutput> {
        let image = validate_image(params.image)?;
        let shell_command = match params.timeout_secs {
            Some(seconds) if seconds > 0 => {
                format!(
                    "timeout --preserve-status {seconds}s /bin/sh -lc {}",
                    shell_quote(params.command)
                )
            }
            _ => params.command.to_string(),
        };
        let mut cmd = Command::new(&self.binary);
        cmd.arg("run")
            .arg("--rm")
            .args(["--name", params.name])
            .args(["--label", MANAGED_LABEL]);
        if let Some(volume) = params.workspace_volume {
            let mount = format!("{volume}:/workspace");
            cmd.args(["-v", &mount]);
        }
        if let Some(cwd) = params.cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in params.env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.arg(image).args(["/bin/sh", "-lc", &shell_command]);

        let output = cmd.output().await?;
        Ok(ExecOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }

    pub fn spawn_exec(
        &self,
        container_id: &str,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<Child> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("exec").arg("-i");
        if let Some(cwd) = cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.arg(container_id).arg(command).args(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd.spawn()?)
    }

    pub fn spawn_sidecar(&self, params: &SidecarParams<'_>) -> Result<Child> {
        let image = validate_image(params.image)?;
        let mut cmd = Command::new(&self.binary);
        cmd.arg("run")
            .arg("--rm")
            .arg("-i")
            .args(["--name", params.name])
            .args(["--label", MANAGED_LABEL]);
        if let Some(volume) = params.workspace_volume {
            cmd.args(["-v", &format!("{volume}:/workspace")]);
        }
        if let Some(cwd) = params.cwd {
            cmd.args(["-w", cwd]);
        }
        for (key, value) in params.env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.arg(image).arg(params.command).args(params.args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd.spawn()?)
    }

    pub async fn copy_to_container(
        &self,
        container_id: &str,
        local_path: &Path,
        container_path: &str,
    ) -> Result<()> {
        let parent = parent_dir(container_path);
        if !parent.is_empty() {
            let mkdir = self
                .exec_shell(
                    container_id,
                    &format!("mkdir -p {}", shell_quote(&parent)),
                    Some("/"),
                    Some(10),
                )
                .await?;
            if mkdir.status != 0 {
                return Err(DockerError::CommandFailed(mkdir.stderr));
            }
        }

        let target = format!("{container_id}:{container_path}");
        let output = Command::new(&self.binary)
            .arg("cp")
            .arg(local_path)
            .arg(target)
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(())
    }

    pub async fn copy_from_container_tar(
        &self,
        container_id: &str,
        container_path: &str,
    ) -> Result<Vec<u8>> {
        let source = format!("{container_id}:{container_path}");
        let output = Command::new(&self.binary)
            .args(["cp", &source, "-"])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(output.stdout)
    }

    pub async fn copy_from_container_to_file(
        &self,
        container_id: &str,
        container_path: &str,
        host_path: &std::path::Path,
    ) -> Result<()> {
        let source = format!("{container_id}:{container_path}");
        let output = Command::new(&self.binary)
            .args(["cp", &source, &host_path.to_string_lossy()])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(())
    }
}

fn stderr_or_stdout(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}

fn managed_containers_from_inspect(json: &str) -> Result<Vec<ManagedContainer>> {
    let inspected = serde_json::from_str::<Vec<InspectContainer>>(json)?;
    Ok(inspected.into_iter().map(ManagedContainer::from).collect())
}

fn orphaned_container_ids(
    containers: &[ManagedContainer],
    active_agent_ids: &HashSet<String>,
    active_project_ids: &HashSet<String>,
) -> Vec<String> {
    dedupe_container_ids(containers.iter().filter_map(|container| {
        let is_orphaned = if container.sidecar {
            container
                .project_id
                .as_ref()
                .is_none_or(|project_id| !active_project_ids.contains(project_id))
        } else {
            container
                .agent_id
                .as_ref()
                .is_none_or(|agent_id| !active_agent_ids.contains(agent_id))
        };
        is_orphaned.then(|| container.id.clone())
    }))
}

fn agent_container_delete_ids(
    containers: &[ManagedContainer],
    agent_id: &str,
    preferred_container_id: Option<&str>,
) -> Vec<String> {
    let preferred = preferred_container_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let labeled = containers
        .iter()
        .filter(|container| container.agent_id.as_deref() == Some(agent_id))
        .map(|container| container.id.clone());
    dedupe_container_ids(preferred.into_iter().chain(labeled))
}

fn find_reusable_agent_container<'a>(
    containers: &'a [ManagedContainer],
    agent_id: &str,
    preferred_container_id: Option<&str>,
) -> Option<&'a ManagedContainer> {
    if let Some(preferred_container_id) = preferred_container_id
        && let Some(container) = containers.iter().find(|container| {
            container.agent_id.as_deref() == Some(agent_id)
                && container.matches_identifier(preferred_container_id)
        })
    {
        return Some(container);
    }

    containers
        .iter()
        .find(|container| container.agent_id.as_deref() == Some(agent_id))
}

fn project_sidecar_container_delete_ids(
    containers: &[ManagedContainer],
    project_id: &str,
    preferred_container_id: Option<&str>,
) -> Vec<String> {
    let preferred = preferred_container_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let labeled = containers
        .iter()
        .filter(|container| {
            container.sidecar
                && container.sidecar_kind.as_deref() == Some(PROJECT_SIDECAR_KIND)
                && container.project_id.as_deref() == Some(project_id)
        })
        .map(|container| container.id.clone());
    dedupe_container_ids(preferred.into_iter().chain(labeled))
}

fn find_reusable_project_sidecar_container<'a>(
    containers: &'a [ManagedContainer],
    project_id: &str,
    preferred_container_id: Option<&str>,
) -> Option<&'a ManagedContainer> {
    let matches_project = |container: &&ManagedContainer| {
        container.sidecar
            && container.sidecar_kind.as_deref() == Some(PROJECT_SIDECAR_KIND)
            && container.project_id.as_deref() == Some(project_id)
    };

    if let Some(preferred_container_id) = preferred_container_id
        && let Some(container) = containers.iter().find(|container| {
            matches_project(container) && container.matches_identifier(preferred_container_id)
        })
    {
        return Some(container);
    }

    containers.iter().find(matches_project)
}

fn dedupe_container_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn is_missing_container_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such container") || message.contains("no such object")
}

fn is_missing_image_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such image") || message.contains("no such object")
}

fn agent_container_name(agent_id: &str) -> String {
    format!("mai-team-{agent_id}")
}

fn agent_label(agent_id: &str) -> String {
    format!("{AGENT_LABEL_KEY}={agent_id}")
}

fn project_sidecar_container_name(project_id: &str) -> String {
    format!("mai-team-project-sidecar-{project_id}")
}

fn create_agent_container_args(
    name: &str,
    agent_label: &str,
    image: &str,
    workspace_volume: &str,
) -> Vec<String> {
    vec![
        "create".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        MANAGED_LABEL.to_string(),
        "--label".to_string(),
        agent_label.to_string(),
        "-v".to_string(),
        format!("{workspace_volume}:/workspace"),
        "-w".to_string(),
        "/workspace".to_string(),
        image.to_string(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]
}

fn create_project_sidecar_container_args(
    name: &str,
    project_id: &str,
    image: &str,
    workspace_volume: &str,
    options: &ContainerCreateOptions,
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        MANAGED_LABEL.to_string(),
        "--label".to_string(),
        format!("{SIDECAR_LABEL_KEY}=true"),
        "--label".to_string(),
        format!("{SIDECAR_KIND_LABEL_KEY}={PROJECT_SIDECAR_KIND}"),
        "--label".to_string(),
        format!("{PROJECT_LABEL_KEY}={project_id}"),
        "-v".to_string(),
        format!("{workspace_volume}:/workspace"),
        "-w".to_string(),
        "/workspace".to_string(),
    ];
    apply_container_create_options(&mut args, options);
    args.extend([
        image.to_string(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]);
    args
}

fn agent_workspace_volume(agent_id: &str) -> String {
    format!("mai-team-workspace-{agent_id}")
}

fn project_workspace_volume(project_id: &str) -> String {
    format!("mai-team-project-{project_id}")
}

fn project_review_workspace_volume(project_id: &str) -> String {
    format!("mai-team-project-review-{project_id}")
}

fn apply_container_create_options(args: &mut Vec<String>, options: &ContainerCreateOptions) {
    if let Some(memory) = options
        .memory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.extend(["--memory".to_string(), memory.to_string()]);
    }
    if let Some(cpus) = options
        .cpus
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.extend(["--cpus".to_string(), cpus.to_string()]);
    }
    if let Some(pids_limit) = options.pids_limit {
        args.extend(["--pids-limit".to_string(), pids_limit.to_string()]);
    }
    if options.cap_drop_all {
        args.extend(["--cap-drop".to_string(), "ALL".to_string()]);
    }
    if options.no_new_privileges {
        args.extend([
            "--security-opt".to_string(),
            "no-new-privileges".to_string(),
        ]);
    }
    if let Some(network) = options
        .network
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.extend(["--network".to_string(), network.to_string()]);
    }
}

fn validate_image(image: &str) -> Result<&str> {
    if image.trim().is_empty() {
        return Err(DockerError::InvalidImage(
            "image name cannot be empty".to_string(),
        ));
    }
    if image.trim() != image {
        return Err(DockerError::InvalidImage(
            "image name cannot include leading or trailing whitespace".to_string(),
        ));
    }
    if image.chars().any(char::is_whitespace) {
        return Err(DockerError::InvalidImage(
            "image name cannot include whitespace".to_string(),
        ));
    }
    if image.chars().any(char::is_control) {
        return Err(DockerError::InvalidImage(
            "image name cannot include control characters".to_string(),
        ));
    }
    Ok(image)
}

fn snapshot_image_name(agent_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("mai-team-snapshot-{agent_id}-{nanos}")
}

impl ManagedContainer {
    fn matches_identifier(&self, identifier: &str) -> bool {
        let identifier = identifier.trim().trim_start_matches('/');
        self.id == identifier
            || self.id.starts_with(identifier)
            || self.name == identifier
            || self.name.trim_start_matches('/') == identifier
    }
}

#[derive(Debug, Deserialize)]
struct InspectContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Config")]
    config: Option<InspectConfig>,
    #[serde(rename = "State")]
    state: Option<InspectState>,
}

#[derive(Debug, Deserialize)]
struct InspectConfig {
    #[serde(rename = "Image")]
    image: Option<String>,
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(rename = "Status")]
    status: Option<String>,
}

impl From<InspectContainer> for ManagedContainer {
    fn from(value: InspectContainer) -> Self {
        let labels = value
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref());
        let agent_id = labels.and_then(|labels| labels.get(AGENT_LABEL_KEY).cloned());
        let project_id = labels.and_then(|labels| labels.get(PROJECT_LABEL_KEY).cloned());
        let sidecar = labels
            .and_then(|labels| labels.get(SIDECAR_LABEL_KEY))
            .is_some_and(|value| value == "true");
        let sidecar_kind = labels.and_then(|labels| labels.get(SIDECAR_KIND_LABEL_KEY).cloned());
        let image = value
            .config
            .and_then(|config| config.image)
            .unwrap_or_default();
        let state = value
            .state
            .and_then(|state| state.status)
            .unwrap_or_default();

        Self {
            id: value.id,
            name: value.name.trim_start_matches('/').to_string(),
            image,
            state,
            agent_id,
            project_id,
            sidecar,
            sidecar_kind,
        }
    }
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| {
            if parent.is_empty() {
                "/".to_string()
            } else {
                parent.to_string()
            }
        })
        .unwrap_or_default()
}

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_handles_common_paths() {
        assert_eq!(parent_dir("/tmp/file.txt"), "/tmp");
        assert_eq!(parent_dir("relative/file.txt"), "relative");
        assert_eq!(parent_dir("file.txt"), "");
    }

    #[test]
    fn parses_managed_containers_from_inspect_json() {
        let containers = managed_containers_from_inspect(
            r#"
            [
                {
                    "Id": "abc123",
                    "Name": "/mai-team-agent-1",
                    "Config": {
                        "Image": "ubuntu:latest",
                        "Labels": {
                            "mai.team.managed": "true",
                            "mai.team.agent": "agent-1",
                            "mai.team.project": "project-1",
                            "mai.team.sidecar": "true",
                            "mai.team.sidecar.kind": "project"
                        }
                    },
                    "State": { "Status": "exited" }
                },
                {
                    "Id": "def456",
                    "Name": "/mai-team-unlabeled",
                    "Config": {
                        "Image": "ubuntu:latest",
                        "Labels": {
                            "mai.team.managed": "true"
                        }
                    },
                    "State": { "Status": "running" }
                }
            ]
            "#,
        )
        .expect("parse containers");

        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].id, "abc123");
        assert_eq!(containers[0].name, "mai-team-agent-1");
        assert_eq!(containers[0].image, "ubuntu:latest");
        assert_eq!(containers[0].state, "exited");
        assert_eq!(containers[0].agent_id.as_deref(), Some("agent-1"));
        assert_eq!(containers[0].project_id.as_deref(), Some("project-1"));
        assert!(containers[0].sidecar);
        assert_eq!(containers[0].sidecar_kind.as_deref(), Some("project"));
        assert_eq!(containers[1].agent_id, None);
        assert!(!containers[1].sidecar);
    }

    #[test]
    fn orphaned_container_ids_include_missing_agent_labels_and_dedupe() {
        let containers = vec![
            managed("keep", Some("agent-1")),
            managed("orphan", Some("deleted-agent")),
            managed("missing-label", None),
            managed("orphan", Some("deleted-agent")),
        ];
        let active_agent_ids = HashSet::from(["agent-1".to_string()]);

        assert_eq!(
            orphaned_container_ids(&containers, &active_agent_ids, &HashSet::new()),
            vec!["orphan".to_string(), "missing-label".to_string()]
        );
    }

    #[test]
    fn orphaned_container_ids_keep_active_project_sidecars() {
        let containers = vec![
            managed("agent-keep", Some("agent-1")),
            managed_project_sidecar("sidecar-keep", "project-1", "agent-1"),
            managed_project_sidecar("sidecar-orphan", "project-2", "agent-2"),
            managed_project_sidecar("sidecar-orphan", "project-2", "agent-2"),
        ];
        let active_agent_ids = HashSet::from(["agent-1".to_string()]);
        let active_project_ids = HashSet::from(["project-1".to_string()]);

        assert_eq!(
            orphaned_container_ids(&containers, &active_agent_ids, &active_project_ids),
            vec!["sidecar-orphan".to_string()]
        );
    }

    #[test]
    fn agent_container_delete_ids_use_preferred_id_and_label_fallback() {
        let containers = vec![
            managed("owned-1", Some("agent-1")),
            managed("other", Some("agent-2")),
            managed("owned-2", Some("agent-1")),
            managed("owned-1", Some("agent-1")),
        ];

        assert_eq!(
            agent_container_delete_ids(&containers, "agent-1", Some("persisted")),
            vec![
                "persisted".to_string(),
                "owned-1".to_string(),
                "owned-2".to_string()
            ]
        );
        assert_eq!(
            agent_container_delete_ids(&containers, "agent-1", Some("owned-1")),
            vec!["owned-1".to_string(), "owned-2".to_string()]
        );
    }

    #[test]
    fn reusable_container_prefers_matching_persisted_container() {
        let containers = vec![
            managed("wrong-owner", Some("agent-2")),
            managed("owned-fallback", Some("agent-1")),
            managed("owned-preferred", Some("agent-1")),
        ];

        assert_eq!(
            find_reusable_agent_container(&containers, "agent-1", Some("owned-preferred"))
                .map(|container| container.id.as_str()),
            Some("owned-preferred")
        );
        assert_eq!(
            find_reusable_agent_container(&containers, "agent-1", Some("wrong-owner"))
                .map(|container| container.id.as_str()),
            Some("owned-fallback")
        );
    }

    #[test]
    fn project_sidecar_delete_ids_use_preferred_id_and_label_fallback() {
        let containers = vec![
            managed_project_sidecar("project-owned-1", "project-1", "agent-1"),
            managed_project_sidecar("project-owned-2", "project-1", "agent-1"),
            managed_project_sidecar("other-project", "project-2", "agent-2"),
        ];

        assert_eq!(
            project_sidecar_container_delete_ids(&containers, "project-1", Some("persisted")),
            vec![
                "persisted".to_string(),
                "project-owned-1".to_string(),
                "project-owned-2".to_string()
            ]
        );
        assert_eq!(
            project_sidecar_container_delete_ids(&containers, "project-1", Some("project-owned-1")),
            vec!["project-owned-1".to_string(), "project-owned-2".to_string()]
        );
    }

    #[test]
    fn reusable_project_sidecar_prefers_matching_persisted_container() {
        let containers = vec![
            managed_project_sidecar("other-project", "project-2", "agent-2"),
            managed_project_sidecar("project-fallback", "project-1", "agent-1"),
            managed_project_sidecar("project-preferred", "project-1", "agent-1"),
        ];

        assert_eq!(
            find_reusable_project_sidecar_container(
                &containers,
                "project-1",
                Some("project-preferred")
            )
            .map(|container| container.id.as_str()),
            Some("project-preferred")
        );
        assert_eq!(
            find_reusable_project_sidecar_container(
                &containers,
                "project-1",
                Some("other-project")
            )
            .map(|container| container.id.as_str()),
            Some("project-fallback")
        );
    }

    #[test]
    fn create_agent_container_args_include_labels_workspace_and_image() {
        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let args = create_agent_container_args(
            "mai-team-child",
            "mai.team.agent=child",
            image,
            "mai-team-workspace-child",
        );

        assert_eq!(args[0], "create");
        assert!(
            args.windows(2)
                .any(|window| window == ["--name", "mai-team-child"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", MANAGED_LABEL])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.agent=child"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["-v", "mai-team-workspace-child:/workspace"])
        );
        assert!(args.windows(2).any(|window| window == ["-w", "/workspace"]));
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn create_project_sidecar_container_args_include_labels_workspace_and_image() {
        let image = "ghcr.io/zr233/mai-team-sidecar:latest";
        let args = create_project_sidecar_container_args(
            "mai-team-project-sidecar-project-1",
            "project-1",
            image,
            "mai-team-project-project-1",
            &ContainerCreateOptions::default(),
        );

        assert_eq!(args[0], "create");
        assert!(
            args.windows(2)
                .any(|window| window == ["--name", "mai-team-project-sidecar-project-1"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", MANAGED_LABEL])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.sidecar=true"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.sidecar.kind=project"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.project=project-1"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["-v", "mai-team-project-project-1:/workspace"])
        );
        assert!(!args.windows(2).any(|window| window[0] == "-e"));
        assert!(args.windows(2).any(|window| window == ["-w", "/workspace"]));
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn project_workspace_volume_uses_project_id() {
        assert_eq!(
            DockerClient::workspace_volume_for_project("project-1"),
            "mai-team-project-project-1"
        );
    }

    #[test]
    fn project_review_workspace_volume_uses_project_id() {
        assert_eq!(
            DockerClient::workspace_volume_for_project_review("project-1"),
            "mai-team-project-review-project-1"
        );
    }

    #[test]
    fn reviewer_agent_container_args_can_use_project_review_volume() {
        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let review_volume = DockerClient::workspace_volume_for_project_review("project-1");
        let args = create_agent_container_args(
            "mai-team-reviewer",
            "mai.team.agent=reviewer",
            image,
            &review_volume,
        );

        assert!(
            args.windows(2)
                .any(|window| window == ["-v", "mai-team-project-review-project-1:/workspace"])
        );
        assert!(
            !args
                .windows(2)
                .any(|window| window == ["-v", "mai-team-workspace-reviewer:/workspace"])
        );
    }

    #[test]
    fn create_project_sidecar_container_args_include_optional_hardening() {
        let image = "ghcr.io/zr233/mai-team-sidecar:latest";
        let args = create_project_sidecar_container_args(
            "mai-team-project-sidecar-project-1",
            "project-1",
            image,
            "mai-team-project-project-1",
            &ContainerCreateOptions {
                memory: Some("1g".to_string()),
                cpus: Some("2".to_string()),
                pids_limit: Some(100),
                cap_drop_all: true,
                no_new_privileges: true,
                network: Some("mai-team".to_string()),
            },
        );

        assert!(args.windows(2).any(|window| window == ["--memory", "1g"]));
        assert!(args.windows(2).any(|window| window == ["--cpus", "2"]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--pids-limit", "100"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--cap-drop", "ALL"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--security-opt", "no-new-privileges"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--network", "mai-team"])
        );
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn validate_image_rejects_empty_whitespace_and_control_characters() {
        assert_eq!(
            validate_image("ubuntu:latest").expect("valid"),
            "ubuntu:latest"
        );
        assert!(matches!(
            validate_image(""),
            Err(DockerError::InvalidImage(_))
        ));
        assert!(matches!(
            validate_image(" ubuntu:latest"),
            Err(DockerError::InvalidImage(_))
        ));
        assert!(matches!(
            validate_image("ubuntu latest"),
            Err(DockerError::InvalidImage(_))
        ));
        assert!(matches!(
            validate_image("ubuntu:\nlatest"),
            Err(DockerError::InvalidImage(_))
        ));
    }

    #[test]
    fn snapshot_image_name_uses_agent_id_and_snapshot_prefix() {
        let image = snapshot_image_name("child-agent");

        assert!(image.starts_with("mai-team-snapshot-child-agent-"));
    }

    fn managed(id: &str, agent_id: Option<&str>) -> ManagedContainer {
        ManagedContainer {
            id: id.to_string(),
            name: format!("mai-team-{id}"),
            image: "ubuntu:latest".to_string(),
            state: "running".to_string(),
            agent_id: agent_id.map(str::to_string),
            project_id: None,
            sidecar: false,
            sidecar_kind: None,
        }
    }

    fn managed_project_sidecar(id: &str, project_id: &str, agent_id: &str) -> ManagedContainer {
        ManagedContainer {
            id: id.to_string(),
            name: format!("mai-team-project-sidecar-{project_id}"),
            image: "ghcr.io/zr233/mai-team-sidecar:latest".to_string(),
            state: "running".to_string(),
            agent_id: Some(agent_id.to_string()),
            project_id: Some(project_id.to_string()),
            sidecar: true,
            sidecar_kind: Some(PROJECT_SIDECAR_KIND.to_string()),
        }
    }
}
