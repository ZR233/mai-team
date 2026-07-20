use std::collections::HashSet;
use std::path::{Component, Path};

use crate::error::{DockerError, Result};

const WORKSPACE_MOUNT_TARGET: &str = "/workspace";

/// 容器 volume 挂载的访问模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMountAccess {
    ReadOnly,
    ReadWrite,
}

/// 一个经过校验的 Docker named-volume 挂载。
///
/// `volume_subpath` 始终是 volume 内的相对路径；调用方必须在创建容器前确保
/// 该目录已存在。附加挂载不能覆盖 agent 的 `/workspace` 工作区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerVolumeMount {
    volume: String,
    target: String,
    access: ContainerMountAccess,
    volume_subpath: Option<String>,
}

impl ContainerVolumeMount {
    pub fn read_only(volume: impl Into<String>, target: impl Into<String>) -> Result<Self> {
        Self::new(volume, target, ContainerMountAccess::ReadOnly, None)
    }

    pub fn read_write(volume: impl Into<String>, target: impl Into<String>) -> Result<Self> {
        Self::new(volume, target, ContainerMountAccess::ReadWrite, None)
    }

    pub fn read_only_subpath(
        volume: impl Into<String>,
        target: impl Into<String>,
        volume_subpath: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            volume,
            target,
            ContainerMountAccess::ReadOnly,
            Some(volume_subpath.into()),
        )
    }

    fn new(
        volume: impl Into<String>,
        target: impl Into<String>,
        access: ContainerMountAccess,
        volume_subpath: Option<String>,
    ) -> Result<Self> {
        let volume = volume.into();
        validate_volume(&volume)?;
        let target = target.into();
        validate_target(&target)?;
        if let Some(subpath) = volume_subpath.as_deref() {
            validate_volume_subpath(subpath)?;
        }
        Ok(Self {
            volume,
            target,
            access,
            volume_subpath,
        })
    }

    pub fn volume(&self) -> &str {
        &self.volume
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn access(&self) -> ContainerMountAccess {
        self.access
    }

    pub fn volume_subpath(&self) -> Option<&str> {
        self.volume_subpath.as_deref()
    }

    pub(crate) fn docker_mount_spec(&self) -> String {
        let mut fields = vec![
            "type=volume".to_string(),
            format!("src={}", self.volume),
            format!("dst={}", self.target),
        ];
        if let Some(subpath) = self.volume_subpath.as_deref() {
            fields.push(format!("volume-subpath={subpath}"));
        }
        if self.access == ContainerMountAccess::ReadOnly {
            fields.push("readonly".to_string());
        }
        fields.join(",")
    }
}

pub(crate) fn validate_additional_mounts(mounts: &[ContainerVolumeMount]) -> Result<()> {
    let mut targets = HashSet::with_capacity(mounts.len());
    for mount in mounts {
        if !targets.insert(mount.target()) {
            return Err(DockerError::InvalidMount(format!(
                "duplicate mount target `{}`",
                mount.target()
            )));
        }
        if mount.target() == WORKSPACE_MOUNT_TARGET
            || mount
                .target()
                .starts_with(&format!("{WORKSPACE_MOUNT_TARGET}/"))
        {
            return Err(DockerError::InvalidMount(format!(
                "additional mount target `{}` overlaps the managed workspace",
                mount.target()
            )));
        }
    }
    Ok(())
}

fn validate_volume(volume: &str) -> Result<()> {
    if volume.is_empty() {
        return Err(DockerError::InvalidMount(
            "volume name cannot be empty".to_string(),
        ));
    }
    if volume.trim() != volume
        || volume.chars().any(char::is_whitespace)
        || volume.chars().any(char::is_control)
        || volume.contains([':', ','])
    {
        return Err(DockerError::InvalidMount(format!(
            "invalid volume name `{volume}`"
        )));
    }
    Ok(())
}

fn validate_target(target: &str) -> Result<()> {
    let path = Path::new(target);
    if !path.is_absolute() {
        return Err(DockerError::InvalidMount(format!(
            "mount target `{target}` must be absolute"
        )));
    }
    if target == "/"
        || target.ends_with('/')
        || target.contains("//")
        || target.contains(',')
        || target.chars().any(char::is_control)
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(DockerError::InvalidMount(format!(
            "mount target `{target}` is not normalized"
        )));
    }
    Ok(())
}

fn validate_volume_subpath(subpath: &str) -> Result<()> {
    let path = Path::new(subpath);
    if subpath.is_empty()
        || path.is_absolute()
        || subpath.ends_with('/')
        || subpath.contains(',')
        || subpath.chars().any(char::is_control)
        || subpath.split('/').any(|component| component.is_empty())
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(DockerError::InvalidMount(format!(
            "volume subpath `{subpath}` must be a normalized relative path"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn renders_read_only_volume_subpath_mount() {
        let mount = ContainerVolumeMount::read_only_subpath(
            "project-volume",
            "/project/repo",
            "review-contexts/run-1/repo",
        )
        .expect("valid mount");

        assert_eq!(
            mount.docker_mount_spec(),
            "type=volume,src=project-volume,dst=/project/repo,volume-subpath=review-contexts/run-1/repo,readonly"
        );
        assert_eq!(mount.access(), ContainerMountAccess::ReadOnly);
        assert_eq!(mount.volume_subpath(), Some("review-contexts/run-1/repo"));
    }

    #[test]
    fn rejects_invalid_mount_fields() {
        for result in [
            ContainerVolumeMount::read_only("", "/project/repo"),
            ContainerVolumeMount::read_only("project", "project/repo"),
            ContainerVolumeMount::read_only_subpath("project", "/project/repo", "../repo"),
            ContainerVolumeMount::read_only_subpath("project", "/project/repo", "/repo"),
        ] {
            assert!(matches!(result, Err(DockerError::InvalidMount(_))));
        }
    }

    #[test]
    fn rejects_duplicate_and_workspace_mounts() {
        let duplicate = [
            ContainerVolumeMount::read_only("one", "/project/repo").expect("first"),
            ContainerVolumeMount::read_write("two", "/project/repo").expect("second"),
        ];
        assert!(matches!(
            validate_additional_mounts(&duplicate),
            Err(DockerError::InvalidMount(_))
        ));

        for target in ["/workspace", "/workspace/repo"] {
            let mounts = [ContainerVolumeMount::read_only("project", target).expect("mount")];
            assert!(matches!(
                validate_additional_mounts(&mounts),
                Err(DockerError::InvalidMount(_))
            ));
        }
    }
}
