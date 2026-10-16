//! 容器挂载参数的强类型模型。
//!
//! 本模块只负责 Docker 参数层面的语义与转义安全：类型不变量保证只读 bind 永远生成
//! `readonly,bind-recursive=readonly,bind-propagation=rprivate`，不接受任何 UI 布尔开关。
//! 宿主路径是否位于部署允许根、目录身份（dev/ino）与内容摘要由上层策略层负责，
//! 本层不声称完成了授权判断。

use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path};

use crate::error::{DockerError, Result};

const WORKSPACE_MOUNT_TARGET: &str = "/workspace";

/// 只读参考源码在容器内的保留命名空间；别名只能生成该前缀下的目标。
pub const REFERENCE_MOUNT_ROOT: &str = "/references";

/// 容器挂载的访问模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMountAccess {
    ReadOnly,
    ReadWrite,
}

/// Docker mount 的真实类型；用于把计划与实际 inspect 结果对齐。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContainerMountType {
    Bind,
    Volume,
    Tmpfs,
    Other(String),
}

impl ContainerMountType {
    pub(crate) fn from_docker_value(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "bind" => Self::Bind,
            "volume" => Self::Volume,
            "tmpfs" => Self::Tmpfs,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for ContainerMountType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind => formatter.write_str("bind"),
            Self::Volume => formatter.write_str("volume"),
            Self::Tmpfs => formatter.write_str("tmpfs"),
            Self::Other(value) => write!(formatter, "other({value})"),
        }
    }
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

/// 已通过字面校验的宿主目录。
///
/// 只保证这是可以安全放进 argv 的规范化绝对路径；允许根、符号链接与目录身份检查
/// 属于策略层，不能因为这里通过就认为路径被授权。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedHostDirectory(String);

