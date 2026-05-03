use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use thiserror::Error;
use tokio::process::{Child, Command};

const MANAGED_LABEL: &str = "mai.team.managed=true";
const AGENT_LABEL_KEY: &str = "mai.team.agent";

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl DockerClient {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            binary: "docker".to_string(),
            image: image.into(),
        }
    }

    pub fn image(&self) -> &str {
        &self.image
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
        let containers = self.list_managed_containers().await?;
        let ids = orphaned_container_ids(&containers, active_agent_ids);

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
        if let Some(container) = self
            .reusable_agent_container(agent_id, preferred_container_id)
            .await?
        {
            return self.prepare_existing_container(container).await;
        }

        self.create_agent_container(agent_id).await
    }

    pub async fn create_agent_container(&self, agent_id: &str) -> Result<ContainerHandle> {
        let name = format!("mai-team-{agent_id}");
        let create = Command::new(&self.binary)
            .args([
                "create",
                "--name",
                &name,
                "--label",
                MANAGED_LABEL,
                "--label",
                &format!("{AGENT_LABEL_KEY}={agent_id}"),
                "-w",
                "/workspace",
                &self.image,
                "sleep",
                "infinity",
            ])
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
            image: self.image.clone(),
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

    async fn reusable_agent_container(
        &self,
        agent_id: &str,
        preferred_container_id: Option<&str>,
    ) -> Result<Option<ManagedContainer>> {
        let containers = self.list_managed_containers().await?;
        Ok(find_reusable_agent_container(&containers, agent_id, preferred_container_id).cloned())
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
        cmd.args([container_id, "/bin/sh", "-lc", &shell_command]);

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
            cmd.args(["-e", &format!("{key}={value}")]);
        }
        cmd.arg(container_id).arg(command).args(args);
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
) -> Vec<String> {
    dedupe_container_ids(containers.iter().filter_map(|container| {
        let is_orphaned = container
            .agent_id
            .as_ref()
            .is_none_or(|agent_id| !active_agent_ids.contains(agent_id));
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
    format!("'{}'", value.replace('\'', "'\\''"))
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
                        "Image": "ubuntu:24.04",
                        "Labels": {
                            "mai.team.managed": "true",
                            "mai.team.agent": "agent-1"
                        }
                    },
                    "State": { "Status": "exited" }
                },
                {
                    "Id": "def456",
                    "Name": "/mai-team-unlabeled",
                    "Config": {
                        "Image": "ubuntu:24.04",
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
        assert_eq!(containers[0].image, "ubuntu:24.04");
        assert_eq!(containers[0].state, "exited");
        assert_eq!(containers[0].agent_id.as_deref(), Some("agent-1"));
        assert_eq!(containers[1].agent_id, None);
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
            orphaned_container_ids(&containers, &active_agent_ids),
            vec!["orphan".to_string(), "missing-label".to_string()]
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

    fn managed(id: &str, agent_id: Option<&str>) -> ManagedContainer {
        ManagedContainer {
            id: id.to_string(),
            name: format!("mai-team-{id}"),
            image: "ubuntu:24.04".to_string(),
            state: "running".to_string(),
            agent_id: agent_id.map(str::to_string),
        }
    }
}
