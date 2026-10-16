//! `docker inspect` 结果的解析。
//!
//! 这里区分两类事实：
//! - `ManagedContainer`：受管容器的轻量索引，用于标签级定位与孤儿清理；
//! - `ContainerEnvironmentFacts`：挂载与 HostConfig 的完整快照，用于核对某个 Agent 的
//!   实际环境是否符合最终计划。标签只用于定位，不能作为授权证明。

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;
use crate::mount::ContainerMountType;
use crate::naming::{
    AGENT_LABEL_KEY, GENERATION_LABEL_KEY, KIND_LABEL_KEY, PROJECT_LABEL_KEY, ROLE_LABEL_KEY,
    SIDECAR_KIND_LABEL_KEY, SIDECAR_LABEL_KEY,
};

/// sidecar 记录自己 `--volumes-from` 源容器的标签键。
///
/// 交接说明（主代理）：该标签目前还没有在任何地方写入；args.rs 的
/// `create_agent_mcp_sidecar_container_args` 加上
/// `--label mai.team.sidecar.source=<agent container id>` 之后，
/// sidecar 复用核对就会自动多一条"指向哪个源容器"的精确判据。
pub(crate) const SIDECAR_SOURCE_LABEL_KEY: &str = "mai.team.sidecar.source";

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
    /// 环境换代标识；旧容器/未标记容器为 `None`，表示“未核验”，不是“已应用空集”。
    pub generation: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedVolume {
    pub name: String,
    pub kind: Option<String>,
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub role: Option<String>,
}

impl ManagedContainer {
    pub(crate) fn matches_identifier(&self, identifier: &str) -> bool {
        let identifier = identifier.trim().trim_start_matches('/');
        self.id == identifier
            || self.id.starts_with(identifier)
            || self.name == identifier
            || self.name.trim_start_matches('/') == identifier
    }
}

/// 单个挂载的实际事实。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerMountFacts {
    pub target: String,
    pub mount_type: ContainerMountType,
    /// bind 的宿主路径，或 volume 的名称（匿名卷使用 Docker 生成的名称）。
    pub source: String,
    pub read_only: bool,
    /// bind 传播模式；Docker 省略时表示默认的 `rprivate`。
    pub propagation: Option<String>,
    /// `bind-recursive=readonly` 的实际状态；`None` 表示引擎未报告该字段，属于未核验。
    pub recursive_read_only: Option<bool>,
    /// volume 的实际名称（含 Docker 生成的匿名卷）。
    pub volume_name: Option<String>,
    pub volume_subpath: Option<String>,
}

/// HostConfig 中与本环境复用判断相关的字段。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerHostFacts {
    pub network_mode: Option<String>,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub memory_bytes: Option<i64>,
    pub nano_cpus: Option<i64>,
    pub pids_limit: Option<i64>,
    pub security_opt: Vec<String>,
}

/// 某个容器实际环境的完整事实。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerEnvironmentFacts {
    pub id: String,
    pub name: String,
    pub state: String,
    pub running: bool,
    /// `Config.Image`：创建时使用的镜像引用。
    pub image_reference: String,
    /// 顶层 `Image`：实际生效的镜像 ID。
    pub image_id: String,
    pub agent_id: Option<String>,
    pub project_id: Option<String>,
    pub generation: Option<Uuid>,
    pub sidecar: bool,
    pub sidecar_kind: Option<String>,
    /// 按目标路径排序的挂载事实（HostConfig 规格优先，真实挂载补齐）。
    pub mounts: Vec<ContainerMountFacts>,
    pub host: ContainerHostFacts,
}

impl ContainerEnvironmentFacts {
    pub fn mount_at(&self, target: &str) -> Option<&ContainerMountFacts> {
        self.mounts.iter().find(|mount| mount.target == target)
    }
}

pub(crate) fn managed_containers_from_inspect(json: &str) -> Result<Vec<ManagedContainer>> {
    let inspected = serde_json::from_str::<Vec<InspectContainer>>(json)?;
    Ok(inspected.into_iter().map(ManagedContainer::from).collect())
}