impl ValidatedHostDirectory {
    pub fn new(path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        validate_host_directory(&path)?;
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 参考源码在容器内的目标路径 `/references/<alias>`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerReferencePath(String);

impl ContainerReferencePath {
    pub fn new(target: impl Into<String>) -> Result<Self> {
        let target = target.into();
        validate_reference_target(&target)?;
        Ok(Self(target))
    }

    /// 由别名生成参考挂载目标；别名语法与项目配置层保持一致。
    pub fn from_alias(alias: impl Into<String>) -> Result<Self> {
        let alias = alias.into();
        validate_alias(&alias)?;
        Self::new(format!("{REFERENCE_MOUNT_ROOT}/{alias}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `/references/` 下的那一段别名。
    pub fn alias(&self) -> &str {
        self.0
            .strip_prefix(REFERENCE_MOUNT_ROOT)
            .unwrap_or(&self.0)
            .trim_start_matches('/')
    }
}

/// 只读 bind 挂载：字段私有，只读、rprivate、严格递归只读是类型不变量。
///
/// 该类型不接收“是否只读”之类的调用方开关，因此无法构造出可写或共享传播的参考挂载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyBindMount {
    source: ValidatedHostDirectory,
    target: ContainerReferencePath,
}

impl ReadOnlyBindMount {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Result<Self> {
        Ok(Self {
            source: ValidatedHostDirectory::new(source)?,
            target: ContainerReferencePath::new(target)?,
        })
    }

    /// 由宿主目录和别名构造参考挂载。
    pub fn for_reference(source: impl Into<String>, alias: impl Into<String>) -> Result<Self> {
        Ok(Self {
            source: ValidatedHostDirectory::new(source)?,
            target: ContainerReferencePath::from_alias(alias)?,
        })
    }

    pub fn source(&self) -> &str {
        self.source.as_str()
    }

    pub fn target(&self) -> &str {
        self.target.as_str()
    }

    pub fn alias(&self) -> &str {
        self.target.alias()
    }

    pub(crate) fn docker_mount_spec(&self) -> String {
        format!(
            "type=bind,src={},dst={},readonly,bind-recursive=readonly,bind-propagation=rprivate",
            self.source.as_str(),
            self.target.as_str()
        )
    }
}

/// 有类型的附加挂载：命名卷或只读参考 bind。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerMount {
    Volume(ContainerVolumeMount),
    ReadOnlyBind(ReadOnlyBindMount),
}

impl From<ContainerVolumeMount> for ContainerMount {
    fn from(mount: ContainerVolumeMount) -> Self {
        Self::Volume(mount)
    }
}

impl From<ReadOnlyBindMount> for ContainerMount {
    fn from(mount: ReadOnlyBindMount) -> Self {
        Self::ReadOnlyBind(mount)
    }
}

impl ContainerMount {
    pub fn target(&self) -> &str {
        match self {
            Self::Volume(mount) => mount.target(),
            Self::ReadOnlyBind(mount) => mount.target(),
        }
    }

    pub fn access(&self) -> ContainerMountAccess {
        match self {
            Self::Volume(mount) => mount.access(),
            Self::ReadOnlyBind(_) => ContainerMountAccess::ReadOnly,
        }
    }

    pub fn mount_type(&self) -> ContainerMountType {
        match self {
            Self::Volume(_) => ContainerMountType::Volume,
            Self::ReadOnlyBind(_) => ContainerMountType::Bind,
        }
    }

    pub fn volume(&self) -> Option<&ContainerVolumeMount> {
        match self {
            Self::Volume(mount) => Some(mount),
            Self::ReadOnlyBind(_) => None,
        }
    }

    pub fn read_only_bind(&self) -> Option<&ReadOnlyBindMount> {
        match self {
            Self::ReadOnlyBind(mount) => Some(mount),
            Self::Volume(_) => None,
        }
    }

    pub(crate) fn docker_mount_spec(&self) -> String {
        match self {
            Self::Volume(mount) => mount.docker_mount_spec(),
            Self::ReadOnlyBind(mount) => mount.docker_mount_spec(),
        }
    }
}

/// 校验附加挂载集合：目标唯一，且不覆盖 agent 的受管工作区。
pub(crate) fn validate_additional_mounts(mounts: &[ContainerMount]) -> Result<()> {
    let mut targets = HashSet::with_capacity(mounts.len());
    for mount in mounts {
        reject_overlapping_target(mount.target())?;
        if !targets.insert(mount.target()) {
            return Err(DockerError::InvalidMount(format!(
                "duplicate mount target `{}`",
                mount.target()
            )));
        }
    }
    Ok(())
}

/// 校验只使用命名卷的挂载集合（工具 sidecar 不接收参考 bind）。
pub(crate) fn validate_volume_mounts(mounts: &[ContainerVolumeMount]) -> Result<()> {
    let mut targets = HashSet::with_capacity(mounts.len());
    for mount in mounts {
        reject_overlapping_target(mount.target())?;
        if !targets.insert(mount.target()) {
            return Err(DockerError::InvalidMount(format!(
                "duplicate mount target `{}`",
                mount.target()
            )));
        }
    }
    Ok(())
}

fn reject_overlapping_target(target: &str) -> Result<()> {
    if target == WORKSPACE_MOUNT_TARGET || target.starts_with(&format!("{WORKSPACE_MOUNT_TARGET}/"))
    {
        return Err(DockerError::InvalidMount(format!(
            "additional mount target `{target}` overlaps the managed workspace"
        )));
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

fn validate_host_directory(path: &str) -> Result<()> {
    let parsed = Path::new(path);
    if path.is_empty() || !parsed.is_absolute() {
        return Err(DockerError::InvalidMount(format!(
            "host directory `{path}` must be an absolute path"
        )));
    }
    if path == "/"
        || path.ends_with('/')
        || path.contains("//")
        || path.contains([',', ':'])
        || path.chars().any(char::is_whitespace)
        || path.chars().any(char::is_control)
        || parsed
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(DockerError::InvalidMount(format!(
            "host directory `{path}` is not a normalized absolute path"
        )));
    }
    Ok(())
}

fn validate_reference_target(target: &str) -> Result<()> {
    validate_target(target)?;
    let Some(alias) = target.strip_prefix(&format!("{REFERENCE_MOUNT_ROOT}/")) else {
        return Err(DockerError::InvalidMount(format!(
            "reference mount target `{target}` must live under {REFERENCE_MOUNT_ROOT}/"
        )));
    };
    validate_alias(alias)
}

fn validate_alias(alias: &str) -> Result<()> {
    let mut characters = alias.chars();
    let first_is_valid = characters
        .next()
        .is_some_and(|character| character.is_ascii_lowercase());
    let rest_are_valid = characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '_' | '-')
    });
    if !first_is_valid || !rest_are_valid || alias.len() > 32 {
        return Err(DockerError::InvalidMount(format!(
            "reference alias `{alias}` must match [a-z][a-z0-9_-]{{0,31}}"
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
    fn renders_strict_read_only_bind_mount() {
        let mount = ReadOnlyBindMount::new(
            "/srv/mai-references/linux/8f058410/repo",
            "/references/linux",
        )
        .expect("valid read-only bind");

        assert_eq!(
            mount.docker_mount_spec(),
            "type=bind,src=/srv/mai-references/linux/8f058410/repo,dst=/references/linux,readonly,bind-recursive=readonly,bind-propagation=rprivate"
        );
        assert_eq!(
            ContainerMount::from(mount.clone()).access(),
            ContainerMountAccess::ReadOnly
        );
        assert_eq!(
            ContainerMount::from(mount).mount_type(),
            ContainerMountType::Bind
        );
    }

    #[test]
    fn builds_reference_mount_from_alias() {
        let mount = ReadOnlyBindMount::for_reference("/srv/mai-references/linux/repo", "linux")
            .expect("valid alias");

        assert_eq!(mount.target(), "/references/linux");
        assert_eq!(mount.alias(), "linux");
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
    fn rejects_unsafe_read_only_bind_inputs() {
        for source in [
            "",
            "relative/repo",
            "/srv/references/",
            "/srv/mai-references/linux/../secret",
            "/srv/mai-references,/linux",
            "/srv/mai references/linux",
            "/srv/mai-references/linux\n/repo",
            "/",
        ] {
            assert!(
                matches!(
                    ReadOnlyBindMount::new(source, "/references/linux"),
                    Err(DockerError::InvalidMount(_))
                ),
                "host source `{source}` must be rejected"
            );
        }

        for target in [
            "/references",
            "/references/",
            "/references/Linux",
            "/references/linux/nested",
            "/references/linux,",
            "/references/1linux",
            "/srv/linux",
            "references/linux",
        ] {
            assert!(
                matches!(
                    ReadOnlyBindMount::new("/srv/mai-references/linux/repo", target),
                    Err(DockerError::InvalidMount(_))
                ),
                "reference target `{target}` must be rejected"
            );
        }
    }

    #[test]
    fn rejects_duplicate_and_workspace_mounts() {
        let duplicate = [
            ContainerMount::Volume(
                ContainerVolumeMount::read_only("one", "/project/repo").expect("first"),
            ),
            ContainerMount::Volume(
                ContainerVolumeMount::read_write("two", "/project/repo").expect("second"),
            ),
        ];
        assert!(matches!(
            validate_additional_mounts(&duplicate),
            Err(DockerError::InvalidMount(_))
        ));

        for target in ["/workspace", "/workspace/repo"] {
            let mounts = [ContainerMount::Volume(
                ContainerVolumeMount::read_only("project", target).expect("mount"),
            )];
            assert!(matches!(
                validate_additional_mounts(&mounts),
                Err(DockerError::InvalidMount(_))
            ));
        }

        let read_only_bind = [ContainerMount::ReadOnlyBind(
            ReadOnlyBindMount::for_reference("/srv/mai-references/linux/repo", "linux")
                .expect("reference mount"),
        )];
        assert!(validate_additional_mounts(&read_only_bind).is_ok());
    }
}
