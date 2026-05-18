use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use tokio::process::Command;

use crate::args::{
    ContainerCreateOptions, create_agent_container_args_with_workspace,
    create_project_sidecar_container_args, validate_image,
};
use crate::client::{DockerClient, stderr_or_stdout};
use crate::error::{DockerError, Result};
use crate::inspect::{
    ManagedContainer, ManagedVolume, managed_containers_from_inspect, managed_volumes_from_inspect,
};
use crate::naming::{
    MANAGED_LABEL, agent_container_name, agent_label, agent_workspace_volume,
    project_sidecar_container_name, snapshot_image_name,
};
use crate::selection::{
    agent_container_delete_ids, find_reusable_agent_container,
    find_reusable_project_sidecar_container, orphaned_container_ids,
    project_sidecar_container_delete_ids,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerHandle {
    pub id: String,
    pub name: String,
    pub image: String,
}

impl DockerClient {
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

        self.inspect_managed_containers(&ids).await
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

    async fn inspect_managed_containers(&self, ids: &[String]) -> Result<Vec<ManagedContainer>> {
        let inspect = Command::new(&self.binary)
            .arg("inspect")
            .args(ids)
            .output()
            .await?;
        if inspect.status.success() {
            return managed_containers_from_inspect(&String::from_utf8(inspect.stdout)?);
        }

        let message = stderr_or_stdout(&inspect);
        if !is_missing_container_error(&message) {
            return Err(DockerError::CommandFailed(message));
        }

        let mut containers = Vec::new();
        for id in ids {
            let inspect = Command::new(&self.binary)
                .arg("inspect")
                .arg(id)
                .output()
                .await?;
            if !inspect.status.success() {
                let message = stderr_or_stdout(&inspect);
                if is_missing_container_error(&message) {
                    continue;
                }
                return Err(DockerError::CommandFailed(message));
            }
            containers.extend(managed_containers_from_inspect(&String::from_utf8(
                inspect.stdout,
            )?)?);
        }

        Ok(containers)
    }

    pub async fn list_managed_volumes(&self) -> Result<Vec<ManagedVolume>> {
        let output = Command::new(&self.binary)
            .args([
                "volume",
                "ls",
                "-q",
                "--filter",
                &format!("label={MANAGED_LABEL}"),
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }

        let names = String::from_utf8(output.stdout)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Ok(Vec::new());
        }

        self.inspect_managed_volumes(&names).await
    }

    async fn inspect_managed_volumes(&self, names: &[String]) -> Result<Vec<ManagedVolume>> {
        let inspect = Command::new(&self.binary)
            .args(["volume", "inspect"])
            .args(names)
            .output()
            .await?;
        if inspect.status.success() {
            return managed_volumes_from_inspect(&String::from_utf8(inspect.stdout)?);
        }

        let message = stderr_or_stdout(&inspect);
        if !is_missing_volume_error(&message) {
            return Err(DockerError::CommandFailed(message));
        }

        let mut volumes = Vec::new();
        for name in names {
            let inspect = Command::new(&self.binary)
                .args(["volume", "inspect", name])
                .output()
                .await?;
            if !inspect.status.success() {
                let message = stderr_or_stdout(&inspect);
                if is_missing_volume_error(&message) {
                    continue;
                }
                return Err(DockerError::CommandFailed(message));
            }
            volumes.extend(managed_volumes_from_inspect(&String::from_utf8(
                inspect.stdout,
            )?)?);
        }

        Ok(volumes)
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
        self.ensure_agent_container_from_image_with_workspace(
            agent_id,
            preferred_container_id,
            image,
            None,
        )
        .await
    }

    pub async fn ensure_agent_container_from_image_with_workspace(
        &self,
        agent_id: &str,
        preferred_container_id: Option<&str>,
        image: &str,
        workspace_volume: Option<&str>,
    ) -> Result<ContainerHandle> {
        let image = validate_image(image)?;
        if let Some(container) = self
            .reusable_agent_container(agent_id, preferred_container_id)
            .await?
        {
            return self.prepare_existing_container(container).await;
        }

        self.create_agent_container_from_image_with_workspace(agent_id, image, workspace_volume)
            .await
    }

    pub async fn ensure_agent_container_from_image_with_workspace_and_repo_mount(
        &self,
        agent_id: &str,
        preferred_container_id: Option<&str>,
        image: &str,
        workspace_volume: Option<&str>,
        repo_mount: Option<&str>,
    ) -> Result<ContainerHandle> {
        let image = validate_image(image)?;
        if let Some(container) = self
            .reusable_agent_container(agent_id, preferred_container_id)
            .await?
        {
            return self.prepare_existing_container(container).await;
        }

        self.create_agent_container_from_image_with_workspace_and_repo_mount(
            agent_id,
            image,
            workspace_volume,
            repo_mount,
        )
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

    pub async fn create_agent_container_from_parent_with_workspace_and_repo_mount(
        &self,
        agent_id: &str,
        parent_container_id: &str,
        workspace_volume: Option<&str>,
        repo_mount: Option<&str>,
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
            .create_agent_container_from_image_with_workspace_and_repo_mount(
                agent_id,
                &image,
                workspace_volume,
                repo_mount,
            )
            .await;
        if let Err(err) = self.delete_image(&image).await {
            tracing::warn!(image = %image, "failed to remove temporary snapshot image: {err}");
        }
        result
    }

    async fn create_agent_container_from_image_with_workspace(
        &self,
        agent_id: &str,
        image: &str,
        workspace_volume: Option<&str>,
    ) -> Result<ContainerHandle> {
        self.create_agent_container_from_image_with_workspace_and_repo_mount(
            agent_id,
            image,
            workspace_volume,
            None,
        )
        .await
    }

    async fn create_agent_container_from_image_with_workspace_and_repo_mount(
        &self,
        agent_id: &str,
        image: &str,
        workspace_volume: Option<&str>,
        repo_mount: Option<&str>,
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
        if let Some(repo_mount) = repo_mount {
            tracing::warn!(
                repo_mount,
                "ignoring host repo bind mount; project agents use workspace volumes"
            );
        }
        let args =
            create_agent_container_args_with_workspace(&name, &label, image, workspace_volume);
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
            if is_missing_volume_error(&message) {
                return Ok(());
            }
            return Err(DockerError::CommandFailed(message));
        }
        Ok(())
    }

    pub async fn volume_exists(&self, volume: &str) -> Result<bool> {
        let output = Command::new(&self.binary)
            .args(["volume", "inspect", volume])
            .output()
            .await?;
        if output.status.success() {
            return Ok(true);
        }
        let message = stderr_or_stdout(&output);
        if is_missing_volume_error(&message) {
            return Ok(false);
        }
        Err(DockerError::CommandFailed(message))
    }

    pub async fn ensure_volume(&self, volume: &str, labels: &[(&str, String)]) -> Result<()> {
        let mut args = vec!["volume".to_string(), "create".to_string()];
        for (key, value) in labels {
            args.push("--label".to_string());
            args.push(format!("{key}={value}"));
        }
        args.push(volume.to_string());
        let output = Command::new(&self.binary).args(&args).output().await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
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
}

fn is_missing_container_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such container")
        || message.contains("no such object")
        || (message.contains("removal of container") && message.contains("already in progress"))
}

fn is_missing_image_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such image") || message.contains("no such object")
}

fn is_missing_volume_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such volume") || message.contains("no such object")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_removal_in_progress_is_idempotent() {
        assert!(is_missing_container_error(
            "Error response from daemon: removal of container 7a73dc22f0e3 is already in progress"
        ));
    }

    #[test]
    fn missing_object_during_inspect_is_idempotent() {
        assert!(is_missing_container_error(
            "Error response from daemon: no such object: 6d80b0090847"
        ));
    }

    #[test]
    fn missing_object_during_volume_inspect_is_idempotent() {
        assert!(is_missing_volume_error(
            "Error response from daemon: no such object: mai-team-project-1-cache"
        ));
    }
}