pub(crate) fn managed_volumes_from_inspect(json: &str) -> Result<Vec<ManagedVolume>> {
    let inspected = serde_json::from_str::<Vec<InspectVolume>>(json)?;
    Ok(inspected.into_iter().map(ManagedVolume::from).collect())
}

pub(crate) fn container_environment_facts_from_inspect(
    json: &str,
) -> Result<Vec<ContainerEnvironmentFacts>> {
    let inspected = serde_json::from_str::<Vec<InspectContainer>>(json)?;
    Ok(inspected
        .into_iter()
        .map(ContainerEnvironmentFacts::from)
        .collect())
}

/// sidecar／源容器核验所需的 inspect 事实。
///
/// 完整环境事实在 [`ContainerEnvironmentFacts`] 里；这里只补两件既有结构没有、
/// 但复用核对必须的信息：容器创建时间，以及 `--volumes-from` 源容器标签。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerVerificationFacts {
    pub environment: ContainerEnvironmentFacts,
    /// 容器创建时间（Docker 用 RFC3339Nano，UTC）。
    pub created: Option<String>,
    /// `mai.team.sidecar.source` 标签记录的 `--volumes-from` 源容器 ID。
    pub sidecar_source_container_id: Option<String>,
}

impl ContainerVerificationFacts {
    pub fn id(&self) -> &str {
        &self.environment.id
    }

    /// 本容器是否严格晚于 `other` 创建。
    ///
    /// 任一时间戳无法归一化时返回 `None`（调用方必须把"无法判断"当成未核验，而不是当成通过）。
    pub fn created_after(&self, other: &Self) -> Option<bool> {
        let this = normalize_docker_timestamp(self.created.as_deref()?)?;
        let other = normalize_docker_timestamp(other.created.as_deref()?)?;
        Some(this > other)
    }
}

pub(crate) fn container_verification_facts_from_inspect(
    json: &str,
) -> Result<Vec<ContainerVerificationFacts>> {
    let inspected = serde_json::from_str::<Vec<InspectContainer>>(json)?;
    Ok(inspected
        .into_iter()
        .map(ContainerVerificationFacts::from)
        .collect())
}

/// 把 Docker 的 RFC3339Nano 时间戳归一化成定宽可比较字符串。
///
/// Docker 会裁掉小数部分的尾随零（整秒时甚至没有小数点），因此直接按字典序比较会把
/// `…:45Z` 误判成晚于 `…:45.1Z`。这里只接受 UTC `Z` 结尾、形状正确的值，并把小数位补齐到
/// 9 位之后再比较；形状不认识时返回 `None`，由调用方按未核验处理。
pub fn normalize_docker_timestamp(value: &str) -> Option<String> {
    let value = value.trim();
    let (date_and_time, fraction) = match value.split_once('.') {
        Some((head, tail)) => (head, tail.strip_suffix('Z')?),
        None => (value.strip_suffix('Z')?, ""),
    };
    let bytes = date_and_time.as_bytes();
    if bytes.len() != 19 {
        return None;
    }
    let shaped = bytes.iter().enumerate().all(|(index, byte)| match index {
        4 | 7 => *byte == b'-',
        10 => *byte == b'T',
        13 | 16 => *byte == b':',
        _ => byte.is_ascii_digit(),
    });
    if !shaped || fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("{date_and_time}.{fraction:0<9}Z"))
}

#[derive(Debug, Deserialize)]
struct InspectContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Created")]
    created: Option<String>,
    #[serde(rename = "Image")]
    image_id: Option<String>,
    #[serde(rename = "Config")]
    config: Option<InspectConfig>,
    #[serde(rename = "State")]
    state: Option<InspectState>,
    #[serde(rename = "Mounts")]
    mounts: Option<Vec<InspectRealizedMount>>,
    #[serde(rename = "HostConfig")]
    host_config: Option<InspectHostConfig>,
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
    #[serde(rename = "Running")]
    running: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct InspectRealizedMount {
    #[serde(rename = "Type")]
    mount_type: Option<String>,
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Source")]
    source: Option<String>,
    #[serde(rename = "Destination")]
    destination: Option<String>,
    #[serde(rename = "RW")]
    read_write: Option<bool>,
    #[serde(rename = "Propagation")]
    propagation: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct InspectHostConfig {
    #[serde(rename = "Binds")]
    binds: Option<Vec<String>>,
    #[serde(rename = "NetworkMode")]
    network_mode: Option<String>,
    #[serde(rename = "Privileged")]
    privileged: Option<bool>,
    #[serde(rename = "CapAdd")]
    cap_add: Option<Vec<String>>,
    #[serde(rename = "CapDrop")]
    cap_drop: Option<Vec<String>>,
    #[serde(rename = "Memory")]
    memory: Option<i64>,
    #[serde(rename = "NanoCpus")]
    nano_cpus: Option<i64>,
    #[serde(rename = "PidsLimit")]
    pids_limit: Option<i64>,
    #[serde(rename = "SecurityOpt")]
    security_opt: Option<Vec<String>>,
    #[serde(rename = "Mounts")]
    mounts: Option<Vec<InspectSpecMount>>,
}

#[derive(Debug, Deserialize)]
struct InspectSpecMount {
    #[serde(rename = "Type")]
    mount_type: Option<String>,
    #[serde(rename = "Source")]
    source: Option<String>,
    #[serde(rename = "Target")]
    target: Option<String>,
    #[serde(rename = "ReadOnly")]
    read_only: Option<bool>,
    #[serde(rename = "BindOptions")]
    bind_options: Option<InspectBindOptions>,
    #[serde(rename = "VolumeOptions")]
    volume_options: Option<InspectVolumeOptions>,
}

#[derive(Debug, Deserialize)]
struct InspectBindOptions {
    #[serde(rename = "Propagation")]
    propagation: Option<String>,
    /// Docker 25.0+ 报告 `bind-recursive=readonly` 的强制递归标记。
    #[serde(rename = "ReadOnlyForceRecursive")]
    read_only_force_recursive: Option<bool>,
    /// Docker 25.0+ 的 `bind-recursive=disabled` 标记。
    #[serde(rename = "ReadOnlyNonRecursive")]
    read_only_non_recursive: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct InspectVolumeOptions {
    #[serde(rename = "Subpath")]
    subpath: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InspectVolume {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

impl From<InspectContainer> for ManagedContainer {
    fn from(value: InspectContainer) -> Self {
        let labels = labels_of(&value.config);
        let agent_id = labels.and_then(|labels| labels.get(AGENT_LABEL_KEY).cloned());
        let project_id = labels.and_then(|labels| labels.get(PROJECT_LABEL_KEY).cloned());
        let sidecar = labels
            .and_then(|labels| labels.get(SIDECAR_LABEL_KEY))
            .is_some_and(|value| value == "true");
        let sidecar_kind = labels.and_then(|labels| labels.get(SIDECAR_KIND_LABEL_KEY).cloned());
        let generation = labels
            .and_then(|labels| labels.get(GENERATION_LABEL_KEY))
            .and_then(|value| Uuid::parse_str(value).ok());
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
            generation,
        }
    }
}

impl From<InspectContainer> for ContainerEnvironmentFacts {
    fn from(value: InspectContainer) -> Self {
        let labels = labels_of(&value.config);
        let agent_id = labels.and_then(|labels| labels.get(AGENT_LABEL_KEY).cloned());
        let project_id = labels.and_then(|labels| labels.get(PROJECT_LABEL_KEY).cloned());
        let generation = labels
            .and_then(|labels| labels.get(GENERATION_LABEL_KEY))
            .and_then(|value| Uuid::parse_str(value).ok());
        let sidecar = labels
            .and_then(|labels| labels.get(SIDECAR_LABEL_KEY))
            .is_some_and(|value| value == "true");
        let sidecar_kind = labels.and_then(|labels| labels.get(SIDECAR_KIND_LABEL_KEY).cloned());
        let image_reference = value
            .config
            .as_ref()
            .and_then(|config| config.image.clone())
            .unwrap_or_default();
        let state = value
            .state
            .as_ref()
            .and_then(|state| state.status.clone())
            .unwrap_or_default();
        let running = value
            .state
            .as_ref()
            .and_then(|state| state.running)
            .unwrap_or(false);
        let host_config = value.host_config.unwrap_or_default();
        let mounts = mounts_from_inspect(
            host_config.mounts.unwrap_or_default(),
            host_config.binds.unwrap_or_default(),
            value.mounts.unwrap_or_default(),
        );

        Self {
            id: value.id,
            name: value.name.trim_start_matches('/').to_string(),
            state,
            running,
            image_reference,
            image_id: value.image_id.unwrap_or_default(),
            agent_id,
            project_id,
            generation,
            sidecar,
            sidecar_kind,
            mounts,
            host: ContainerHostFacts {
                network_mode: host_config.network_mode,
                privileged: host_config.privileged.unwrap_or(false),
                cap_add: host_config.cap_add.unwrap_or_default(),
                cap_drop: host_config.cap_drop.unwrap_or_default(),
                memory_bytes: host_config.memory,
                nano_cpus: host_config.nano_cpus,
                pids_limit: host_config.pids_limit,
                security_opt: host_config.security_opt.unwrap_or_default(),
            },
        }
    }
}

impl From<InspectContainer> for ContainerVerificationFacts {
    fn from(value: InspectContainer) -> Self {
        let sidecar_source_container_id = labels_of(&value.config)
            .and_then(|labels| labels.get(SIDECAR_SOURCE_LABEL_KEY).cloned())
            .map(|source| source.trim().to_string())
            .filter(|source| !source.is_empty());
        let created = value
            .created
            .as_deref()
            .map(str::trim)
            .filter(|created| !created.is_empty())
            .map(ToOwned::to_owned);
        let environment = ContainerEnvironmentFacts::from(value);
        Self {
            environment,
            created,
            sidecar_source_container_id,
        }
    }
}

impl From<InspectVolume> for ManagedVolume {
    fn from(value: InspectVolume) -> Self {
        let labels = value.labels.as_ref();
        Self {
            name: value.name,
            kind: labels.and_then(|labels| labels.get(KIND_LABEL_KEY).cloned()),
            project_id: labels.and_then(|labels| labels.get(PROJECT_LABEL_KEY).cloned()),
            agent_id: labels.and_then(|labels| labels.get(AGENT_LABEL_KEY).cloned()),
            role: labels.and_then(|labels| labels.get(ROLE_LABEL_KEY).cloned()),
        }
    }
}

fn labels_of(config: &Option<InspectConfig>) -> Option<&HashMap<String, String>> {
    config.as_ref().and_then(|config| config.labels.as_ref())
}

/// 合并三类来源，得到“实际生效”的挂载集合：
/// 1. `HostConfig.Mounts`：创建时的显式规格（含 volume subpath 与 bind 递归选项）；
/// 2. `HostConfig.Binds`：`-v` 形式的绑定（工作区命名卷属于这一类）；
/// 3. 顶层 `Mounts`：真实生效的挂载，包含镜像声明的匿名卷，并覆盖 RW 事实。
fn mounts_from_inspect(
    specs: Vec<InspectSpecMount>,
    binds: Vec<String>,
    realized: Vec<InspectRealizedMount>,
) -> Vec<ContainerMountFacts> {
    let mut mounts: BTreeMap<String, ContainerMountFacts> = BTreeMap::new();

    for spec in specs {
        let Some(target) = spec.target else {
            continue;
        };
        let mount_type = spec
            .mount_type
            .as_deref()
            .map(ContainerMountType::from_docker_value)
            .unwrap_or(ContainerMountType::Bind);
        let source = spec.source.unwrap_or_default();
        let volume_name = (mount_type == ContainerMountType::Volume).then(|| source.clone());
        let recursive_read_only = spec.bind_options.as_ref().and_then(|options| {
            if options.read_only_force_recursive == Some(true) {
                Some(true)
            } else if options.read_only_non_recursive == Some(true) {
                Some(false)
            } else {
                None
            }
        });
        mounts.insert(
            target.clone(),
            ContainerMountFacts {
                target,
                mount_type,
                source,
                read_only: spec.read_only.unwrap_or(false),
                propagation: spec
                    .bind_options
                    .and_then(|options| options.propagation)
                    .map(|value| value.to_string())
                    .filter(|value| !value.is_empty()),
                recursive_read_only,
                volume_name,
                volume_subpath: spec
                    .volume_options
                    .and_then(|options| options.subpath)
                    .filter(|value| !value.is_empty()),
            },
        );
    }

    for bind in binds {
        let Some((source, target, read_only)) = parse_bind_spec(&bind) else {
            continue;
        };
        mounts.entry(target.clone()).or_insert(ContainerMountFacts {
            target,
            volume_name: (!source.starts_with('/')).then(|| source.clone()),
            mount_type: if source.starts_with('/') {
                ContainerMountType::Bind
            } else {
                ContainerMountType::Volume
            },
            source,
            read_only,
            propagation: None,
            recursive_read_only: None,
            volume_subpath: None,
        });
    }

    for mount in realized {
        let Some(target) = mount.destination else {
            continue;
        };
        let mount_type = mount
            .mount_type
            .as_deref()
            .map(ContainerMountType::from_docker_value)
            .unwrap_or(ContainerMountType::Volume);
        let is_volume = mount_type == ContainerMountType::Volume;
        let read_only = !mount.read_write.unwrap_or(false);
        let name = mount.name.clone().filter(|value| !value.is_empty());
        let propagation = mount.propagation.clone().filter(|value| !value.is_empty());

        if let Some(existing) = mounts.get_mut(&target) {
            // 真实生效的状态覆盖创建规格中的声明。
            existing.read_only = read_only;
            if is_volume {
                if let Some(name) = name {
                    existing.volume_name = Some(name);
                }
            } else if let Some(propagation) = propagation {
                existing.propagation = Some(propagation);
            }
            continue;
        }

        // 例如镜像声明的匿名卷：只出现在真实挂载里，必须保留其真实名称。
        let source = if is_volume {
            name.clone()
                .unwrap_or_else(|| mount.source.clone().unwrap_or_default())
        } else {
            mount.source.clone().unwrap_or_default()
        };
        mounts.insert(
            target.clone(),
            ContainerMountFacts {
                target,
                mount_type,
                source,
                read_only,
                propagation,
                recursive_read_only: None,
                volume_name: if is_volume { name } else { None },
                volume_subpath: None,
            },
        );
    }

    mounts.into_values().collect()
}

/// 解析 `-v` 形式的 `source:target[:mode]`。
fn parse_bind_spec(value: &str) -> Option<(String, String, bool)> {
    let mut parts = value.split(':');
    let source = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mode = parts.next();
    if source.is_empty() || target.is_empty() || parts.next().is_some() {
        return None;
    }
    let read_only = mode
        .map(|mode| mode.split(',').any(|flag| flag.trim() == "ro"))
        .unwrap_or(false);
    Some((source, target, read_only))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

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
        assert_eq!(containers[0].generation, None);
        assert!(containers[0].sidecar);
        assert_eq!(containers[0].sidecar_kind.as_deref(), Some("project"));
        assert_eq!(containers[1].agent_id, None);
        assert!(!containers[1].sidecar);
    }

    #[test]
    fn parses_managed_volumes_from_inspect_json() {
        let volumes = managed_volumes_from_inspect(
            r#"
            [
                {
                    "Name": "mai-team-project-project-1-agent-agent-1",
                    "Labels": {
                        "mai.team.managed": "true",
                        "mai.team.kind": "agent-workspace",
                        "mai.team.project": "project-1",
                        "mai.team.agent": "agent-1",
                        "mai.team.role": "reviewer"
                    }
                },
                {
                    "Name": "mai-team-project-project-1-cache",
                    "Labels": {
                        "mai.team.managed": "true",
                        "mai.team.kind": "project-cache",
                        "mai.team.project": "project-1"
                    }
                }
            ]
            "#,
        )
        .expect("parse volumes");

        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].name, "mai-team-project-project-1-agent-agent-1");
        assert_eq!(volumes[0].kind.as_deref(), Some("agent-workspace"));
        assert_eq!(volumes[0].project_id.as_deref(), Some("project-1"));
        assert_eq!(volumes[0].agent_id.as_deref(), Some("agent-1"));
        assert_eq!(volumes[0].role.as_deref(), Some("reviewer"));
        assert_eq!(volumes[1].kind.as_deref(), Some("project-cache"));
        assert_eq!(volumes[1].agent_id, None);
    }

    #[test]
    fn parses_environment_facts_with_mounts_and_host_config() {
        let facts = container_environment_facts_from_inspect(
            r#"
            [
                {
                    "Id": "abc123",
                    "Name": "/mai-team-agent-1-g8f058410c69e",
                    "Image": "sha256:effective",
                    "Config": {
                        "Image": "ghcr.io/example/agent:latest",
                        "Labels": {
                            "mai.team.managed": "true",
                            "mai.team.agent": "agent-1",
                            "mai.team.generation": "8f058410-c69e-45d6-a10d-d204db72b30a"
                        }
                    },
                    "State": { "Status": "running", "Running": true },
                    "Mounts": [
                        {
                            "Type": "bind",
                            "Source": "/srv/mai-references/linux/repo",
                            "Destination": "/references/linux",
                            "RW": false
                        },
                        {
                            "Type": "volume",
                            "Name": "anonymous-data",
                            "Source": "/var/lib/docker/volumes/anonymous-data/_data",
                            "Destination": "/var/lib/postgresql/data",
                            "RW": true
                        }
                    ],
                    "HostConfig": {
                        "Binds": ["mai-team-workspace-agent-1:/workspace"],
                        "NetworkMode": "host",
                        "Memory": 8589934592,
                        "NanoCpus": 4000000000,
                        "PidsLimit": 2048,
                        "Mounts": [
                            {
                                "Type": "bind",
                                "Source": "/srv/mai-references/linux/repo",
                                "Target": "/references/linux",
                                "ReadOnly": true,
                                "BindOptions": {
                                    "Propagation": "rprivate",
                                    "ReadOnlyForceRecursive": true
                                }
                            }
                        ]
                    }
                }
            ]
            "#,
        )
        .expect("parse environment facts");

        assert_eq!(facts.len(), 1);
        let facts = &facts[0];
        assert_eq!(facts.image_reference, "ghcr.io/example/agent:latest");
        assert_eq!(facts.image_id, "sha256:effective");
        assert!(facts.running);
        assert_eq!(
            facts.generation.map(|value| value.to_string()),
            Some("8f058410-c69e-45d6-a10d-d204db72b30a".to_string())
        );
        assert_eq!(facts.host.memory_bytes, Some(8589934592));
        assert_eq!(facts.host.nano_cpus, Some(4000000000));
        assert_eq!(facts.host.pids_limit, Some(2048));

        let reference = facts
            .mount_at("/references/linux")
            .expect("reference mount");
        assert_eq!(reference.mount_type, ContainerMountType::Bind);
        assert_eq!(reference.source, "/srv/mai-references/linux/repo");
        assert!(reference.read_only);
        assert_eq!(reference.propagation.as_deref(), Some("rprivate"));
        assert_eq!(reference.recursive_read_only, Some(true));

        let workspace = facts.mount_at("/workspace").expect("workspace mount");
        assert_eq!(workspace.mount_type, ContainerMountType::Volume);
        assert_eq!(
            workspace.volume_name.as_deref(),
            Some("mai-team-workspace-agent-1")
        );
        assert!(!workspace.read_only);

        let anonymous = facts
            .mount_at("/var/lib/postgresql/data")
            .expect("anonymous volume");
        assert_eq!(anonymous.volume_name.as_deref(), Some("anonymous-data"));
        assert!(!anonymous.read_only);
    }

    #[test]
    fn realized_mount_read_write_state_overrides_spec() {
        let facts = container_environment_facts_from_inspect(
            r#"
            [
                {
                    "Id": "abc123",
                    "Name": "/mai-team-agent-1",
                    "Image": "sha256:effective",
                    "Config": { "Image": "ubuntu:latest", "Labels": {} },
                    "State": { "Status": "running" },
                    "Mounts": [
                        {
                            "Type": "bind",
                            "Source": "/srv/mai-references/linux/repo",
                            "Destination": "/references/linux",
                            "RW": true
                        }
                    ],
                    "HostConfig": {
                        "Mounts": [
                            {
                                "Type": "bind",
                                "Source": "/srv/mai-references/linux/repo",
                                "Target": "/references/linux",
                                "ReadOnly": true
                            }
                        ]
                    }
                }
            ]
            "#,
        )
        .expect("parse environment facts");

        let reference = facts[0]
            .mount_at("/references/linux")
            .expect("reference mount");
        assert!(
            !reference.read_only,
            "realized RW=true 必须覆盖创建规格中的 readonly 声明"
        );
    }

    #[test]
    fn parses_verification_facts_with_source_label_and_creation_time() {
        let facts = container_verification_facts_from_inspect(
            r#"
            [
                {
                    "Id": "sidecar-1",
                    "Name": "/mai-team-agent-mcp-agent-1-g8f058410c69e",
                    "Created": "2026-10-15T07:23:45.123456789Z",
                    "Image": "sha256:sidecar",
                    "Config": {
                        "Image": "ghcr.io/zr233/mai-team-sidecar:latest",
                        "Labels": {
                            "mai.team.managed": "true",
                            "mai.team.agent": "agent-1",
                            "mai.team.sidecar": "true",
                            "mai.team.sidecar.kind": "agent-mcp",
                            "mai.team.sidecar.source": "agent-container-1"
                        }
                    },
                    "State": { "Status": "running", "Running": true }
                },
                {
                    "Id": "agent-1",
                    "Name": "/mai-team-agent-1-g8f058410c69e",
                    "Created": "2026-10-15T07:23:40Z",
                    "Image": "sha256:effective",
                    "Config": { "Image": "ghcr.io/example/agent:latest", "Labels": {} },
                    "State": { "Status": "running", "Running": true }
                }
            ]
            "#,
        )
        .expect("parse verification facts");

        assert_eq!(facts.len(), 2);
        assert_eq!(
            facts[0].sidecar_source_container_id.as_deref(),
            Some("agent-container-1")
        );
        assert_eq!(
            facts[0].created.as_deref(),
            Some("2026-10-15T07:23:45.123456789Z")
        );
        assert_eq!(facts[1].sidecar_source_container_id, None);
        assert_eq!(facts[0].id(), "sidecar-1");
        assert!(facts[0].environment.sidecar);
        assert_eq!(
            facts[0].environment.sidecar_kind.as_deref(),
            Some("agent-mcp")
        );

        assert_eq!(facts[0].created_after(&facts[1]), Some(true));
        assert_eq!(facts[1].created_after(&facts[0]), Some(false));
    }

    #[test]
    fn creation_time_comparison_needs_a_known_shape() {
        assert_eq!(
            normalize_docker_timestamp("2026-10-15T07:23:45Z").as_deref(),
            Some("2026-10-15T07:23:45.000000000Z")
        );
        assert_eq!(
            normalize_docker_timestamp("2026-10-15T07:23:45.5Z").as_deref(),
            Some("2026-10-15T07:23:45.500000000Z")
        );
        // 整秒必须排在带小数秒之后，而不是因为字典序被误判成更晚。
        assert!(
            normalize_docker_timestamp("2026-10-15T07:23:45Z").expect("whole second")
                < normalize_docker_timestamp("2026-10-15T07:23:45.1Z").expect("fraction")
        );
        // 不认识的形状必须返回 None，让调用方按未核验处理。
        assert_eq!(
            normalize_docker_timestamp("2026-10-15T07:23:45+08:00"),
            None
        );
        assert_eq!(normalize_docker_timestamp("yesterday"), None);
        assert_eq!(
            normalize_docker_timestamp("2026-10-15T07:23:45.1234567890Z"),
            None
        );
    }
}
